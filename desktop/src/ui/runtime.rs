//! Tokio-side runtime: connects to WhatsApp, processes events, handles commands.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_channel::Sender;
use tokio::sync::mpsc::UnboundedReceiver;

use wacore::download::Downloadable;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::proto_helpers::MessageExt;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events::{ContactUpdate, Event};
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::types::presence::{ChatPresence, ReceiptType};
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::{Client, Jid, RevokeType, TokioRuntime};
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::bridge::{ChatSummary, IncomingMessage, ReceiptStatus, WaCommand, WaEvent};

// ── Persistence (bincode binary format) ──────────────────────────────────────

const CHATS_FILE: &str = "wa_chats.bin";
const CONTACTS_FILE: &str = "wa_contacts.bin";
const LID_PHONE_FILE: &str = "wa_lid_phone.bin";
const MESSAGES_DIR: &str = "wa_messages";

// Magic header for versioned binary files: "WA01"
// Bumped from '01' to '02' after adding is_edited + is_system_message to IncomingMessage.
// Old bincode files with header '01' will fail to deserialize and be re-created.
const BIN_HEADER: [u8; 4] = [0x57, 0x41, b'0', b'2'];

// Legacy JSON filenames for auto-migration
const CHATS_FILE_JSON: &str = "wa_chats.json";
const CONTACTS_FILE_JSON: &str = "wa_contacts.json";
const LID_PHONE_FILE_JSON: &str = "wa_lid_phone.json";

/// Load the Tenor API key from env or local file (never hardcoded in source).
fn tenor_api_key() -> String {
    std::env::var("TENOR_API_KEY")
        .or_else(|_| std::fs::read_to_string("tenor_key.txt").map(|s| s.trim().to_string()))
        .or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::fs::read_to_string(
                std::path::Path::new(&home).join(".config/whatsapp-desktop/tenor_key"),
            )
            .map(|s| s.trim().to_string())
        })
        .unwrap_or_default()
}

fn messages_dir() -> PathBuf {
    PathBuf::from(MESSAGES_DIR)
}

fn messages_file(chat_id: &str) -> PathBuf {
    let safe = chat_id.replace(['/', '\\', '@', ':'], "_");
    messages_dir().join(format!("{safe}.bin"))
}

/// Read a bincode file with version header. Returns None on any failure.
fn read_bin<T: serde::de::DeserializeOwned>(path: &str) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 4 || data[..4] != BIN_HEADER {
        return None;
    }
    bincode::deserialize(&data[4..]).ok()
}

/// Write a bincode file with version header.
fn write_bin<T: serde::Serialize>(path: &str, value: &T) {
    if let Ok(payload) = bincode::serialize(value) {
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&BIN_HEADER);
        data.extend_from_slice(&payload);
        let _ = std::fs::write(path, data);
    }
}

/// Read a bincode file for messages (uses PathBuf).
fn read_bin_path<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 4 || data[..4] != BIN_HEADER {
        return None;
    }
    bincode::deserialize(&data[4..]).ok()
}

fn write_bin_path<T: serde::Serialize>(path: &PathBuf, value: &T) {
    if let Ok(payload) = bincode::serialize(value) {
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&BIN_HEADER);
        data.extend_from_slice(&payload);
        let _ = std::fs::write(path, data);
    }
}

pub fn load_chats() -> Vec<ChatSummary> {
    if let Some(chats) = read_bin::<Vec<ChatSummary>>(CHATS_FILE) {
        return chats;
    }
    // Fallback: try legacy JSON
    let Ok(data) = std::fs::read_to_string(CHATS_FILE_JSON) else {
        return vec![];
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_chats(chats: &[ChatSummary]) {
    write_bin(CHATS_FILE, &chats.to_vec());
}

pub fn load_contact_names() -> HashMap<String, String> {
    if let Some(names) = read_bin::<HashMap<String, String>>(CONTACTS_FILE) {
        return names;
    }
    let Ok(data) = std::fs::read_to_string(CONTACTS_FILE_JSON) else {
        return HashMap::new();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_contact_names(names: &HashMap<String, String>) {
    write_bin(CONTACTS_FILE, names);
}

/// Check if a proposed "name" is a valid display name (not a raw JID).
/// Strict: only rejects empty, name-equals-jid, or name-contains-@.
/// Does NOT reject pure phone numbers like "+16472876066" — those are
/// legitimate display fallbacks for unsaved contacts.
fn is_valid_contact_name(name: &str, jid: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name == jid {
        return false;
    }
    if name.contains('@') {
        return false;
    }
    true
}

/// One-time startup pass: scan every message file on disk and extract any
/// push_names we haven't seen yet. Populates contact_names comprehensively
/// so LIDs that have ever sent a message get display names resolved from
/// the moment the app starts — before any chat is opened.
///
/// The poison-purge (removing name == JID entries) is guarded behind a
/// marker file so it runs ONCE per user — previously it ran every startup
/// which could wipe legitimate names if validation was too strict.
///
/// Never REMOVES entries on subsequent runs — only ADDS new push_names.
pub fn rebuild_contact_names_from_history() -> HashMap<String, String> {
    let mut names = load_contact_names();

    // One-time poison purge (gated by marker — runs once per user only)
    let purge_marker = std::path::PathBuf::from(".contact_purge_v1");
    let mut purged = 0;
    if !purge_marker.exists() {
        let before = names.len();
        // Strict purge: only remove entries where name exactly equals the JID
        // (true poison from push_name storing the raw JID as a fallback).
        // Do NOT use is_valid_contact_name here — that's too aggressive and
        // would wipe legitimately saved names like "+16472876066".
        names.retain(|jid, name| name != jid);
        purged = before - names.len();
        if purged > 0 {
            log::info!("One-time purge: removed {purged} entries where name == JID");
        }
        let _ = std::fs::write(&purge_marker, "done");
    }

    let dir = messages_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        save_contact_names(&names);
        return names;
    };
    let mut learned = 0u32;
    let mut scanned = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if ext != "bin" {
            continue;
        }
        scanned += 1;
        let Some(msgs) = read_bin_path::<Vec<IncomingMessage>>(&path) else {
            continue;
        };
        for m in msgs {
            if m.is_from_me || m.sender_id.is_empty() {
                continue;
            }
            // Skip messages where sender_name looks like a JID (old poison)
            if m.sender_name.is_empty() || m.sender_name == m.sender_id {
                continue;
            }
            // Don't overwrite — first-seen push_name wins.
            if !names.contains_key(&m.sender_id) {
                names.insert(m.sender_id.clone(), m.sender_name.clone());
                learned += 1;
            }
        }
    }
    log::info!(
        "Scanned {scanned} message files, learned {learned} push_names, purged {purged} poison entries"
    );
    save_contact_names(&names);
    names
}

pub fn load_lid_phone_map() -> HashMap<String, String> {
    if let Some(map) = read_bin::<HashMap<String, String>>(LID_PHONE_FILE) {
        return map;
    }
    let Ok(data) = std::fs::read_to_string(LID_PHONE_FILE_JSON) else {
        return HashMap::new();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_lid_phone_map(map: &HashMap<String, String>) {
    write_bin(LID_PHONE_FILE, map);
}

/// Old bincode header (pre-is_system_message). Used for fallback deserialization.
const BIN_HEADER_V1: [u8; 4] = [0x57, 0x41, b'0', b'1'];

pub fn load_messages(chat_id: &str) -> Vec<IncomingMessage> {
    let bin_path = messages_file(chat_id);

    // Try current version first
    if let Some(msgs) = read_bin_path::<Vec<IncomingMessage>>(&bin_path) {
        return msgs;
    }

    // Fallback: try old bincode format (v1) — the new fields have #[serde(default)]
    // which works for JSON but not bincode. However, if the data happens to deserialize
    // (e.g. trailing bytes ignored), we can recover it. Otherwise, fall through to JSON.
    if let Ok(data) = std::fs::read(&bin_path) {
        if data.len() >= 4 && data[..4] == BIN_HEADER_V1 {
            // Try deserializing — bincode may or may not handle the missing fields.
            // If it fails, delete the stale file so JSON fallback or re-sync can work.
            if let Ok(msgs) = bincode::deserialize::<Vec<IncomingMessage>>(&data[4..]) {
                // Re-save in new format
                save_messages(chat_id, &msgs);
                return msgs;
            } else {
                // Old format can't be read — delete so we don't keep failing
                let _ = std::fs::remove_file(&bin_path);
                log::info!("Deleted incompatible v1 bincode for {chat_id}, falling back to JSON");
            }
        }
    }

    // Fallback: try legacy JSON
    let safe = chat_id.replace(['/', '\\', '@', ':'], "_");
    let json_path = messages_dir().join(format!("{safe}.json"));
    let Ok(data) = std::fs::read_to_string(&json_path) else {
        return vec![];
    };
    let msgs: Vec<IncomingMessage> = serde_json::from_str(&data).unwrap_or_default();
    // Re-save as new bincode for future loads
    if !msgs.is_empty() {
        save_messages(chat_id, &msgs);
    }
    msgs
}

fn save_messages(chat_id: &str, messages: &[IncomingMessage]) {
    let dir = messages_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    write_bin_path(&messages_file(chat_id), &messages.to_vec());
}

/// Append a single message to an existing chat's message file.
/// Loads existing messages, appends (deduplicating by ID), and saves.
fn save_messages_append(chat_id: &str, msg: &IncomingMessage) {
    let mut messages = load_messages(chat_id);
    if !messages.iter().any(|m| m.id == msg.id) {
        messages.push(msg.clone());
        save_messages(chat_id, &messages);
    }
}

/// One-time migration: convert all JSON files to binary format.
/// Called at startup before RuntimeState is created.
fn migrate_json_to_bincode() {
    // Skip if already migrated (binary chats file exists)
    if std::path::Path::new(CHATS_FILE).exists() {
        return;
    }
    // Skip if no JSON files exist (fresh install)
    if !std::path::Path::new(CHATS_FILE_JSON).exists() {
        return;
    }

    log::info!("MIGRATION: Converting JSON files to binary format...");

    // Migrate chats
    if let Ok(data) = std::fs::read_to_string(CHATS_FILE_JSON) {
        if let Ok(chats) = serde_json::from_str::<Vec<ChatSummary>>(&data) {
            write_bin(CHATS_FILE, &chats);
            log::info!("  Migrated {} chats", chats.len());
        }
    }

    // Migrate contacts
    if let Ok(data) = std::fs::read_to_string(CONTACTS_FILE_JSON) {
        if let Ok(names) = serde_json::from_str::<HashMap<String, String>>(&data) {
            write_bin(CONTACTS_FILE, &names);
            log::info!("  Migrated {} contacts", names.len());
        }
    }

    // Migrate LID→phone map
    if let Ok(data) = std::fs::read_to_string(LID_PHONE_FILE_JSON) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&data) {
            write_bin(LID_PHONE_FILE, &map);
            log::info!("  Migrated {} LID→phone mappings", map.len());
        }
    }

    // Migrate message files
    let dir = messages_dir();
    if dir.exists() {
        let mut count = 0u32;
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(msgs) = serde_json::from_str::<Vec<IncomingMessage>>(&data) {
                        let bin_path = path.with_extension("bin");
                        write_bin_path(&bin_path, &msgs);
                        count += 1;
                    }
                }
            }
        }
        log::info!("  Migrated {} message files", count);
    }

    log::info!("MIGRATION: Complete. JSON files kept as backup.");
}

/// Upsert a chat into the persisted list and return true if it was new/changed.
/// Format a JID string into a human-readable fallback name.
/// `14155551234@s.whatsapp.net` → `+14155551234`
/// `12345.6789@lid` → `+12345` (LID — not a real phone, but better than raw JID)
/// Groups and anything else → returned as-is.
pub fn display_name_from_jid(jid: &str) -> String {
    if let Some(user) = jid.strip_suffix("@s.whatsapp.net") {
        // Strip device suffix (e.g., "12345:5" → "12345")
        let user = user.split(':').next().unwrap_or(user);
        if user.chars().all(|c| c.is_ascii_digit()) {
            return format!("+{user}");
        }
    }
    // LID JIDs look like "12345678.0:90@lid" or "12345:8@lid" — extract digits
    // before the first non-digit (., :) and format as a phone-ish number.
    if let Some(user) = jid.strip_suffix("@lid") {
        let digits: String = user
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.len() >= 5 {
            return format!("+{digits}");
        }
    }
    jid.to_string()
}

fn persist_chat(state: &Arc<Mutex<RuntimeState>>, summary: ChatSummary) {
    state.lock().unwrap().upsert_chat(summary);
}

/// Create a system/notification message (centered gray text, no bubble).
/// Used for group events like "You added Alice", "Bob left", etc.
fn make_system_message(chat_id: &str, text: &str) -> crate::bridge::IncomingMessage {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let id = format!("sys_{ts}_{}", rand_u32());
    let mut msg = crate::bridge::IncomingMessage::outgoing(
        id,
        chat_id.to_string(),
        Some(text.to_string()),
        ts,
    );
    msg.is_from_me = false;
    msg.is_system_message = true;
    msg
}

/// Simple random u32 for unique IDs (no external crate needed).
fn rand_u32() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    h.finish() as u32
}

/// Resolve a sender JID to a display name using all available name sources.
/// Tries: contact_names (by JID), contact_names (by phone JID variant),
/// chat_names (the JID might be a DM chat ID), formatted phone number fallback.
fn resolve_sender_name(s: &RuntimeState, sender_jid: &str) -> String {
    // 1. Direct lookup in contact_names
    if let Some(name) = s.contact_names.get(sender_jid) {
        return name.clone();
    }
    // 2. LID→phone resolution: if sender is @lid, resolve to phone JID and look up
    if sender_jid.ends_with("@lid") {
        if let Some(phone_jid) = s.lid_to_phone.get(sender_jid) {
            if let Some(name) = s.contact_names.get(phone_jid) {
                return name.clone();
            }
            // Even without a contact name, format the phone number nicely
            return display_name_from_jid(phone_jid);
        }
    }
    // 3. Try chat_names (DM chat entries often have resolved names)
    if let Some(name) = s.chat_names.get(sender_jid) {
        if !name.contains('@') && !name.contains("@lid") && !name.is_empty() {
            return name.clone();
        }
    }
    // 4. Reverse lookup: if sender is @s.whatsapp.net, find LID via reverse map (O(1))
    if sender_jid.ends_with("@s.whatsapp.net") {
        if let Some(lid) = s.phone_to_lid.get(sender_jid) {
            if let Some(name) = s.contact_names.get(lid.as_str()) {
                return name.clone();
            }
        }
    }
    // 5. Check contact_names by user part (works when JID domain differs)
    if let Some(user) = sender_jid.split('@').next() {
        // Try common variants directly (O(1)) before falling back to scan
        let lid_key = format!("{user}@lid");
        let phone_key = format!("{user}@s.whatsapp.net");
        if let Some(name) = s
            .contact_names
            .get(&lid_key)
            .or_else(|| s.contact_names.get(&phone_key))
        {
            return name.clone();
        }
    }
    // 6. For LID JIDs with no resolution, try to at least show the phone number
    if sender_jid.ends_with("@lid") {
        if let Some(phone) = s.lid_to_phone.get(sender_jid) {
            return display_name_from_jid(phone);
        }
    }
    // 7. Formatted phone number fallback
    display_name_from_jid(sender_jid)
}

/// Build a display name for a group from message senders.
/// Checks in-memory cache first, falls back to disk.
/// Returns something like "Karim Valji, Lorne, …" — similar to how WhatsApp shows unnamed groups.
fn group_name_from_history(
    history: &HashMap<String, Vec<crate::bridge::IncomingMessage>>,
    chat_id: &str,
) -> Option<String> {
    // Try in-memory cache first
    if let Some(msgs) = history.get(chat_id) {
        if let Some(name) = names_from_messages(msgs) {
            return Some(name);
        }
    }
    // Fall back to disk
    let disk_msgs = load_messages(chat_id);
    names_from_messages(&disk_msgs)
}

fn names_from_messages(msgs: &[crate::bridge::IncomingMessage]) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    for m in msgs {
        if !m.is_from_me
            && !m.sender_name.is_empty()
            && !m.sender_name.contains('@')
            && !names.contains(&m.sender_name.as_str())
        {
            names.push(&m.sender_name);
            if names.len() >= 3 {
                break;
            }
        }
    }
    if names.is_empty() {
        return None;
    }
    if names.len() >= 3 {
        Some(format!("{}, {}, …", names[0], names[1]))
    } else {
        Some(names.join(", "))
    }
}

/// Replace @JID mentions in message text with @DisplayName.
fn resolve_mentions(text: &str, s: &RuntimeState) -> String {
    let mut result = text.to_string();
    let words: Vec<&str> = text.split_whitespace().collect();
    for word in words {
        if !word.starts_with('@') || word.len() < 4 {
            continue;
        }
        let jid_part = &word[1..]; // strip leading @
        if !jid_part
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            continue;
        }

        // Try all possible JID formats for this number
        let candidates = if jid_part.contains('@') {
            vec![jid_part.to_string()]
        } else {
            vec![
                format!("{jid_part}@lid"), // LID format (most common in mentions)
                format!("{jid_part}@s.whatsapp.net"), // Phone format
            ]
        };
        for full_jid in &candidates {
            let name = resolve_sender_name(s, full_jid);
            if !name.contains('@') && name != *full_jid && name.len() > 1 {
                result = result.replace(word, &format!("@{name}"));
                break;
            }
        }
    }
    result
}

// ── Shared runtime state ──────────────────────────────────────────────────────

struct RuntimeState {
    /// Authoritative in-memory chat list — ALL writes go through upsert_chat/rename_chat.
    chats: Vec<ChatSummary>,
    /// In-memory message cache for quick access after first load
    history: HashMap<String, Vec<IncomingMessage>>,
    /// Tracks access order for LRU eviction of the history cache.
    /// Most recently accessed chat_id is at the back.
    history_lru: Vec<String>,
    /// Last seen message ID per chat for mark-as-read
    last_msg_id: HashMap<String, String>,
    /// Last message sender per chat (needed for group mark-as-read)
    last_msg_sender: HashMap<String, String>,
    /// Chat display names (kept in sync with chats[].name)
    chat_names: HashMap<String, String>,
    /// LID JID → phone JID mapping for resolving LID-addressed group messages
    lid_to_phone: HashMap<String, String>,
    /// Reverse map: phone JID → LID JID (for O(1) reverse lookups in name resolution)
    phone_to_lid: HashMap<String, String>,
    /// Names from app-state ContactUpdate (phonebook sync) — highest-priority name source.
    /// Separate from chat_names so we never mistake a saved phone-number for a real name.
    contact_names: HashMap<String, String>,
    /// Channel for async disk flusher — send a snapshot whenever chats change.
    /// Using watch so rapid updates coalesce: only the latest version hits disk.
    save_tx: std::sync::mpsc::Sender<Vec<ChatSummary>>,
    /// Channel for message disk writes — never block the async runtime.
    /// Sends (chat_id, messages_clone) to a background thread.
    msg_save_tx: std::sync::mpsc::Sender<(String, Vec<IncomingMessage>)>,
    /// Own JIDs for identifying self-reads in group receipts
    own_phone: String,
    own_lid: String,
    connect_count: u32,
}

impl RuntimeState {
    fn new(
        save_tx: std::sync::mpsc::Sender<Vec<ChatSummary>>,
        msg_save_tx: std::sync::mpsc::Sender<(String, Vec<IncomingMessage>)>,
    ) -> Self {
        let chats = load_chats();
        let chat_names: HashMap<String, String> = chats
            .iter()
            .map(|c| (c.id.clone(), c.name.clone()))
            .collect();
        let contact_names = load_contact_names();
        let lid_to_phone = load_lid_phone_map();
        let phone_to_lid: HashMap<String, String> = lid_to_phone
            .iter()
            .map(|(lid, phone)| (phone.clone(), lid.clone()))
            .collect();
        log::info!(
            "Loaded {} contact names, {} LID→phone mappings from disk",
            contact_names.len(),
            lid_to_phone.len()
        );

        Self {
            chats,
            history: HashMap::new(),
            history_lru: Vec::new(),
            last_msg_id: HashMap::new(),
            last_msg_sender: HashMap::new(),
            chat_names,
            lid_to_phone,
            phone_to_lid,
            contact_names,
            save_tx,
            msg_save_tx,
            own_phone: String::new(),
            own_lid: String::new(),
            connect_count: 0,
        }
    }

    /// Rebuild the phone→LID reverse map from lid_to_phone.
    /// Call after bulk lid_to_phone modifications.
    fn rebuild_phone_to_lid(&mut self) {
        self.phone_to_lid = self
            .lid_to_phone
            .iter()
            .map(|(lid, phone)| (phone.clone(), lid.clone()))
            .collect();
    }

    /// Mark a chat as recently accessed in the LRU tracker.
    /// Call whenever a chat's history is loaded or modified.
    fn touch_history(&mut self, chat_id: &str) {
        self.history_lru.retain(|id| id != chat_id);
        self.history_lru.push(chat_id.to_string());
    }

    /// Evict least-recently-used chat histories to keep memory bounded.
    /// Keeps at most MAX_CACHED chats in memory. Evicted chats are saved
    /// to disk first (via queue_save_messages), then dropped.
    fn evict_old_histories(&mut self) {
        const MAX_CACHED: usize = 25;
        let before = self.history.len();
        while self.history_lru.len() > MAX_CACHED {
            let evict_id = self.history_lru.remove(0);
            // Data is already on disk — just drop from memory
            self.history.remove(&evict_id);
        }
        if before > MAX_CACHED {
            let total_msgs: usize = self.history.values().map(|v| v.len()).sum();
            log::info!(
                "CACHE_EVICT: {} → {} chats ({} msgs in cache)",
                before,
                self.history.len(),
                total_msgs
            );
        }
    }

    /// Queue a message-history write to the background disk thread.
    /// Clones the data — caller can release the Mutex immediately.
    fn queue_save_messages(&self, chat_id: &str) {
        if let Some(msgs) = self.history.get(chat_id) {
            let _ = self.msg_save_tx.send((chat_id.to_string(), msgs.clone()));
        }
    }

    /// Store a contact name under one or more JID keys and flush to disk.
    /// Pass both a LID and phone JID when available so lookups always hit.
    /// Update contact name in memory and return true if changed (caller should
    /// flush contact_names to disk AFTER releasing the lock).
    /// Insert a LID→phone mapping and keep the reverse map in sync.
    fn insert_lid_phone(&mut self, lid: String, phone: String) {
        self.phone_to_lid.insert(phone.clone(), lid.clone());
        self.lid_to_phone.insert(lid, phone);
    }

    fn record_contact_name(&mut self, jid: &str, name: &str, also_jid: Option<&str>) -> bool {
        if !is_valid_contact_name(name, jid) {
            return false;
        }
        let mut changed = false;
        if self.contact_names.get(jid).map(|n| n.as_str()) != Some(name) {
            self.contact_names.insert(jid.to_string(), name.to_string());
            changed = true;
        }
        if let Some(alt) = also_jid {
            if !alt.is_empty()
                && is_valid_contact_name(name, alt)
                && self.contact_names.get(alt).map(|n| n.as_str()) != Some(name)
            {
                self.contact_names.insert(alt.to_string(), name.to_string());
                changed = true;
            }
            // Store LID→phone mapping whenever we have both JIDs
            if !alt.is_empty() {
                if jid.ends_with("@lid") && alt.ends_with("@s.whatsapp.net") {
                    self.insert_lid_phone(jid.to_string(), alt.to_string());
                } else if alt.ends_with("@lid") && jid.ends_with("@s.whatsapp.net") {
                    self.insert_lid_phone(alt.to_string(), jid.to_string());
                }
            }
        }
        if changed {
            self.rename_chat(jid, name);
            if let Some(alt) = also_jid {
                if !alt.is_empty() {
                    self.rename_chat(alt, name);
                }
            }
        }
        changed
    }

    /// Insert or update a chat and queue an async disk flush.
    /// Never overwrites `last_message` or `timestamp` with older data.
    fn upsert_chat(&mut self, mut summary: ChatSummary) {
        // Never persist an empty or raw-JID name — resolve using all available sources
        let looks_raw = summary.name.is_empty()
            || summary.name.contains("@lid")
            || summary.name.contains("@s.whatsapp.net")
            || summary.name.contains("@g.us");
        if looks_raw {
            summary.name = if summary.is_group {
                group_name_from_history(&self.history, &summary.id)
                    .unwrap_or_else(|| resolve_sender_name(self, &summary.id))
            } else {
                resolve_sender_name(self, &summary.id)
            };
        }
        self.chat_names
            .insert(summary.id.clone(), summary.name.clone());
        if let Some(existing) = self.chats.iter_mut().find(|c| c.id == summary.id) {
            // Preserve the newer last_message + timestamp
            let keep_old_preview =
                existing.timestamp > summary.timestamp && !existing.last_message.is_empty();
            let old_msg = existing.last_message.clone();
            let old_ts = existing.timestamp;

            // Preserve unread count if incoming summary has 0 but existing has unread
            let old_unread = existing.unread_count;

            // Preserve user-set flags — these are managed by app state sync
            // (PinUpdate/MuteUpdate/ArchiveUpdate), not by history sync.
            // History sync data (conv.pinned, conv.mute_end_time) may be stale.
            let old_pinned = existing.is_pinned;
            let old_muted = existing.is_muted;
            let old_archived = existing.is_archived;
            let old_favorite = existing.is_favorite;
            let old_label = existing.label.clone();
            let old_pinned_msg = existing.pinned_msg_id.clone();

            if old_pinned != summary.is_pinned {
                log::info!(
                    "UPSERT_CHAT: {} pin conflict: existing={old_pinned} incoming={} → keeping {old_pinned}",
                    summary.id,
                    summary.is_pinned
                );
            }

            // Update name, last_message, timestamp, unread, is_group
            *existing = summary;

            // Restore user-set flags
            existing.is_pinned = old_pinned;
            existing.is_muted = old_muted;
            existing.is_archived = old_archived;
            existing.is_favorite = old_favorite;
            if old_label.is_some() {
                existing.label = old_label;
            }
            if old_pinned_msg.is_some() {
                existing.pinned_msg_id = old_pinned_msg;
            }

            // But restore the preview/timestamp if the old one was newer
            if keep_old_preview {
                existing.last_message = old_msg;
                existing.timestamp = old_ts;
            }
            // Don't reset unread to 0 if the update doesn't carry unread info
            if existing.unread_count == 0 && old_unread > 0 {
                existing.unread_count = old_unread;
            }
        } else {
            self.chats.push(summary);
        }
        self.chats.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let _ = self.save_tx.send(self.chats.clone());
    }

    /// Build a chat list with the best available names applied.
    /// Whenever we send a full `ChatsLoaded` snapshot to the UI we use this
    /// so that phonebook names (from ContactUpdate) always win, even if some
    /// `JoinedGroup` tasks raced ahead and stored a phone number.
    fn chats_with_best_names(&self) -> Vec<ChatSummary> {
        // Build a set of phone JIDs that already have their own chat entry,
        // so we can suppress duplicate @lid chats that map to the same person.
        let phone_chat_ids: std::collections::HashSet<&str> = self
            .chats
            .iter()
            .filter(|c| c.id.ends_with("@s.whatsapp.net"))
            .map(|c| c.id.as_str())
            .collect();

        self.chats
            .iter()
            .filter_map(|c| {
                // Filter out status broadcasts (WhatsApp Stories)
                if c.id == "status@broadcast" || c.id.contains("@broadcast") {
                    return None;
                }
                // Suppress @lid ghost chats when the phone JID chat already exists
                if c.id.ends_with("@lid") {
                    if let Some(phone) = self.lid_to_phone.get(&c.id) {
                        if phone_chat_ids.contains(phone.as_str()) {
                            return None;
                        }
                    }
                }

                // Priority: contact_names by chat_id → contact_names via LID mapping → c.name
                let name = self
                    .contact_names
                    .get(&c.id)
                    .cloned()
                    .or_else(|| {
                        // If chat_id is phone JID, use reverse map to find LID
                        if c.id.ends_with("@s.whatsapp.net") {
                            if let Some(lid) = self.phone_to_lid.get(&c.id) {
                                if let Some(n) = self.contact_names.get(lid) {
                                    return Some(n.clone());
                                }
                            }
                        }
                        // If chat_id is LID, resolve to phone and look up
                        if c.id.ends_with("@lid") {
                            if let Some(phone) = self.lid_to_phone.get(&c.id) {
                                if let Some(n) = self.contact_names.get(phone) {
                                    return Some(n.clone());
                                }
                            }
                        }
                        None
                    })
                    .unwrap_or_else(|| c.name.clone());
                // Never send an empty or raw-JID name to the UI
                let name = if name.is_empty() || name.contains("@lid") || name.contains("@g.us") {
                    if c.is_group {
                        // For groups with unresolved names, build a name from cached
                        // message senders (like WhatsApp does for unnamed groups)
                        group_name_from_history(&self.history, &c.id)
                            .unwrap_or_else(|| resolve_sender_name(self, &c.id))
                    } else {
                        resolve_sender_name(self, &c.id)
                    }
                } else {
                    name
                };
                if name == c.name {
                    Some(c.clone())
                } else {
                    Some(ChatSummary { name, ..c.clone() })
                }
            })
            .collect()
    }

    /// Update only the display name of an existing chat and queue a disk flush.
    fn rename_chat(&mut self, chat_id: &str, name: &str) {
        self.chat_names
            .insert(chat_id.to_string(), name.to_string());
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            if c.name != name {
                c.name = name.to_string();
                let _ = self.save_tx.send(self.chats.clone());
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_wa_runtime(event_tx: Sender<WaEvent>, mut cmd_rx: UnboundedReceiver<WaCommand>) {
    if let Err(e) = run_inner(event_tx.clone(), &mut cmd_rx).await {
        log::error!("WhatsApp runtime error: {e:#}");
        let _ = event_tx.send(WaEvent::Disconnected(e.to_string())).await;
    }
}

async fn run_inner(
    event_tx: Sender<WaEvent>,
    cmd_rx: &mut UnboundedReceiver<WaCommand>,
) -> Result<()> {
    // Start AI autocorrect background thread (reads Gemini API key from env/file)
    crate::ui::autocorrect::start_ai_corrector();

    let backend = Arc::new(SqliteStore::new("whatsapp.db").await?);

    // One-time migration: force re-sync of app-state 'regular' collections to
    // pick up contact-name mutations (both ContactAction and LidContactAction).
    // Marker version bumped whenever we change contact handling so the re-sync
    // runs again with the new code. Current version: v4 (wider name propagation —
    // stores under LID, phone, AND device-suffix-stripped base JIDs).
    {
        let marker = std::path::PathBuf::from(".contact_resync_v4");
        // Clean up old markers to avoid cruft
        let _ = std::fs::remove_file(".contact_resync_v2");
        let _ = std::fs::remove_file(".contact_resync_v3");
        if !marker.exists() {
            log::info!("Running one-time contact re-sync migration (v4)");
            let device_id = backend.device_id();
            for name in &["regular_low", "regular_high", "regular"] {
                if let Err(e) = backend
                    .set_app_state_version_for_device(
                        name,
                        wacore::appstate::hash::HashState::default(),
                        device_id,
                    )
                    .await
                {
                    log::warn!("Failed to reset {name}: {e}");
                }
            }
            log::info!("Reset app_state_versions for regular collections — next connect will pull all contact names from phone");
            let _ = std::fs::write(&marker, "v4");
        }
    }
    let transport_factory = TokioWebSocketTransportFactory::new();
    let http_client = UreqHttpClient::new();

    // Dedicated thread for chat-list disk writes.
    // Uses mpsc (not watch) so pending saves are guaranteed to flush on shutdown —
    // the thread drains the channel before exiting, unlike a Tokio task which is cancelled.
    let (save_tx, chat_save_rx) = std::sync::mpsc::channel::<Vec<ChatSummary>>();
    std::thread::Builder::new()
        .name("chat-disk-writer".into())
        .spawn(move || {
            while let Ok(chats) = chat_save_rx.recv() {
                // Drain any queued updates so we only write the latest
                let mut latest = chats;
                while let Ok(newer) = chat_save_rx.try_recv() {
                    latest = newer;
                }
                save_chats(&latest);
            }
        })
        .expect("Failed to spawn chat-disk-writer thread");

    // Dedicated thread for message-history disk writes.
    // Writes are ordered per-chat (last-writer-wins) and never block the async runtime.
    let (msg_save_tx, msg_save_rx) = std::sync::mpsc::channel::<(String, Vec<IncomingMessage>)>();
    std::thread::Builder::new()
        .name("msg-disk-writer".into())
        .spawn(move || {
            while let Ok((chat_id, messages)) = msg_save_rx.recv() {
                save_messages(&chat_id, &messages);
            }
        })
        .expect("Failed to spawn disk-writer thread");

    // One-time migration from JSON to binary format
    migrate_json_to_bincode();

    // Rebuild contact_names from ALL historical message files before
    // RuntimeState loads contact_names from disk. This captures push_names
    // from every chat's message history so that LIDs resolve to display
    // names from the moment the app starts — no need to open each chat.
    // Rebuild writes to the same disk file that RuntimeState::new() reads.
    let _ = tokio::task::spawn_blocking(rebuild_contact_names_from_history)
        .await;

    let state = Arc::new(Mutex::new(RuntimeState::new(save_tx, msg_save_tx)));

    // ── Centralized LID resolver ──
    // Any handler that encounters an unresolved @lid JID sends it here.
    // The resolver batches requests, deduplicates, and runs usync in bulk.
    let (lid_resolve_tx, mut lid_resolve_rx) =
        tokio::sync::mpsc::unbounded_channel::<String>();

    let tx = event_tx.clone();
    let state_ev = state.clone();
    let lid_tx_ev = lid_resolve_tx.clone();
    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(transport_factory)
        .with_http_client(http_client)
        .with_runtime(TokioRuntime)
        .on_event(move |event, client| {
            let tx = tx.clone();
            let state = state_ev.clone();
            let lid_tx = lid_tx_ev.clone();
            async move {
                handle_wa_event(&tx, &state, &client, &lid_tx, event).await;
            }
        })
        .build()
        .await?;

    let client = bot.client();
    let mut bot_handle = bot.run().await?;

    // Spawn the background LID resolver task
    {
        let client_r = client.clone();
        let state_r = state.clone();
        let tx_r = event_tx.clone();
        tokio::spawn(async move {
            // Collect LIDs and resolve every 2s (or when queue is large)
            let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                tokio::select! {
                    Some(lid) = lid_resolve_rx.recv() => {
                        pending.insert(lid);
                        // If we have enough, resolve immediately
                        if pending.len() >= 10 {
                            resolve_lid_batch(&client_r, &state_r, &tx_r, &mut pending).await;
                        }
                    }
                    _ = interval.tick() => {
                        if !pending.is_empty() {
                            resolve_lid_batch(&client_r, &state_r, &tx_r, &mut pending).await;
                        }
                    }
                }
            }
        });
    }

    // Spawn suspend/resume detector — watches for wall-clock drift that indicates
    // the system was sleeping. On wake, forces a disconnect+reconnect to re-sync.
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let check_interval = std::time::Duration::from_secs(5);
        loop {
            let before = std::time::Instant::now();
            tokio::time::sleep(check_interval).await;
            let elapsed = before.elapsed();
            // If a 5s sleep took >15s, the system was likely suspended
            if elapsed > std::time::Duration::from_secs(15) {
                log::info!(
                    "Suspend/resume detected: {}s sleep took {}s",
                    check_interval.as_secs(),
                    elapsed.as_secs()
                );
                let _ = wake_tx.send(()).await;
            }
        }
    });

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                let c = client.clone();
                let tx = event_tx.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_command(&c, &tx, &state, cmd).await {
                        log::warn!("Command error: {e:#}");
                    }
                });
            }
            Some(_) = wake_rx.recv() => {
                log::info!("System resumed from suspend — forcing reconnect");
                // Spawn instead of awaiting — force_reconnect() sends a WebSocket
                // close frame which can hang on a stale TCP socket after suspend.
                // Spawning keeps this select loop responsive for commands.
                let c = client.clone();
                tokio::spawn(async move { c.force_reconnect().await; });
            }
            _ = &mut bot_handle => { break; }
        }
    }

    Ok(())
}

// ── Event handler ─────────────────────────────────────────────────────────────

/// Send an unresolved LID to the background resolver. Deduplicates automatically.
fn queue_lid_resolve(resolver_tx: &tokio::sync::mpsc::UnboundedSender<String>, lid: &str) {
    let _ = resolver_tx.send(lid.to_string());
}

/// Resolve a batch of LID JIDs via usync, persist mappings, and notify UI.
async fn resolve_lid_batch(
    client: &Arc<Client>,
    state: &Arc<Mutex<RuntimeState>>,
    tx: &Sender<WaEvent>,
    pending: &mut std::collections::HashSet<String>,
) {
    // Filter out already-resolved LIDs
    let unresolved: Vec<String> = {
        let s = state.lock().unwrap();
        pending
            .drain()
            .filter(|lid| !s.lid_to_phone.contains_key(lid))
            .collect()
    };
    if unresolved.is_empty() {
        return;
    }
    let jids: Vec<Jid> = unresolved
        .iter()
        .filter_map(|s| s.parse::<Jid>().ok())
        .collect();
    if jids.is_empty() {
        return;
    }
    log::info!("LID resolver: resolving {} JIDs via usync", jids.len());
    match client.get_user_devices(&jids).await {
        Ok(_) => {
            let mut resolved = 0u32;
            for lid in &unresolved {
                if let Some(phone_jid) = client.resolve_lid_to_phone_jid(lid).await {
                    let mut s = state.lock().unwrap();
                    s.insert_lid_phone(lid.clone(), phone_jid.clone());
                    // Copy contact name to phone JID if known under LID
                    if let Some(name) = s.contact_names.get(lid).cloned() {
                        if !s.contact_names.contains_key(&phone_jid) {
                            s.contact_names.insert(phone_jid, name);
                        }
                    }
                    resolved += 1;
                }
            }
            if resolved > 0 {
                log::info!("LID resolver: resolved {resolved}/{} JIDs", unresolved.len());
                // Persist mappings
                let map = state.lock().unwrap().lid_to_phone.clone();
                tokio::task::spawn_blocking(move || save_lid_phone_map(&map));
                // Refresh the full chat list so any LID-named chats get updated names
                let chats: Vec<crate::bridge::ChatSummary> = state
                    .lock()
                    .unwrap()
                    .chats_with_best_names()
                    .into_iter()
                    .filter(|c| !c.id.contains("@broadcast"))
                    .collect();
                let _ = tx.send(WaEvent::ChatsLoaded(chats)).await;
            }
        }
        Err(e) => {
            log::warn!("LID resolver: usync failed: {e:#}");
        }
    }
}

async fn handle_wa_event(
    tx: &Sender<WaEvent>,
    state: &Arc<Mutex<RuntimeState>>,
    client: &Arc<Client>,
    lid_resolver_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    event: Event,
) {
    let ev = match event {
        Event::PairingQrCode { code, .. } => WaEvent::QrCode(code),

        Event::Connected(_) => {
            // All chats are already loaded into state at startup (RuntimeState::new).
            // Just upgrade any bare-JID names and then send to UI.
            let chats = {
                let mut s = state.lock().unwrap();
                let mut upgraded = false;
                for c in &mut s.chats {
                    if !c.is_group {
                        let better = display_name_from_jid(&c.id);
                        let looks_raw = c.name.contains('@')
                            || (c.name.chars().all(|c| c.is_ascii_digit()) && c.name.len() > 6);
                        if looks_raw && better != c.name {
                            c.name = better;
                            upgraded = true;
                        }
                    }
                }
                if upgraded {
                    s.chat_names = s
                        .chats
                        .iter()
                        .map(|c| (c.id.clone(), c.name.clone()))
                        .collect();
                    let _ = s.save_tx.send(s.chats.clone()); // async disk write
                }
                // Apply any phonebook names that arrived before Connected fired
                s.chats_with_best_names()
            };

            // Filter out broadcast/status chats and send all of them.
            let initial_chats: Vec<ChatSummary> = chats
                .into_iter()
                .filter(|c| !c.id.contains("@broadcast"))
                .collect();
            // Log name resolution stats
            {
                let s = state.lock().unwrap();
                log::info!(
                    "Name stats: {} contact_names, {} lid_to_phone, {} chat_names",
                    s.contact_names.len(),
                    s.lid_to_phone.len(),
                    s.chat_names.len()
                );
                // Check how many chats got names
                let named = initial_chats
                    .iter()
                    .filter(|c| !c.name.contains('@') && !c.name.is_empty())
                    .count();
                log::info!("Chats with resolved names: {named}/{}", initial_chats.len());
            }
            // Get own JID + name for avatar + identity
            let (own_phone, own_name) = {
                let device = client.persistence_manager().get_device_snapshot().await;
                // Strip device suffix (e.g. "1234567890:82@s.whatsapp.net" → "1234567890@s.whatsapp.net")
                let raw = device
                    .pn
                    .as_ref()
                    .map(|j| j.to_string())
                    .unwrap_or_default();
                let phone = if let (Some(colon), Some(at)) = (raw.find(':'), raw.find('@')) {
                    if colon < at {
                        format!("{}{}", &raw[..colon], &raw[at..])
                    } else {
                        raw
                    }
                } else {
                    raw
                };
                let name = device.push_name.clone();
                (phone, name)
            };
            // Track reconnect for resume-from-suspend sync
            let is_reconnect = {
                let mut s = state.lock().unwrap();
                s.connect_count += 1;
                s.connect_count > 1
            };
            if is_reconnect {
                log::info!("Reconnect detected — requesting missed message sync");
                // Re-sync app state and send fresh chat list AFTER sync completes.
                // Previously the chat list was sent before the resync started,
                // so the UI got stale data and never received the update.
                {
                    use wacore::appstate::patch_decode::WAPatchName;
                    let c = client.clone();
                    let t = tx.clone();
                    let s = state.clone();
                    tokio::spawn(async move {
                        let _ = t.send(WaEvent::SyncProgress(true)).await;
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                        let _ = c.resync_app_state(WAPatchName::RegularLow).await;
                        let _ = c.resync_app_state(WAPatchName::Regular).await;
                        // Send fresh chat list after resync
                        let chats = s.lock().unwrap().chats_with_best_names();
                        let _ = t.send(WaEvent::ChatsLoaded(chats)).await;
                        let _ = t.send(WaEvent::SyncProgress(false)).await;
                    });
                }
            }

            // Store own JIDs for self-read detection in group receipts
            {
                let device = client.persistence_manager().get_device_snapshot().await;
                let mut s = state.lock().unwrap();
                s.own_phone = own_phone.clone();
                if let Some(lid) = &device.lid {
                    let lid_str = lid.to_string();
                    let stripped =
                        if let (Some(c), Some(a)) = (lid_str.find(':'), lid_str.find('@')) {
                            if c < a {
                                format!("{}{}", &lid_str[..c], &lid_str[a..])
                            } else {
                                lid_str
                            }
                        } else {
                            lid_str
                        };
                    s.own_lid = stripped;
                }
                log::info!("Own JIDs: phone={} lid={}", s.own_phone, s.own_lid);
            }
            log::info!(
                "Connected — loaded {} chats, own_jid={own_phone}, name={own_name}",
                initial_chats.len()
            );
            let _ = tx
                .send(WaEvent::Connected {
                    phone: own_phone,
                    name: own_name,
                })
                .await;
            let _ = tx.send(WaEvent::ChatsLoaded(initial_chats)).await;
            // Do NOT unconditionally start the spinner here — let OfflineSyncPreview drive it.
            // On reconnect, OfflineSyncCompleted fires only once (compare_exchange guard),
            // so showing the spinner here would leave it stuck forever on second connect.

            // Spawn background group-name refresh + LID dedup.
            // Wait 5 s so the initial JoinedGroup burst settles before we make
            // additional IQ requests that compete with the sync traffic.
            let client_clone = client.clone();
            let state_clone = state.clone();
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                // Force sync app state collections
                {
                    use wacore::appstate::patch_decode::WAPatchName;
                    // Regular: quick replies
                    log::info!("Requesting resync of Regular collection...");
                    if let Err(e) = client_clone.resync_app_state(WAPatchName::Regular).await {
                        log::warn!("Regular collection resync failed: {e:#}");
                    }
                    // RegularLow: markChatAsRead, archive, pin — needed for read sync
                    log::info!("Requesting resync of RegularLow collection (read sync)...");
                    if let Err(e) = client_clone.resync_app_state(WAPatchName::RegularLow).await {
                        log::warn!("RegularLow collection resync failed: {e:#}");
                    } else {
                        log::info!("RegularLow collection resync completed");
                    }
                }
                resolve_all_lids(&client_clone, &state_clone).await;
                merge_lid_chats(&client_clone, &state_clone, &tx_clone).await;
                fetch_and_update_group_names(&client_clone, &state_clone, &tx_clone).await;
                // Rebuild reverse map after all LID resolution is done
                state_clone.lock().unwrap().rebuild_phone_to_lid();

                // Send refreshed chat list after LID dedup + group name resolution
                {
                    let fresh: Vec<ChatSummary> = state_clone
                        .lock()
                        .unwrap()
                        .chats_with_best_names()
                        .into_iter()
                        .filter(|c| !c.id.contains("@broadcast"))
                        .collect();
                    log::info!(
                        "POST-GROUP-REFRESH: refreshing UI with {} chats",
                        fresh.len()
                    );
                    let _ = tx_clone.send(WaEvent::ChatsLoaded(fresh)).await;
                }

                serve_cached_avatars(&state_clone, &tx_clone).await;

                // Poll RegularLow every 15s as safety net for read sync.
                // Runs in separate task so it doesn't block main processing.
                {
                    let c = client_clone.clone();
                    let st = state_clone.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                            // Memory diagnostics
                            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                                let rss = status
                                    .lines()
                                    .find(|l| l.starts_with("VmRSS:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .unwrap_or("?");
                                let s = st.lock().unwrap();
                                let cache_chats = s.history.len();
                                let cache_msgs: usize = s.history.values().map(|v| v.len()).sum();
                                let contacts = s.contact_names.len();
                                log::info!(
                                    "MEM: RSS={rss}KB cache={cache_chats}chats/{cache_msgs}msgs contacts={contacts}"
                                );
                            }
                            use wacore::appstate::patch_decode::WAPatchName;
                            log::debug!("RegularLow poll tick");
                            match c.resync_app_state(WAPatchName::RegularLow).await {
                                Ok(_) => {}
                                Err(e) => log::debug!("RegularLow poll error: {e:#}"),
                            }
                        }
                    });
                }
            });

            return;
        }

        Event::Disconnected(_) | Event::LoggedOut(_) => {
            WaEvent::Disconnected("Connection closed".to_string())
        }

        Event::Message(msg, info) => {
            let msg_id = info.id.to_string();
            let raw_chat_id = info.source.chat.to_string();

            // Log poll-related messages
            {
                let base = msg.get_base_message();
                if base.poll_update_message.is_some() {
                    log::debug!("Poll update message: id={msg_id} chat={raw_chat_id}");
                }
                if base.poll_creation_message.is_some()
                    || base.poll_creation_message_v2.is_some()
                    || base.poll_creation_message_v3.is_some()
                {
                    log::debug!("Poll creation message: id={msg_id} chat={raw_chat_id}");
                }
            }

            // Skip status broadcasts (WhatsApp Stories) — they aren't regular chats
            if raw_chat_id == "status@broadcast" || raw_chat_id.contains("@broadcast") {
                return;
            }

            // Seed LID→phone mapping from sender_alt (alternative JID) when available.
            // This ensures future messages can be resolved even without a server round-trip.
            if let Some(alt) = &info.source.sender_alt {
                let sender = info.source.sender.to_string();
                let alt_str = alt.to_string();
                let mut s = state.lock().unwrap();
                if sender.ends_with("@lid") && alt_str.ends_with("@s.whatsapp.net") {
                    s.insert_lid_phone(sender, alt_str);
                } else if alt_str.ends_with("@lid") && sender.ends_with("@s.whatsapp.net") {
                    s.insert_lid_phone(alt_str, sender);
                }
            }

            // Resolve LID chat_id to phone JID: check sender_alt first (for 1:1 DMs),
            // then cached lid_to_phone mapping. IMPORTANT: only trust the mapping
            // if the resolved phone JID actually has an existing chat — otherwise
            // stale/corrupt LID→phone mappings create phantom chats with wrong
            // phone numbers (bug where self-sent messages from phone land in a
            // completely different chat).
            let (chat_id, needs_lid_resolve) = if raw_chat_id.ends_with("@lid") {
                let alt_phone = info
                    .source
                    .sender_alt
                    .as_ref()
                    .filter(|a| a.to_string().ends_with("@s.whatsapp.net"))
                    .map(|a| a.to_string());
                let s = state.lock().unwrap();
                // Try cached mapping first
                let cached = s.lid_to_phone.get(&raw_chat_id).cloned();
                // Strip :device suffix for alternate lookup (LIDs can arrive
                // with or without device suffix; the stored map uses non-AD)
                let base_lid = if let Some(colon) = raw_chat_id.find(':') {
                    if let Some(at) = raw_chat_id.find('@') {
                        if colon < at {
                            format!("{}{}", &raw_chat_id[..colon], &raw_chat_id[at..])
                        } else {
                            raw_chat_id.clone()
                        }
                    } else {
                        raw_chat_id.clone()
                    }
                } else {
                    raw_chat_id.clone()
                };
                let cached_base = s.lid_to_phone.get(&base_lid).cloned();

                // Check if contact_names has an entry for this LID (or its base form).
                // If yes AND we can find the person's phone JID chat, merge there.
                let contact_phone_chat = {
                    let find_chat_by_name = |name: &str| -> Option<String> {
                        if name.is_empty() {
                            return None;
                        }
                        s.chats
                            .iter()
                            .find(|c| c.id.ends_with("@s.whatsapp.net") && c.name == name)
                            .map(|c| c.id.clone())
                    };
                    s.contact_names
                        .get(&raw_chat_id)
                        .or_else(|| s.contact_names.get(&base_lid))
                        .and_then(|name| find_chat_by_name(name))
                };

                let mut needs_resolve = false;
                let resolved = if info.source.is_from_me {
                    match cached.as_ref().or(cached_base.as_ref()).or(alt_phone.as_ref()) {
                        Some(phone) if s.chats.iter().any(|c| &c.id == phone) => {
                            Some(phone.clone())
                        }
                        _ if contact_phone_chat.is_some() => contact_phone_chat,
                        _ => {
                            if s.chats.iter().any(|c| c.id == raw_chat_id) {
                                None // keep raw_chat_id as the @lid form
                            } else if s.chats.iter().any(|c| c.id == base_lid) {
                                Some(base_lid.clone())
                            } else {
                                // Neither form known — queue background
                                // resolution so merge_lid_chats can dedup later.
                                needs_resolve = true;
                                cached
                                    .clone()
                                    .or_else(|| cached_base.clone())
                                    .or_else(|| alt_phone.clone())
                            }
                        }
                    }
                } else {
                    cached.or(cached_base).or(alt_phone)
                };
                (resolved.unwrap_or(raw_chat_id), needs_resolve)
            } else {
                (raw_chat_id, false)
            };

            // Queue background LID resolution when a self-message landed on a
            // phantom chat. If resolution finds the phone JID for an existing
            // chat, merge_lid_chats (triggered on next startup/refresh) folds
            // the phantom into the real chat.
            if needs_lid_resolve && chat_id.ends_with("@lid") {
                queue_lid_resolve(&lid_resolver_tx, &chat_id);
                log::info!(
                    "Queued LID resolution for self-message phantom chat: {chat_id}"
                );
            }

            // Handle message revoke (delete for everyone) — edit_attribute tells us
            use whatsapp_rust::types::message::EditAttribute;
            let is_revoke = matches!(
                info.edit,
                EditAttribute::SenderRevoke | EditAttribute::AdminRevoke
            );
            if is_revoke {
                // Target ID can be in meta_info.target_id or we fall back to msg_id
                let target_id = info
                    .meta_info
                    .target_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| msg_id.clone());
                log::info!("Revoke message received: {target_id} in {chat_id}");
                // Remove from cache
                {
                    let mut s = state.lock().unwrap();
                    if let Some(msgs) = s.history.get_mut(&chat_id) {
                        msgs.retain(|m| m.id != target_id);
                        s.queue_save_messages(&chat_id);
                    }
                }
                let _ = tx
                    .send(WaEvent::MessageDeletedLocal {
                        chat_id,
                        msg_id: target_id,
                    })
                    .await;
                return;
            }

            // Handle message edits — update text in cache and notify UI.
            // Detect edits either via the edit attribute OR by checking
            // for protocol_message.edited_message (self-edits from phone
            // sometimes arrive without the edit attribute set).
            let has_edited_msg = msg
                .protocol_message
                .as_ref()
                .and_then(|pm| pm.edited_message.as_ref())
                .is_some();
            if matches!(info.edit, EditAttribute::MessageEdit) || has_edited_msg {
                // Target ID priority: meta_info.target_id → protocol_message.key.id
                // → fall back to msg_id (last resort; risks editing wrong msg)
                let target_id = info
                    .meta_info
                    .target_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .or_else(|| {
                        msg.protocol_message
                            .as_ref()
                            .and_then(|pm| pm.key.as_ref())
                            .and_then(|k| k.id.clone())
                    })
                    .unwrap_or_else(|| msg_id.clone());
                // Edits arrive wrapped in protocol_message.edited_message (tag 14).
                // text_content() alone doesn't unwrap this layer, so check both.
                let new_text = msg
                    .text_content()
                    .map(|s| s.to_string())
                    .or_else(|| {
                        msg.protocol_message
                            .as_ref()
                            .and_then(|pm| pm.edited_message.as_deref())
                            .and_then(|em| em.text_content().map(|s| s.to_string()))
                    })
                    .unwrap_or_default();
                log::info!(
                    "Message edit received: {target_id} in {chat_id} new_text={:?}",
                    new_text
                );
                {
                    let mut s = state.lock().unwrap();
                    if let Some(msgs) = s.history.get_mut(&chat_id) {
                        if let Some(m) = msgs.iter_mut().find(|m| m.id == target_id) {
                            m.text = Some(new_text.clone());
                            m.is_edited = true;
                        }
                        s.queue_save_messages(&chat_id);
                    }
                }
                let _ = tx
                    .send(WaEvent::MessageEdited {
                        chat_id,
                        msg_id: target_id,
                        new_text,
                    })
                    .await;
                return;
            }

            // Handle incoming reactions — persist to cache AND notify UI
            {
                let base = msg.get_base_message();
                if let Some(rm) = &base.reaction_message {
                    let target_id = rm
                        .key
                        .as_ref()
                        .and_then(|k| k.id.clone())
                        .unwrap_or_default();
                    let emoji = rm.text.clone().unwrap_or_default();
                    let sender = info.source.sender.to_string();
                    if !target_id.is_empty() {
                        // Persist reaction to message cache
                        {
                            let mut s = state.lock().unwrap();
                            if let Some(msgs) = s.history.get_mut(&chat_id) {
                                if let Some(m) = msgs.iter_mut().find(|m| m.id == target_id) {
                                    if emoji.is_empty() {
                                        // Empty emoji = reaction removed
                                        m.reactions.retain(|(s, _)| *s != sender);
                                    } else {
                                        // Replace existing reaction from this sender or add new
                                        m.reactions.retain(|(s, _)| *s != sender);
                                        m.reactions.push((sender, emoji.clone()));
                                    }
                                    s.queue_save_messages(&chat_id);
                                }
                            }
                        }
                        if !emoji.is_empty() {
                            let _ = tx
                                .send(WaEvent::ReactionUpdated {
                                    chat_id: chat_id.clone(),
                                    msg_id: target_id,
                                    emoji,
                                })
                                .await;
                        }
                    }
                    return;
                }
            }

            // Handle incoming pin messages
            {
                let base = msg.get_base_message();
                if let Some(pin) = &base.pin_in_chat_message {
                    if let Some(key) = &pin.key {
                        let pinned_msg_id = key.id.clone().unwrap_or_default();
                        let pin_type = pin.r#type.unwrap_or(0);
                        log::info!(
                            "Incoming pin: msg={pinned_msg_id} type={pin_type} in {chat_id}"
                        );
                        if pin_type == 1 {
                            // PinForAll
                            let _ = tx
                                .send(WaEvent::MessagePinned {
                                    chat_id: chat_id.clone(),
                                    msg_id: pinned_msg_id,
                                })
                                .await;
                        }
                    }
                    return;
                }
            }

            // Handle incoming poll votes (PollUpdateMessage)
            {
                let base = msg.get_base_message();
                if let Some(pum) = &base.poll_update_message {
                    // The poll_update targets the original poll message
                    let poll_msg_id = pum
                        .poll_creation_message_key
                        .as_ref()
                        .and_then(|k| k.id.clone())
                        .unwrap_or_default();
                    let voter_jid = info.source.sender.clone();
                    log::info!(
                        "PollUpdate received: voter={voter_jid} poll={poll_msg_id} in {chat_id}"
                    );

                    // Find the poll message and its secret (check memory then disk)
                    let (poll_secret, poll_options, poll_creator) = {
                        let s = state.lock().unwrap();
                        let from_mem = if let Some(msgs) = s.history.get(&chat_id) {
                            msgs.iter().find(|m| m.id == poll_msg_id).map(|pm| {
                                (
                                    pm.poll_secret.clone(),
                                    pm.poll_options.clone(),
                                    pm.sender_id.clone(),
                                )
                            })
                        } else {
                            None
                        };
                        if let Some(found) = from_mem {
                            log::info!("  Found poll in memory: secret_len={}", found.0.len());
                            found
                        } else {
                            let mem_chats: Vec<String> = s.history.keys().cloned().collect();
                            log::info!(
                                "  Poll NOT in memory. History has {} chats. Looking for chat_id={chat_id}",
                                mem_chats.len()
                            );
                            drop(s);
                            // Fallback: load from disk
                            let disk_msgs = load_messages(&chat_id);
                            log::info!("  Loaded {} msgs from disk for {chat_id}", disk_msgs.len());
                            let found = disk_msgs
                                .iter()
                                .find(|m| m.id == poll_msg_id)
                                .map(|pm| {
                                    log::info!(
                                        "  Found poll on disk: secret_len={}",
                                        pm.poll_secret.len()
                                    );
                                    (
                                        pm.poll_secret.clone(),
                                        pm.poll_options.clone(),
                                        pm.sender_id.clone(),
                                    )
                                })
                                .unwrap_or_else(|| {
                                    log::warn!(
                                        "  Poll msg {poll_msg_id} NOT FOUND on disk either!"
                                    );
                                    (vec![], vec![], String::new())
                                });
                            found
                        }
                    };

                    log::info!(
                        "PollUpdate lookup: chat={chat_id} poll_msg_id={poll_msg_id} secret_len={} opts={} creator={poll_creator:?}",
                        poll_secret.len(),
                        poll_options.len()
                    );

                    if !poll_secret.is_empty() {
                        // Decrypt the vote
                        if let Some(vote) = &pum.vote {
                            if let (Some(enc_payload), Some(enc_iv)) =
                                (&vote.enc_payload, &vote.enc_iv)
                            {
                                // For key derivation, try PHONE JID first (WhatsApp may use PN, not LID)
                                let creator_jid: Jid = if poll_creator.is_empty() {
                                    let s = state.lock().unwrap();
                                    // Prefer phone JID for key derivation
                                    if !s.own_phone.is_empty() {
                                        s.own_phone.parse().unwrap_or(voter_jid.clone())
                                    } else if !s.own_lid.is_empty() {
                                        s.own_lid.parse().unwrap_or(voter_jid.clone())
                                    } else {
                                        voter_jid.clone()
                                    }
                                } else {
                                    // Resolve LID to phone if possible
                                    let resolved = {
                                        let s = state.lock().unwrap();
                                        if poll_creator.ends_with("@lid") {
                                            s.lid_to_phone.get(&poll_creator).cloned()
                                        } else {
                                            None
                                        }
                                    };
                                    resolved.and_then(|p| p.parse().ok()).unwrap_or_else(|| {
                                        poll_creator.parse().unwrap_or(voter_jid.clone())
                                    })
                                };
                                // Also resolve voter LID to phone
                                let voter_resolved: Jid = {
                                    let vs = voter_jid.to_non_ad().to_string();
                                    if vs.ends_with("@lid") {
                                        let s = state.lock().unwrap();
                                        s.lid_to_phone
                                            .get(&vs)
                                            .and_then(|p| p.parse().ok())
                                            .unwrap_or(voter_jid.clone())
                                    } else {
                                        voter_jid.clone()
                                    }
                                };
                                // Try multiple JID combinations for key derivation
                                // (WhatsApp uses PN JIDs on phone, LID JIDs on web)
                                let creator_pn = creator_jid.to_non_ad().to_string();
                                let voter_pn = voter_resolved.to_non_ad().to_string();
                                let voter_lid = voter_jid.to_non_ad().to_string();
                                // Also get creator LID
                                let creator_lid = {
                                    let s = state.lock().unwrap();
                                    if creator_pn.ends_with("@s.whatsapp.net") {
                                        s.lid_to_phone
                                            .iter()
                                            .find(|(_, p)| **p == creator_pn)
                                            .map(|(l, _)| l.clone())
                                            .unwrap_or_default()
                                    } else {
                                        creator_pn.clone()
                                    }
                                };
                                log::info!(
                                    "PollUpdate decrypt: creator_pn={creator_pn} creator_lid={creator_lid} voter_pn={voter_pn} voter_lid={voter_lid} secret_len={}",
                                    poll_secret.len()
                                );

                                // Try ALL possible JID combinations
                                let mut combos: Vec<(&str, &str)> = vec![
                                    (&creator_pn, &voter_pn),
                                    (&creator_pn, &voter_lid),
                                    (&creator_lid, &voter_pn),
                                    (&creator_lid, &voter_lid),
                                ];
                                // Deduplicate
                                combos.dedup();
                                let mut decrypted = None;
                                for (c, v) in &combos {
                                    if let Ok(key) = wacore::poll::derive_vote_encryption_key(
                                        &poll_secret,
                                        &poll_msg_id,
                                        c,
                                        v,
                                    ) {
                                        if let Ok(hashes) = wacore::poll::decrypt_poll_vote(
                                            enc_payload,
                                            enc_iv,
                                            &key,
                                            &poll_msg_id,
                                            v,
                                        ) {
                                            decrypted = Some(hashes);
                                            log::info!(
                                                "PollUpdate decrypted with creator={c} voter={v}"
                                            );
                                            break;
                                        }
                                    }
                                }
                                match decrypted {
                                    Some(option_hashes) => {
                                        // Match hashes to option names
                                        let voted: Vec<String> = option_hashes
                                            .iter()
                                            .filter_map(|hash| {
                                                poll_options
                                                    .iter()
                                                    .find(|opt| {
                                                        wacore::poll::compute_option_hash(opt)
                                                            .as_slice()
                                                            == hash.as_slice()
                                                    })
                                                    .cloned()
                                            })
                                            .collect();
                                        let voter_name = {
                                            let s = state.lock().unwrap();
                                            resolve_sender_name(&s, &voter_jid.to_string())
                                        };
                                        log::info!(
                                            "Poll vote decoded: {voter_name} voted for {voted:?}"
                                        );
                                        // Persist vote to message cache — load from disk if not in memory
                                        let all_votes = {
                                            let mut s = state.lock().unwrap();
                                            if !s.history.contains_key(&chat_id) {
                                                let disk_msgs = load_messages(&chat_id);
                                                if !disk_msgs.is_empty() {
                                                    s.history.insert(chat_id.clone(), disk_msgs);
                                                }
                                            }
                                            if let Some(msgs) = s.history.get_mut(&chat_id) {
                                                if let Some(pm) =
                                                    msgs.iter_mut().find(|m| m.id == poll_msg_id)
                                                {
                                                    pm.poll_votes.retain(|(n, _)| *n != voter_name);
                                                    pm.poll_votes
                                                        .push((voter_name.clone(), voted.clone()));
                                                    log::info!(
                                                        "VOTE_PERSIST: {} now has {} votes",
                                                        poll_msg_id,
                                                        pm.poll_votes.len()
                                                    );
                                                }
                                                s.queue_save_messages(&chat_id);
                                            }
                                            // Read back accumulated votes
                                            s.history
                                                .get(&chat_id)
                                                .and_then(|msgs| {
                                                    msgs.iter().find(|m| m.id == poll_msg_id)
                                                })
                                                .map(|m| m.poll_votes.clone())
                                                .unwrap_or_default()
                                        };
                                        let _ = tx
                                            .send(WaEvent::PollVoteUpdate {
                                                chat_id: chat_id.clone(),
                                                poll_msg_id: poll_msg_id.clone(),
                                                all_votes,
                                            })
                                            .await;
                                    }
                                    None => log::warn!(
                                        "Failed to decrypt poll vote with any JID combination"
                                    ),
                                }
                            }
                        }
                    } else {
                        log::warn!(
                            "PollUpdate: NO SECRET for poll {poll_msg_id} — cannot decrypt vote"
                        );
                    }
                    return;
                }
            }

            // Extract media info (borrow msg before consuming it)
            let pending = {
                let base = msg.get_base_message();
                extract_pending_download(base)
            };

            // Diagnostic logging for ALL messages — helps catch missing-message bugs.
            let is_from_me_dbg = info.source.is_from_me;
            let is_group_dbg = chat_id.ends_with("@g.us");
            let raw_chat_dbg = info.source.chat.to_string();
            let sender_dbg = info.source.sender.to_string();
            let edit_dbg = format!("{:?}", info.edit);
            let msg_id_dbg = info.id.to_string();

            let mapped = map_message(*msg, info);
            if mapped.is_none() {
                log::warn!(
                    "DROPPED message: id={msg_id_dbg} chat={chat_id} raw_chat={raw_chat_dbg} \
                     sender={sender_dbg} from_me={is_from_me_dbg} group={is_group_dbg} \
                     edit={edit_dbg} — map_message returned None (no text/media/contact/poll)"
                );
                return;
            }
            match mapped {
                Some(mut m) => {
                    log::info!(
                        "MSG routed: id={} chat={} raw_chat={raw_chat_dbg} from_me={is_from_me_dbg} \
                         group={is_group_dbg} sender={sender_dbg} text={:?}",
                        m.id,
                        m.chat_id,
                        m.text.as_deref().unwrap_or("<media>")
                    );
                    // Override chat_id with the resolved (merged) version
                    m.chat_id = chat_id.clone();

                    // Resolve sender name, quoted sender, and @mentions in text
                    {
                        let s = state.lock().unwrap();
                        if m.sender_name.is_empty() && !m.sender_id.is_empty() && !m.is_from_me {
                            m.sender_name = resolve_sender_name(&s, &m.sender_id);
                            // DM fallback: if sender is unresolved LID, use chat name
                            if m.sender_name.contains("@lid") && !chat_id.ends_with("@g.us") {
                                let chat_name = resolve_sender_name(&s, &chat_id);
                                if !chat_name.contains('@') {
                                    m.sender_name = chat_name;
                                } else if let Some(name) = s.chat_names.get(&chat_id) {
                                    if !name.is_empty() && !name.contains('@') {
                                        m.sender_name = name.clone();
                                    }
                                }
                            }
                        }
                        if let Some(qs) = &m.quoted_sender {
                            if qs.contains('@') {
                                m.quoted_sender = Some(resolve_sender_name(&s, qs));
                            } else if qs.starts_with('+') || qs.chars().all(|c| c.is_ascii_digit()) {
                                let num = qs.trim_start_matches('+');
                                let phone_jid = format!("{num}@s.whatsapp.net");
                                let resolved = resolve_sender_name(&s, &phone_jid);
                                if resolved != phone_jid {
                                    m.quoted_sender = Some(resolved);
                                }
                            }
                        }
                        // Resolve reaction sender JIDs to display names
                        for (sender, _) in &mut m.reactions {
                            if sender.contains('@') {
                                *sender = resolve_sender_name(&s, sender);
                            }
                        }
                        // Resolve @mentions in text AND quoted_text
                        if let Some(ref text) = m.text {
                            let resolved = resolve_mentions(text, &s);
                            if resolved != *text {
                                m.text = Some(resolved);
                            }
                        }
                        if let Some(ref qt) = m.quoted_text {
                            let resolved = resolve_mentions(qt, &s);
                            if resolved != *qt {
                                m.quoted_text = Some(resolved);
                            }
                        }
                    }
                    // Store push_name as contact name for group participants
                    // who aren't in the user's contacts. This lets resolve_sender_name
                    // find their profile name on future lookups.
                    if is_valid_contact_name(&m.sender_name, &m.sender_id)
                        && !m.sender_id.is_empty()
                        && !m.is_from_me
                    {
                        let mut s = state.lock().unwrap();
                        if !s.contact_names.contains_key(&m.sender_id) {
                            s.contact_names
                                .insert(m.sender_id.clone(), m.sender_name.clone());
                            // Also store under phone JID if sender is LID
                            if m.sender_id.ends_with("@lid") {
                                if let Some(phone) = s.lid_to_phone.get(&m.sender_id).cloned() {
                                    if !s.contact_names.contains_key(&phone)
                                        && is_valid_contact_name(&m.sender_name, &phone)
                                    {
                                        s.contact_names.insert(phone, m.sender_name.clone());
                                    }
                                }
                            }
                        }
                    }
                    // Update last_msg_id + sender for mark-as-read
                    {
                        let mut s = state.lock().unwrap();
                        s.last_msg_id.insert(m.chat_id.clone(), m.id.clone());
                        if !m.sender_id.is_empty() && !m.is_from_me {
                            s.last_msg_sender
                                .insert(m.chat_id.clone(), m.sender_id.clone());
                        }
                    }
                    // Persist message and update chat summary
                    let (name_update, new_chat) = persist_new_message(&m, state);
                    if let Some(s) = new_chat {
                        let _ = tx.send(WaEvent::ChatAdded(s)).await;
                    }

                    // Spawn media download if needed
                    if let Some(dl) = pending {
                        let c = client.clone();
                        let t = tx.clone();
                        let id = msg_id.clone();
                        let cid = chat_id.clone();
                        let st = state.clone();
                        tokio::spawn(async move {
                            execute_media_download(c, t, &st, id, cid, dl).await;
                        });
                    }

                    // Send name update if the contact name was newly discovered
                    if let Some((nid, nname)) = name_update {
                        let _ = tx
                            .send(WaEvent::ChatNameUpdated {
                                chat_id: nid,
                                name: nname,
                            })
                            .await;
                    }

                    // Queue unresolved LID for background resolution
                    if m.sender_name.contains("@lid") && m.sender_id.ends_with("@lid") && !m.is_from_me {
                        queue_lid_resolve(&lid_resolver_tx, &m.sender_id);
                    }

                    WaEvent::MessageReceived(m)
                }
                None => return,
            }
        }

        Event::Receipt(r) => {
            log::debug!(
                "Receipt: type={:?} chat={} msgs={} sender={}",
                r.r#type,
                r.source.chat,
                r.message_ids.len(),
                r.message_sender
            );
            // Detect "we read on another device":
            // - ReadSelf type (DMs)
            // - Read type where sender is OUR OWN LID/phone (groups)
            let sender_raw = r.message_sender.to_string();
            let sender_stripped =
                if let (Some(c), Some(a)) = (sender_raw.find(':'), sender_raw.find('@')) {
                    if c < a {
                        format!("{}{}", &sender_raw[..c], &sender_raw[a..])
                    } else {
                        sender_raw.clone()
                    }
                } else {
                    sender_raw.clone()
                };
            let is_own_read = if matches!(r.r#type, ReceiptType::Read) {
                let s = state.lock().unwrap();
                (!s.own_lid.is_empty() && sender_stripped == s.own_lid)
                    || (!s.own_phone.is_empty() && sender_stripped == s.own_phone)
            } else {
                false
            };
            let is_read_self = matches!(r.r#type, ReceiptType::ReadSelf) || is_own_read;
            let status = match r.r#type {
                ReceiptType::Read | ReceiptType::ReadSelf => ReceiptStatus::Read,
                _ => ReceiptStatus::Delivered,
            };
            // Use the receipt's source.chat directly — no scanning all history
            let chat_id = {
                let raw = r.source.chat.to_string();
                // Strip device suffix (e.g., :82)
                let stripped = if let (Some(colon), Some(at)) = (raw.find(':'), raw.find('@')) {
                    if colon < at {
                        format!("{}{}", &raw[..colon], &raw[at..])
                    } else {
                        raw
                    }
                } else {
                    raw
                };
                let s = state.lock().unwrap();
                if stripped.ends_with("@lid") {
                    s.lid_to_phone.get(&stripped).cloned().unwrap_or(stripped)
                } else {
                    stripped
                }
            };

            if !is_read_self {
                // Update sent message receipt status in cache (fast: only scan ONE chat)
                let mut s = state.lock().unwrap();
                if let Some(msgs) = s.history.get_mut(&chat_id) {
                    let mut changed = false;
                    for msg_id in &r.message_ids {
                        if let Some(m) = msgs.iter_mut().find(|m| m.id == *msg_id && m.is_from_me) {
                            m.receipt_status = status.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        s.queue_save_messages(&chat_id);
                    }
                }
            } else {
                // We read on another device. For groups (is_own_read), chat_id IS the group.
                // For DMs (ReadSelf), chat_id is our own JID — need message ID lookup.
                let mut actual_chats: Vec<String> = Vec::new();
                if is_own_read {
                    // Group: chat_id is already correct
                    actual_chats.push(chat_id.clone());
                }
                // Phase 1: scan in-memory history (fast)
                {
                    let s = state.lock().unwrap();
                    for msg_id in &r.message_ids {
                        for (cid, msgs) in &s.history {
                            if msgs.iter().any(|m| m.id == *msg_id) {
                                if !actual_chats.contains(cid) {
                                    actual_chats.push(cid.clone());
                                }
                                break;
                            }
                        }
                    }
                }
                // Phase 2: if not found in memory, scan unread chats' disk files
                if actual_chats.is_empty() {
                    let unread_chats: Vec<String> = state
                        .lock()
                        .unwrap()
                        .chats
                        .iter()
                        .filter(|c| c.unread_count > 0)
                        .map(|c| c.id.clone())
                        .collect();
                    for msg_id in &r.message_ids {
                        for cid in &unread_chats {
                            if actual_chats.contains(cid) {
                                continue;
                            }
                            let disk_msgs = load_messages(cid);
                            if disk_msgs.iter().any(|m| m.id == *msg_id) {
                                actual_chats.push(cid.clone());
                                break;
                            }
                        }
                        if !actual_chats.is_empty() {
                            break;
                        }
                    }
                }
                // If we couldn't find the chat from message IDs, fall back to the receipt's chat field
                // (which may be wrong for ReadSelf but is our best guess)
                if actual_chats.is_empty() {
                    actual_chats.push(chat_id.clone());
                }
                for cid in &actual_chats {
                    log::info!("ReadSelf: clearing unread for chat={cid}");
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(c) = s.chats.iter_mut().find(|c| c.id == *cid) {
                            if c.unread_count > 0 {
                                log::info!("  Reset unread from {} → 0", c.unread_count);
                                c.unread_count = 0;
                                let _ = s.save_tx.send(s.chats.clone());
                            }
                        }
                    }
                    let _ = tx
                        .send(WaEvent::ChatReadOnOtherDevice {
                            chat_id: cid.clone(),
                        })
                        .await;
                }
            }
            for msg_id in r.message_ids {
                let _ = tx
                    .send(WaEvent::ReceiptUpdate {
                        msg_id,
                        status: status.clone(),
                    })
                    .await;
            }
            return;
        }

        // Log ALL notifications for diagnostics
        Event::Notification(node) => {
            if let Some(type_attr) = node.attrs.get("type") {
                log::info!("Notification: type={} tag={}", type_attr.as_str(), node.tag);
            }
            return;
        }

        // App state sync: another device marked a chat as read
        Event::MarkChatAsReadUpdate(update) => {
            let raw_jid = update.jid.to_string();
            let is_read = update.action.read.unwrap_or(false);
            // Strip device suffix and resolve LID→phone
            let stripped = if let (Some(colon), Some(at)) = (raw_jid.find(':'), raw_jid.find('@')) {
                if colon < at {
                    format!("{}{}", &raw_jid[..colon], &raw_jid[at..])
                } else {
                    raw_jid.clone()
                }
            } else {
                raw_jid.clone()
            };
            let chat_id = {
                let s = state.lock().unwrap();
                if stripped.ends_with("@lid") {
                    s.lid_to_phone.get(&stripped).cloned().unwrap_or(stripped)
                } else {
                    stripped
                }
            };
            log::info!(
                "MarkChatAsReadUpdate: {chat_id} read={is_read} full_sync={}",
                update.from_full_sync
            );
            // For full sync events, only process "read" updates (clearing badges
            // is always safe). Skip "unread" from full sync since the server's
            // unread_count in history sync is more authoritative.
            if update.from_full_sync && !is_read {
                return;
            }
            if is_read {
                {
                    let mut s = state.lock().unwrap();
                    if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                        c.unread_count = 0;
                        let _ = s.save_tx.send(s.chats.clone());
                    }
                }
                let _ = tx.send(WaEvent::ChatReadOnOtherDevice { chat_id }).await;
            }
            return;
        }

        Event::ChatPresence(p) => {
            let is_typing = matches!(p.state, ChatPresence::Composing);
            let raw_sender = p.source.sender.to_string();
            let raw_chat = p.source.chat.to_string();

            // Strip device suffix and resolve LID→phone for both chat and sender
            let strip_device = |jid: &str| -> String {
                if let (Some(colon), Some(at)) = (jid.find(':'), jid.find('@')) {
                    if colon < at {
                        return format!("{}{}", &jid[..colon], &jid[at..]);
                    }
                }
                jid.to_string()
            };
            let chat_stripped = strip_device(&raw_chat);
            let sender_stripped = strip_device(&raw_sender);

            let (chat_id, sender_name) = {
                let s = state.lock().unwrap();
                let cid = if chat_stripped.ends_with("@lid") {
                    s.lid_to_phone
                        .get(&chat_stripped)
                        .cloned()
                        .unwrap_or(chat_stripped.clone())
                } else {
                    chat_stripped.clone()
                };
                let mut sname = resolve_sender_name(&s, &sender_stripped);
                // For DM chats, the sender IS the contact — use the chat name
                // if sender resolution failed (still contains @lid)
                if sname.contains("@lid") && !cid.ends_with("@g.us") {
                    // Try resolving via the chat JID (which may be phone-based)
                    let chat_name = resolve_sender_name(&s, &cid);
                    if !chat_name.contains('@') {
                        sname = chat_name;
                    } else if let Some(name) = s.chat_names.get(&cid) {
                        if !name.is_empty() && !name.contains('@') {
                            sname = name.clone();
                        }
                    }
                }
                (cid, sname)
            };

            // Queue unresolved LID for background resolution
            if sender_name.contains("@lid") && sender_stripped.ends_with("@lid") {
                queue_lid_resolve(&lid_resolver_tx, &sender_stripped);
            }

            log::debug!("Typing: chat={chat_id} sender={sender_name} typing={is_typing}");
            WaEvent::TypingIndicator {
                chat_id,
                sender_name,
                is_typing,
            }
        }

        Event::JoinedGroup(lazy_conv) => {
            if let Some(conv) = lazy_conv.get_with_messages() {
                let raw_id = conv.id.clone();

                // Skip status broadcasts and internal chats
                if raw_id == "status@broadcast" || raw_id.contains("@broadcast") {
                    return;
                }

                // Resolve LID to phone JID using all available sources:
                // 1. pn_jid from the conversation proto (most reliable)
                // 2. Cached lid_to_phone mapping from prior sessions
                // 3. Fallback to raw @lid only as last resort
                let pn_from_conv = conv.pn_jid.as_deref().unwrap_or("").to_string();
                let chat_id = if raw_id.ends_with("@lid") {
                    if !pn_from_conv.is_empty() && pn_from_conv.ends_with("@s.whatsapp.net") {
                        // Store the mapping immediately so later events also resolve
                        state
                            .lock()
                            .unwrap()
                            .insert_lid_phone(raw_id.clone(), pn_from_conv.clone());
                        pn_from_conv.clone()
                    } else {
                        let s = state.lock().unwrap();
                        s.lid_to_phone.get(&raw_id).cloned().unwrap_or(raw_id)
                    }
                } else {
                    raw_id
                };

                // Name resolution (lowest → highest priority):
                // 1. display_name / conv.name from history sync
                // 2. push_name from a received message in this conversation
                // 3. formatted phone number fallback
                // 4. [overrides all above] already-saved name if it looks real (reconnect)
                // 5. [overrides all above] app-state ContactUpdate name (phonebook)
                let raw_name = conv
                    .display_name
                    .clone()
                    .or_else(|| conv.name.clone())
                    .filter(|n| !n.is_empty() && !n.contains('@'))
                    .or_else(|| {
                        if !chat_id.ends_with("@g.us") {
                            conv.messages.iter().find_map(|h| {
                                let web_msg = h.message.as_ref()?;
                                let from_me = web_msg.key.from_me.unwrap_or(false);
                                if !from_me {
                                    web_msg.push_name.clone().filter(|n| !n.is_empty())
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| display_name_from_jid(&chat_id));

                // Apply higher-priority name overrides from state.
                // Also check conv.pn_jid: for LID conversations the phone JID may
                // have a contact_name entry even when the lid JID doesn't.
                let pn_jid = conv.pn_jid.as_deref().unwrap_or("").to_string();
                let name = {
                    let s = state.lock().unwrap();
                    // 1. Phonebook name keyed by chat_id (phone or lid JID)
                    // 2. Phonebook name keyed by the alternate JID (pn_jid)
                    // 3. Already-saved real name on reconnect
                    // 4. raw_name from history sync / push_name / phone fallback
                    let contact_name = s.contact_names.get(&chat_id).cloned().or_else(|| {
                        if !pn_jid.is_empty() {
                            s.contact_names.get(&pn_jid).cloned()
                        } else {
                            None
                        }
                    });

                    if let Some(cn) = contact_name {
                        cn
                    } else if let Some(existing) = s.chats.iter().find(|c| c.id == chat_id) {
                        let n = &existing.name;
                        let looks_raw = n.contains('@')
                            || (n.chars().all(|c| c.is_ascii_digit() || c == '+') && n.len() > 4);
                        if looks_raw { raw_name } else { n.clone() }
                    } else {
                        raw_name
                    }
                };

                log::debug!("JoinedGroup {chat_id}: name={name:?} pn_jid={pn_jid:?}");

                // Store LID→phone mapping from every JoinedGroup conversation
                if chat_id.ends_with("@lid") && !pn_jid.is_empty() {
                    let mut s = state.lock().unwrap();
                    s.insert_lid_phone(chat_id.clone(), pn_jid.clone());
                    // Also store contact name under phone JID if we have one for the LID
                    if let Some(lid_name) = s.contact_names.get(&chat_id).cloned() {
                        if !s.contact_names.contains_key(&pn_jid) {
                            s.contact_names.insert(pn_jid.clone(), lid_name);
                        }
                    }
                }
                // Extract push_names from history sync messages for group participants
                {
                    let mut s = state.lock().unwrap();
                    for h in &conv.messages {
                        if let Some(web_msg) = h.message.as_ref() {
                            if let Some(push_name) = &web_msg.push_name {
                                if !push_name.is_empty() {
                                    // Get the sender JID from key.participant (group) or key.remote_jid (DM)
                                    let sender = web_msg
                                        .key
                                        .participant
                                        .as_deref()
                                        .filter(|p| !p.is_empty())
                                        .unwrap_or("");
                                    if !sender.is_empty() && !s.contact_names.contains_key(sender) {
                                        s.contact_names
                                            .insert(sender.to_string(), push_name.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                for h in &conv.messages {
                    if let Some(web_msg) = h.message.as_ref() {
                        if let Some(participant) = &web_msg.key.participant {
                            if participant.ends_with("@lid") {}
                        }
                    }
                }

                let mut sync_messages: Vec<IncomingMessage> = conv
                    .messages
                    .iter()
                    .filter_map(|h| map_history_message(h, &chat_id))
                    .collect();

                // Resolve sender names for any messages with empty names
                {
                    let s = state.lock().unwrap();
                    for m in &mut sync_messages {
                        if m.sender_name.is_empty() && !m.sender_id.is_empty() && !m.is_from_me {
                            m.sender_name = resolve_sender_name(&s, &m.sender_id);
                        }
                    }
                }

                let conv_timestamp = conv
                    .last_msg_timestamp
                    .or(conv.conversation_timestamp)
                    .unwrap_or(0) as i64;

                // Merge sync messages with disk data. Track which sync messages
                // are NEW (not already on disk/cache) for UI notification.
                //
                // CRITICAL: always merge against the current state (cache OR disk),
                // never silently drop sync messages. Previous code had a TOCTOU race
                // where chat eviction between cache check and lock acquisition caused
                // sync messages to be lost — the most common cause of "messages
                // missing after reboot" in groups with many chats.
                let (last_message, best_timestamp, new_messages) = {
                    // Try cache path first, fall back to disk path if the chat was
                    // evicted between check and use.
                    let cache_result = {
                        let mut s = state.lock().unwrap();
                        if let Some(history) = s.history.get_mut(&chat_id) {
                            let mut new_msgs = Vec::new();
                            for m in &sync_messages {
                                if let Some(existing) = history.iter_mut().find(|x| x.id == m.id) {
                                    if !m.reactions.is_empty() && existing.reactions.is_empty() {
                                        existing.reactions = m.reactions.clone();
                                    }
                                } else {
                                    new_msgs.push(m.clone());
                                    history.push(m.clone());
                                }
                            }
                            history.sort_by_key(|m| m.timestamp);
                            let last_msg = history.last();
                            let preview = last_msg.map(|m| media_preview(m)).unwrap_or_default();
                            let ts = last_msg.map(|m| m.timestamp).unwrap_or(conv_timestamp);
                            s.queue_save_messages(&chat_id);
                            Some((preview, ts, new_msgs))
                        } else {
                            None
                        }
                    };

                    if let Some(result) = cache_result {
                        result
                    } else {
                        // Not in cache — load from disk, merge, and save back.
                        // This is the safe fallback path that also handles the
                        // TOCTOU race (chat evicted between cache check and use).
                        let mut disk_msgs = load_messages(&chat_id);
                        let mut new_msgs = Vec::new();
                        for m in &sync_messages {
                            if let Some(existing) = disk_msgs.iter_mut().find(|x| x.id == m.id) {
                                if !m.reactions.is_empty() && existing.reactions.is_empty() {
                                    existing.reactions = m.reactions.clone();
                                }
                            } else {
                                new_msgs.push(m.clone());
                                disk_msgs.push(m.clone());
                            }
                        }
                        disk_msgs.sort_by_key(|m| m.timestamp);
                        let last_msg = disk_msgs.last();
                        let preview = last_msg.map(|m| media_preview(m)).unwrap_or_default();
                        let ts = last_msg.map(|m| m.timestamp).unwrap_or(conv_timestamp);
                        save_messages(&chat_id, &disk_msgs);
                        (preview, ts, new_msgs)
                    }
                };

                // Push new sync messages to the UI as live messages
                // (so self-messages from other devices and missed messages appear immediately)
                for m in &new_messages {
                    let _ = tx.send(WaEvent::MessageReceived(m.clone())).await;
                }

                // Respect the server's unread count when present.
                // When absent (None), preserve the existing count if the chat
                // is already known, otherwise count incoming non-from-me messages
                // so fresh syncs don't incorrectly mark everything as read.
                let unread_count = match conv.unread_count {
                    Some(n) => n,
                    None => {
                        let existing = state
                            .lock()
                            .unwrap()
                            .chats
                            .iter()
                            .find(|c| c.id == chat_id)
                            .map(|c| c.unread_count);
                        existing.unwrap_or_else(|| {
                            new_messages.iter().filter(|m| !m.is_from_me).count() as u32
                        })
                    }
                };

                let summary = ChatSummary {
                    id: chat_id.clone(),
                    name: name.clone(),
                    last_message,
                    timestamp: best_timestamp,
                    unread_count,
                    is_group: chat_id.ends_with("@g.us"),
                    is_muted: conv.mute_end_time.map(|t| t > 0).unwrap_or(false),
                    is_pinned: conv.pinned.map(|p| p > 0).unwrap_or(false),
                    is_archived: false,
                    is_favorite: false,
                    label: None,
                    pinned_msg_id: None,
                };

                persist_chat(state, summary);

                // Suppress @lid duplicates: if this is an @lid chat whose phone
                // JID already exists, or vice versa, don't send both to the UI.
                {
                    let s = state.lock().unwrap();
                    if chat_id.ends_with("@lid") {
                        if let Some(phone) = s.lid_to_phone.get(&chat_id) {
                            if s.chats.iter().any(|c| c.id == *phone) {
                                // Phone JID chat exists — suppress this @lid entry
                                return;
                            }
                        }
                    }
                }

                // Read back the PERSISTED summary — upsert_chat preserves user-set
                // flags (pin, mute, etc.) that history sync data would overwrite.
                let persisted = state
                    .lock()
                    .unwrap()
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .cloned();
                match persisted {
                    Some(s) => WaEvent::ChatAdded(s),
                    None => return,
                }
            } else {
                return;
            }
        }

        Event::OfflineSyncPreview(preview) => {
            // Only show the spinner if there's actually offline content to process
            if preview.total > 0 || preview.messages > 0 {
                let _ = tx.send(WaEvent::SyncProgress(true)).await;
            }
            return;
        }

        Event::OfflineSyncCompleted(_) => {
            // Post-sync integrity: verify poll votes survived the sync merge
            {
                let s = state.lock().unwrap();
                let mut total = 0u32;
                let mut with_votes = 0u32;
                for msgs in s.history.values() {
                    for m in msgs {
                        if m.poll_question.is_some() {
                            total += 1;
                            if !m.poll_votes.is_empty() {
                                with_votes += 1;
                            }
                        }
                    }
                }
                if total > 0 {
                    log::info!(
                        "POST-SYNC INTEGRITY: {with_votes}/{total} polls have votes in memory cache"
                    );
                }
                // Also check pin state
                let pinned: Vec<_> = s
                    .chats
                    .iter()
                    .filter(|c| c.is_pinned)
                    .map(|c| c.id.clone())
                    .collect();
                log::info!("POST-SYNC: {} pinned chats: {:?}", pinned.len(), pinned);
            }
            // Flush newly learned contact names to disk
            {
                let names = state.lock().unwrap().contact_names.clone();
                std::thread::spawn(move || save_contact_names(&names));
            }
            // Send a full chat list refresh now that sync is complete —
            // the initial ChatsLoaded may have been sent before all chats arrived.
            {
                let fresh_chats: Vec<ChatSummary> = state
                    .lock()
                    .unwrap()
                    .chats_with_best_names()
                    .into_iter()
                    .filter(|c| !c.id.contains("@broadcast"))
                    .collect();
                log::info!("POST-SYNC: refreshing UI with {} chats", fresh_chats.len());
                let _ = tx.send(WaEvent::ChatsLoaded(fresh_chats)).await;
            }
            let _ = tx.send(WaEvent::SyncProgress(false)).await;
            // Sync is done — now safe to fetch profile pictures without disrupting messages.
            let client_clone = client.clone();
            let state_clone = state.clone();
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                fetch_profile_pictures(&client_clone, &state_clone, &tx_clone).await;
            });
            return;
        }

        Event::PushNameUpdate(update) => {
            let chat_id = update.jid.to_string();
            let name = update.new_push_name.clone();
            if !name.is_empty() {
                // Store in contact_names (persisted) so it survives restarts,
                // and rename any already-loaded chat.
                let names_snapshot = {
                    let mut s = state.lock().unwrap();
                    s.record_contact_name(&chat_id, &name, None);
                    s.contact_names.clone()
                };
                tokio::task::spawn_blocking(move || save_contact_names(&names_snapshot));
                WaEvent::ChatNameUpdated { chat_id, name }
            } else {
                return;
            }
        }

        Event::ContactUpdate(ContactUpdate { jid, action, .. }) => {
            // App-state contact sync — carries the name from the user's phonebook.
            // highest-priority name source (same as what WA Web shows).
            //
            // Critical: WA uses LID JIDs in app-state. action.pn_jid holds the
            // corresponding phone JID. Store the name under BOTH so lookups succeed
            // regardless of which JID format the conversation uses.
            let lid_jid = jid.to_string();
            let phone_jid = action.pn_jid.as_deref().unwrap_or("").to_string();

            let name = action
                .full_name
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| action.first_name.clone().filter(|n| !n.is_empty()))
                .unwrap_or_default();
            if name.is_empty() {
                return;
            }

            log::debug!("ContactUpdate: lid={lid_jid} phone={phone_jid} name={name}");

            // Store the phonebook name under ALL known JID variants so
            // lookups succeed whether the chat is keyed by LID, phone, or
            // any device-suffix variant. This OVERWRITES existing entries
            // (including push_names we captured from messages) because the
            // phonebook name from the user's contacts should always win.
            let (names_snapshot, resolved_phone) = {
                let mut s = state.lock().unwrap();
                // 1. Primary LID
                s.record_contact_name(&lid_jid, &name, None);
                // 2. Explicit phone JID from ContactAction
                let mut phone = phone_jid.clone();
                if !phone.is_empty() {
                    s.record_contact_name(&phone, &name, None);
                    if lid_jid.ends_with("@lid") {
                        s.insert_lid_phone(lid_jid.clone(), phone.clone());
                    }
                }
                // 3. Phone derived from lid_to_phone mapping (for LidContactAction
                //    which has no pn_jid field — previously we stored only under
                //    LID, leaving phone-JID chats unresolved)
                if phone.is_empty() && lid_jid.ends_with("@lid") {
                    if let Some(mapped) = s.lid_to_phone.get(&lid_jid).cloned() {
                        s.record_contact_name(&mapped, &name, None);
                        phone = mapped;
                    }
                }
                // 4. Also strip :device suffix and store under non-AD variants
                //    (app-state JIDs sometimes carry device, chat JIDs often don't)
                let strip_dev = |j: &str| -> String {
                    match (j.find(':'), j.find('@')) {
                        (Some(c), Some(a)) if c < a => format!("{}{}", &j[..c], &j[a..]),
                        _ => j.to_string(),
                    }
                };
                let lid_base = strip_dev(&lid_jid);
                if lid_base != lid_jid {
                    s.record_contact_name(&lid_base, &name, None);
                }
                if !phone.is_empty() {
                    let phone_base = strip_dev(&phone);
                    if phone_base != phone {
                        s.record_contact_name(&phone_base, &name, None);
                    }
                }
                (s.contact_names.clone(), phone)
            };
            tokio::task::spawn_blocking(move || save_contact_names(&names_snapshot));

            // Notify the UI — use the phone JID if available (more likely to match a chat row)
            let chat_id = if resolved_phone.is_empty() {
                lid_jid
            } else {
                resolved_phone
            };
            WaEvent::ChatNameUpdated { chat_id, name }
        }

        Event::QuickReplyUpdate(update) => {
            // On first quick reply from a full sync, signal the UI to clear defaults
            static QR_SYNC_STARTED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if update.from_full_sync
                && !QR_SYNC_STARTED.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                // Send empty list first to clear defaults
                let _ = tx
                    .send(WaEvent::QuickRepliesSynced { replies: vec![] })
                    .await;
            }

            let act = &update.action;
            let deleted = act.deleted.unwrap_or(false);
            if !deleted {
                if let (Some(shortcut), Some(message)) = (&act.shortcut, &act.message) {
                    let reply = crate::bridge::QuickReplyData {
                        shortcut: shortcut.clone(),
                        message: message.clone(),
                        keywords: act.keywords.clone(),
                    };
                    log::info!("QuickReply synced: /{shortcut}");
                    let _ = tx
                        .send(WaEvent::QuickRepliesSynced {
                            replies: vec![reply],
                        })
                        .await;
                }
            }
            return;
        }

        // App state sync: another device pinned/unpinned a chat
        Event::PinUpdate(update) => {
            let raw_jid = update.jid.to_string();
            let pinned = update.action.pinned.unwrap_or(false);
            let stripped = if let (Some(colon), Some(at)) = (raw_jid.find(':'), raw_jid.find('@')) {
                if colon < at {
                    format!("{}{}", &raw_jid[..colon], &raw_jid[at..])
                } else {
                    raw_jid.clone()
                }
            } else {
                raw_jid.clone()
            };
            let chat_id = {
                let s = state.lock().unwrap();
                if stripped.ends_with("@lid") {
                    s.lid_to_phone.get(&stripped).cloned().unwrap_or(stripped)
                } else {
                    stripped
                }
            };
            log::info!(
                "PinUpdate: {chat_id} pinned={pinned} full_sync={}",
                update.from_full_sync
            );
            // Full sync: skip entirely — local disk state is authoritative.
            // Only incremental updates (real-time changes from other devices) modify pins.
            if update.from_full_sync {
                return;
            }
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    if c.is_pinned != pinned {
                        log::info!(
                            "PinUpdate: changing {} from {} to {}",
                            chat_id,
                            c.is_pinned,
                            pinned
                        );
                        c.is_pinned = pinned;
                        let _ = s.save_tx.send(s.chats.clone());
                    }
                }
            }
            let _ = tx.send(WaEvent::ChatPinned { chat_id, pinned }).await;
            return;
        }

        // ── Group participant changes → system message + member list refresh ──
        Event::GroupUpdate(update) => {
            use wacore::stanza::groups::GroupNotificationAction;

            let chat_id = {
                let raw = update.group_jid.to_string();
                let s = state.lock().unwrap();
                if raw.ends_with("@lid") {
                    s.lid_to_phone.get(&raw).cloned().unwrap_or(raw)
                } else {
                    raw
                }
            };

            // Build a human-readable system message
            let resolve = |jid: &Jid| -> String {
                let raw = jid.to_string();
                let s = state.lock().unwrap();
                let resolved = if raw.ends_with("@lid") {
                    s.lid_to_phone.get(&raw).cloned().unwrap_or(raw.clone())
                } else {
                    raw.clone()
                };
                resolve_sender_name(&s, &resolved)
            };

            let text = match &update.action {
                GroupNotificationAction::Add { participants, .. } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    format!("Added {}", names.join(", "))
                }
                GroupNotificationAction::Remove { participants, .. } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    format!("{} left", names.join(", "))
                }
                GroupNotificationAction::Promote { participants } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    format!("{} is now an admin", names.join(", "))
                }
                GroupNotificationAction::Demote { participants } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    format!("{} is no longer an admin", names.join(", "))
                }
                GroupNotificationAction::Modify { .. } => "Group info was updated".to_string(),
                _ => {
                    log::debug!("Unhandled group notification action");
                    return;
                }
            };

            log::info!("GroupUpdate: {text} in {chat_id}");

            let now = update.timestamp.timestamp();
            let sys_msg = IncomingMessage {
                id: format!("sys_{now}_{}", wacore::time::now_millis()),
                chat_id: chat_id.clone(),
                sender_id: String::new(),
                sender_name: String::new(),
                text: Some(text),
                media_type: None,
                timestamp: now,
                is_from_me: false,
                is_forwarded: false,
                forwarding_score: 0,
                quoted_msg_id: None,
                quoted_text: None,
                quoted_sender: None,
                quoted_media_path: None,
                reactions: vec![],
                media_local_path: None,
                media_filename: None,
                media_caption: None,
                contact_name: None,
                contact_vcard: None,
                link_title: None,
                link_description: None,
                link_url: None,
                link_thumbnail_path: None,
                poll_question: None,
                poll_options: vec![],
                poll_selectable: 0,
                poll_secret: vec![],
                poll_votes: vec![],
                receipt_status: ReceiptStatus::Sent,
                is_edited: false,
                is_system_message: true,
            };

            // Persist and send to UI
            {
                let mut s = state.lock().unwrap();
                if let Some(msgs) = s.history.get_mut(&chat_id) {
                    msgs.push(sys_msg.clone());
                    s.queue_save_messages(&chat_id);
                }
            }
            let _ = tx.send(WaEvent::MessageReceived(sys_msg)).await;

            // Refresh the member list by requesting fresh group info
            let c = client.clone();
            let t = tx.clone();
            let s = state.clone();
            let cid = chat_id.clone();
            tokio::spawn(async move {
                let jid: Jid = match cid.parse() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                if let Ok(meta) = c.groups().get_metadata(&jid).await {
                    // First pass: resolve what we can from cache
                    let mut members: Vec<crate::bridge::GroupMember> = meta
                        .participants
                        .iter()
                        .map(|p| {
                            let st = s.lock().unwrap();
                            let pjid = p.jid.to_string();
                            let name = resolve_sender_name(&st, &pjid);
                            crate::bridge::GroupMember {
                                jid: pjid,
                                name,
                                is_admin: p.is_admin,
                            }
                        })
                        .collect();

                    // Send initial (partially resolved) list immediately
                    let _ = t
                        .send(WaEvent::GroupMembers {
                            chat_id: cid.clone(),
                            members: members.clone(),
                        })
                        .await;

                    // Collect unresolved LID members for usync
                    let unresolved: Vec<String> = members
                        .iter()
                        .filter(|m| m.name.contains("@lid"))
                        .map(|m| m.jid.clone())
                        .collect();

                    if !unresolved.is_empty() {
                        log::info!(
                            "GroupMembers {cid}: {} unresolved LID participants, triggering usync",
                            unresolved.len()
                        );
                        let jids: Vec<Jid> = unresolved
                            .iter()
                            .filter_map(|lid| lid.parse::<Jid>().ok())
                            .collect();
                        if let Ok(_) = c.get_user_devices(&jids).await {
                            let mut newly_resolved = 0u32;
                            for lid_str in &unresolved {
                                if let Some(phone_jid) =
                                    c.resolve_lid_to_phone_jid(lid_str).await
                                {
                                    let mut st = s.lock().unwrap();
                                    st.insert_lid_phone(lid_str.clone(), phone_jid.clone());
                                    newly_resolved += 1;
                                }
                            }
                            if newly_resolved > 0 {
                                // Persist and re-resolve
                                {
                                    let map = s.lock().unwrap().lid_to_phone.clone();
                                    tokio::task::spawn_blocking(move || save_lid_phone_map(&map));
                                }
                                // Re-resolve all members with updated mappings
                                for m in &mut members {
                                    if m.name.contains("@lid") || m.name.contains("@s.whatsapp.net") {
                                        let st = s.lock().unwrap();
                                        m.name = resolve_sender_name(&st, &m.jid);
                                    }
                                }
                                log::info!(
                                    "GroupMembers {cid}: resolved {newly_resolved}/{} via usync",
                                    unresolved.len()
                                );
                                let _ = t
                                    .send(WaEvent::GroupMembers {
                                        chat_id: cid,
                                        members,
                                    })
                                    .await;
                            }
                        }
                    }
                }
            });
            return;
        }

        _ => return,
    };

    let _ = tx.send(ev).await;
}

/// Persist an incoming live message to disk and update the chat summary.
/// Returns `Some((chat_id, name))` if a new/better contact name was discovered.
/// Returns: (optional name_update, optional new_chat_summary)
fn persist_new_message(
    m: &IncomingMessage,
    state: &Arc<Mutex<RuntimeState>>,
) -> (Option<(String, String)>, Option<ChatSummary>) {
    let chat_id = m.chat_id.clone();
    let is_group = chat_id.ends_with("@g.us");
    let was_new = !state.lock().unwrap().chats.iter().any(|c| c.id == chat_id);

    // Update in-memory cache (fast) and queue async disk write (non-blocking).
    // NEVER do disk I/O while holding the Mutex.
    let (
        existing_name,
        existing_unread,
        existing_is_muted,
        existing_is_pinned,
        existing_is_archived,
        existing_is_favorite,
        existing_label,
    ) = {
        let mut s = state.lock().unwrap();

        // If cache doesn't have this chat yet, load from disk first to avoid
        // overwriting the full history with just the new message.
        if !s.history.contains_key(&chat_id) {
            let disk_msgs = load_messages(&chat_id);
            s.history.insert(chat_id.clone(), disk_msgs);
        }
        s.touch_history(&chat_id);
        s.evict_old_histories();

        let history = s.history.entry(chat_id.clone()).or_default();
        if !history.iter().any(|x| x.id == m.id) {
            history.push(m.clone());
            history.sort_by_key(|msg| msg.timestamp);
        }

        // Queue disk write on background thread — non-blocking
        s.queue_save_messages(&chat_id);

        // Read chat info
        if let Some(ex) = s.chats.iter().find(|c| c.id == chat_id) {
            (
                Some(ex.name.clone()),
                ex.unread_count,
                ex.is_muted,
                ex.is_pinned,
                ex.is_archived,
                ex.is_favorite,
                ex.label.clone(),
            )
        } else {
            (None, 0, false, false, false, false, None)
        }
    };

    // Resolve display name
    let (resolved_name, name_is_new) = if let Some(ref existing_name) = existing_name {
        let name_looks_like_jid = existing_name.contains('@')
            || (existing_name.chars().all(|c| c.is_ascii_digit()) && existing_name.len() > 6);
        if !is_group && name_looks_like_jid && !m.is_from_me && !m.sender_name.is_empty() {
            (m.sender_name.clone(), true)
        } else if !is_group && name_looks_like_jid {
            let formatted = display_name_from_jid(&chat_id);
            (formatted.clone(), formatted != *existing_name)
        } else {
            (existing_name.clone(), false)
        }
    } else {
        let name = if !is_group && !m.is_from_me && !m.sender_name.is_empty() {
            m.sender_name.clone()
        } else {
            display_name_from_jid(&chat_id)
        };
        (name, false)
    };

    let preview = media_preview(m);
    let summary = ChatSummary {
        id: chat_id.clone(),
        name: resolved_name.clone(),
        last_message: preview,
        timestamp: m.timestamp,
        unread_count: existing_unread,
        is_group,
        is_muted: existing_is_muted,
        is_pinned: existing_is_pinned,
        is_archived: existing_is_archived,
        is_favorite: existing_is_favorite,
        label: existing_label,
        pinned_msg_id: None,
    };
    persist_chat(state, summary);

    if name_is_new && !is_group {
        let names_snapshot = {
            let mut s = state.lock().unwrap();
            s.record_contact_name(&chat_id, &resolved_name, None);
            s.contact_names.clone()
        };
        // Disk write on background — don't block the async runtime
        std::thread::spawn(move || save_contact_names(&names_snapshot));
    }

    let name_update = if name_is_new {
        Some((chat_id.clone(), resolved_name))
    } else {
        None
    };
    let new_chat = if was_new {
        state
            .lock()
            .unwrap()
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .cloned()
    } else {
        None
    };
    (name_update, new_chat)
}

fn media_preview(m: &IncomingMessage) -> String {
    if let Some(t) = &m.text {
        return t.clone();
    }
    match &m.media_type {
        Some(crate::bridge::MediaType::Image) => "📷 Photo".to_string(),
        Some(crate::bridge::MediaType::Video) => "🎥 Video".to_string(),
        Some(crate::bridge::MediaType::Audio) => "🎵 Audio".to_string(),
        Some(crate::bridge::MediaType::Document) => m
            .media_filename
            .as_deref()
            .map(|f| format!("📄 {f}"))
            .unwrap_or_else(|| "📄 Document".to_string()),
        Some(crate::bridge::MediaType::Sticker) => "🎭 Sticker".to_string(),
        Some(crate::bridge::MediaType::Gif) => "🎞 GIF".to_string(),
        None => "📎 Message".to_string(),
    }
}

// ── Command handler ───────────────────────────────────────────────────────────

async fn handle_command(
    client: &Arc<Client>,
    tx: &Sender<WaEvent>,
    state: &Arc<Mutex<RuntimeState>>,
    cmd: WaCommand,
) -> Result<()> {
    match cmd {
        WaCommand::SendText {
            chat_id,
            text,
            tmp_id,
            mentioned_jids,
        } => {
            let jid: Jid = chat_id.parse()?;

            // Check if text contains a URL — fetch OpenGraph preview
            let link_url = text
                .split_whitespace()
                .find(|w| w.starts_with("http://") || w.starts_with("https://"))
                .map(|s| s.to_string());
            let (og_title, og_desc, og_thumb) = if let Some(ref url) = link_url {
                let u = url.clone();
                tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    // Helper to fetch JSON from a URL
                    let fetch_json = |url: &str| -> Option<serde_json::Value> {
                        ureq::get(url)
                            .call()
                            .ok()
                            .and_then(|r| r.into_string().ok())
                            .and_then(|s| serde_json::from_str(&s).ok())
                    };

                    // 1. Try site-specific oEmbed (YouTube, Vimeo, etc.)
                    let oembed_url = if u.contains("youtube.com") || u.contains("youtu.be") {
                        Some(format!(
                            "https://www.youtube.com/oembed?url={}&format=json",
                            u
                        ))
                    } else if u.contains("vimeo.com") {
                        Some(format!("https://vimeo.com/api/oembed.json?url={}", u))
                    } else if u.contains("twitter.com") || u.contains("x.com") {
                        Some(format!("https://publish.twitter.com/oembed?url={}", u))
                    } else {
                        None
                    };

                    if let Some(oembed) = oembed_url {
                        if let Some(json) = fetch_json(&oembed) {
                            let title = json
                                .get("title")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let desc = json
                                .get("author_name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let thumb = json
                                .get("thumbnail_url")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            if title.is_some() {
                                log::info!("oEmbed OK: title={title:?} thumb={thumb:?}");
                                return (title, desc, thumb);
                            }
                        }
                    }

                    // 2. Try noembed.com (supports many providers)
                    if let Some(json) = fetch_json(&format!("https://noembed.com/embed?url={}", u))
                    {
                        if json.get("error").is_none() {
                            let title = json
                                .get("title")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let desc = json
                                .get("author_name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let thumb = json
                                .get("thumbnail_url")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            if title.is_some() {
                                log::info!("noembed OK: title={title:?}");
                                return (title, desc, thumb);
                            }
                        }
                    }

                    // 3. Scrape with Facebook crawler UA (most sites serve OG to this)
                    let req = ureq::get(&u).set(
                        "User-Agent",
                        "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)",
                    );
                    match req.call() {
                        Ok(resp) => {
                            let mut bytes = Vec::new();
                            resp.into_reader()
                                .take(200_000)
                                .read_to_end(&mut bytes)
                                .ok();
                            let body = String::from_utf8_lossy(&bytes);
                            let extract = |tag: &str| -> Option<String> {
                                // Search for property="og:X" content="..." in any order
                                if let Some(pos) = body.find(tag) {
                                    let region =
                                        &body[pos.saturating_sub(100)..body.len().min(pos + 300)];
                                    if let Some(c) = region.find("content=\"") {
                                        let start = c + 9;
                                        if let Some(end) = region[start..].find('"') {
                                            let val = &region[start..start + end];
                                            if !val.is_empty() {
                                                return Some(val.to_string());
                                            }
                                        }
                                    }
                                }
                                None
                            };
                            let title = extract("og:title").or_else(|| {
                                body.split("<title>")
                                    .nth(1)
                                    .and_then(|s| s.split("</title>").next())
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                            });
                            let desc = extract("og:description");
                            let thumb = extract("og:image");
                            log::info!("OG scrape: title={title:?}");
                            (title, desc, thumb)
                        }
                        Err(e) => {
                            log::warn!("OG fetch failed: {e}");
                            (None, None, None)
                        }
                    }
                })
                .await
                .unwrap_or((None, None, None))
            } else {
                (None, None, None)
            };

            let og_title_clone = og_title.clone();
            let og_desc_clone = og_desc.clone();
            let link_url_clone = link_url.clone();
            let og_thumb_clone = og_thumb.clone();
            let has_preview = og_title.is_some() || link_url.is_some();
            let msg = if !mentioned_jids.is_empty() || has_preview {
                // Use ExtendedTextMessage for mentions or link previews
                let mut ctx = wa::ContextInfo {
                    mentioned_jid: mentioned_jids,
                    ..Default::default()
                };
                wa::Message {
                    extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                        text: Some(text.clone()),
                        context_info: if ctx.mentioned_jid.is_empty() && !has_preview {
                            None
                        } else {
                            Some(Box::new(ctx))
                        },
                        matched_text: link_url.clone(),
                        title: og_title,
                        description: og_desc,
                        ..Default::default()
                    })),
                    ..Default::default()
                }
            } else {
                wa::Message {
                    conversation: Some(text.clone()),
                    ..Default::default()
                }
            };
            match client.send_message(jid, msg).await {
                Ok(real_id) => {
                    // Persist sent message to cache so it survives chat switches
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let sent_msg = IncomingMessage {
                        id: real_id.clone(),
                        chat_id: chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: Some(text),
                        media_type: None,
                        timestamp: now,
                        is_from_me: true,
                        is_forwarded: false,
                        forwarding_score: 0,
                        quoted_msg_id: None,
                        quoted_text: None,
                        quoted_sender: None,
                        reactions: vec![],
                        media_local_path: None,
                        media_filename: None,
                        media_caption: None,
                        contact_name: None,
                        contact_vcard: None,
                        link_title: og_title_clone,
                        link_description: og_desc_clone,
                        link_url: link_url_clone,
                        link_thumbnail_path: og_thumb_clone,
                        receipt_status: ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                        quoted_media_path: None,
                        poll_question: None,
                        poll_options: vec![],
                        poll_selectable: 0,
                        poll_secret: vec![],
                        poll_votes: vec![],
                    };
                    let (_, new_chat) = persist_new_message(&sent_msg, state);
                    if let Some(s) = new_chat {
                        let _ = tx.send(WaEvent::ChatAdded(s)).await;
                    }
                    // Notify UI: confirm the bubble + update chat list preview
                    let _ = tx
                        .send(WaEvent::MessageConfirmed {
                            tmp_id,
                            real_id: real_id.clone(),
                            chat_id: chat_id.clone(),
                        })
                        .await;
                    let _ = tx.send(WaEvent::MessageReceived(sent_msg)).await;
                }
                Err(e) => {
                    log::warn!("SendText failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                }
            }
        }

        WaCommand::SendReply {
            chat_id,
            text,
            quoted_msg_id,
            quoted_sender,
            tmp_id,
            mentioned_jids,
        } => {
            let jid: Jid = chat_id.parse()?;
            // quoted_sender may be empty (replying to own message) — use own JID as fallback
            let sender_jid: Jid =
                if quoted_sender.is_empty() || quoted_sender.parse::<Jid>().is_err() {
                    match client.get_pn().await {
                        Some(j) => j,
                        None => chat_id.parse()?, // last resort
                    }
                } else {
                    quoted_sender.parse()?
                };

            // Look up the original message text from cache to include in the quote
            let quoted_text = {
                let s = state.lock().unwrap();
                s.history
                    .get(&chat_id)
                    .and_then(|msgs| msgs.iter().find(|m| m.id == quoted_msg_id))
                    .and_then(|m| m.text.clone().or_else(|| m.media_caption.clone()))
                    .unwrap_or_default()
            };
            let quoted_text_for_msg = quoted_text.clone();
            let quoted_message = wa::Message {
                conversation: if quoted_text_for_msg.is_empty() {
                    None
                } else {
                    Some(quoted_text_for_msg)
                },
                ..Default::default()
            };

            let mut ctx = whatsapp_rust::proto_helpers::build_quote_context_with_info(
                &quoted_msg_id,
                &sender_jid,
                &jid,
                &quoted_message,
            );
            // Merge mentions into the quote context
            if !mentioned_jids.is_empty() {
                ctx.mentioned_jid = mentioned_jids;
            }

            let msg = wa::Message {
                extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                    text: Some(text.clone()),
                    context_info: Some(Box::new(ctx)),
                    ..Default::default()
                })),
                ..Default::default()
            };
            match client.send_message(jid, msg).await {
                Ok(real_id) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let sent_msg = IncomingMessage {
                        id: real_id.clone(),
                        chat_id: chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: Some(text),
                        media_type: None,
                        timestamp: now,
                        is_from_me: true,
                        is_forwarded: false,
                        forwarding_score: 0,
                        quoted_msg_id: Some(quoted_msg_id.clone()),
                        quoted_text: Some(quoted_text.clone()),
                        quoted_sender: Some(quoted_sender.clone()),
                        reactions: vec![],
                        media_local_path: None,
                        media_filename: None,
                        media_caption: None,
                        contact_name: None,
                        contact_vcard: None,
                        link_title: None,
                        link_description: None,
                        link_url: None,
                        link_thumbnail_path: None,
                        receipt_status: ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                        poll_question: None,
                        poll_options: vec![],
                        poll_selectable: 0,
                        poll_secret: vec![],
                        poll_votes: vec![],
                        quoted_media_path: None,
                    };
                    persist_new_message(&sent_msg, state);
                    let _ = tx
                        .send(WaEvent::MessageConfirmed {
                            tmp_id,
                            real_id: real_id.clone(),
                            chat_id: chat_id.clone(),
                        })
                        .await;
                    let _ = tx.send(WaEvent::MessageReceived(sent_msg)).await;
                }
                Err(e) => {
                    log::warn!("SendReply failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                }
            }
        }

        WaCommand::ResendMessage {
            chat_id,
            msg_id,
            text,
        } => {
            let jid: Jid = chat_id.parse()?;
            let msg = wa::Message {
                conversation: Some(text.clone()),
                ..Default::default()
            };
            match client.send_message(jid, msg).await {
                Ok(new_id) => {
                    // Update the receipt on the existing bubble to Sent
                    let _ = tx
                        .send(WaEvent::ReceiptUpdate {
                            msg_id: msg_id.clone(),
                            status: crate::bridge::ReceiptStatus::Sent,
                        })
                        .await;
                    // If the ID changed (failed → real), notify with new ID
                    if new_id != msg_id {
                        let _ = tx
                            .send(WaEvent::ReceiptUpdate {
                                msg_id: new_id,
                                status: crate::bridge::ReceiptStatus::Sent,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    log::warn!("ResendMessage failed: {e:#}");
                    let _ = tx.send(WaEvent::MessageFailed { msg_id, chat_id }).await;
                }
            }
        }

        WaCommand::ForwardMessage {
            to_chat_id,
            original_msg_id,
        } => {
            log::info!("Forward {original_msg_id} → {to_chat_id} (not yet implemented)");
        }

        WaCommand::DeleteForEveryone { chat_id, msg_id } => {
            let jid: Jid = chat_id.parse()?;
            log::info!("DeleteForEveryone: chat={chat_id} msg={msg_id}");
            match client
                .revoke_message(jid, msg_id.clone(), RevokeType::Sender)
                .await
            {
                Ok(_) => {
                    log::info!("DeleteForEveryone: server revoke succeeded for {msg_id}");
                }
                Err(e) => {
                    log::warn!("DeleteForEveryone server error: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!(
                            "Delete for everyone failed: {e}"
                        )))
                        .await;
                }
            }
            // Always remove locally
            {
                let mut s = state.lock().unwrap();
                if let Some(history) = s.history.get_mut(&chat_id) {
                    history.retain(|m| m.id != msg_id);
                    s.queue_save_messages(&chat_id);
                }
            }
            let _ = tx
                .send(WaEvent::MessageDeletedLocal {
                    chat_id: chat_id.clone(),
                    msg_id: msg_id.clone(),
                })
                .await;
        }

        WaCommand::SetTyping { chat_id, is_typing } => {
            let jid: Jid = chat_id.parse()?;
            if is_typing {
                client.chatstate().send_composing(&jid).await?;
            } else {
                client.chatstate().send_paused(&jid).await?;
            }
        }

        WaCommand::LoadChat { chat_id, chat_name } => {
            // Subscribe to presence for this chat (enables typing notifications)
            if let Ok(jid) = chat_id.parse::<Jid>() {
                if let Err(e) = client.presence().subscribe(&jid).await {
                    log::debug!("Presence subscribe failed for {chat_id}: {e:#}");
                }
            }
            // Check in-memory cache first (no disk I/O).
            let cached = {
                let s = state.lock().unwrap();
                s.history.get(&chat_id).cloned()
            };

            let all_messages = if let Some(msgs) = cached {
                state.lock().unwrap().touch_history(&chat_id);
                msgs
            } else {
                // Cache miss — load from disk OFF the async thread
                let cid = chat_id.clone();
                let disk_msgs = tokio::task::spawn_blocking(move || load_messages(&cid))
                    .await
                    .unwrap_or_default();
                // Populate cache for next time + evict old entries
                {
                    let mut s = state.lock().unwrap();
                    s.history.insert(chat_id.clone(), disk_msgs.clone());
                    s.touch_history(&chat_id);
                    s.evict_old_histories();
                }
                disk_msgs
            };

            // Extract push_names from ALL loaded messages into contact_names
            // (before truncation, so names from older messages are available)
            {
                let mut s = state.lock().unwrap();
                let mut learned = 0u32;
                for m in &all_messages {
                    if is_valid_contact_name(&m.sender_name, &m.sender_id)
                        && !m.sender_id.is_empty()
                        && !m.is_from_me
                    {
                        if !s.contact_names.contains_key(&m.sender_id) {
                            s.contact_names
                                .insert(m.sender_id.clone(), m.sender_name.clone());
                            learned += 1;
                        }
                    }
                }
                if learned > 0 {
                    log::info!(
                        "LoadChat {chat_id}: learned {learned} new contact names from message history"
                    );
                }
            }

            // Log poll vote state for debugging
            {
                let polls_with_votes: Vec<_> = all_messages
                    .iter()
                    .filter(|m| m.poll_question.is_some() && !m.poll_votes.is_empty())
                    .map(|m| {
                        format!(
                            "{}({} voters)",
                            &m.id[..8.min(m.id.len())],
                            m.poll_votes.len()
                        )
                    })
                    .collect();
                if !polls_with_votes.is_empty() {
                    log::info!("LoadChat {chat_id}: polls with votes: {polls_with_votes:?}");
                }
            }

            // Filter for display only (doesn't affect persisted data), then take last 100
            let mut all_messages: Vec<IncomingMessage> = all_messages
                .into_iter()
                .filter(|m| m.text.is_some() || m.media_type.is_some() || m.media_caption.is_some())
                .collect();
            all_messages.sort_by_key(|m| m.timestamp);
            // Truncate early — resolve names only for the messages we'll display.
            // 50 messages is enough for initial view; user can scroll up for more.
            if all_messages.len() > 50 {
                all_messages = all_messages.split_off(all_messages.len() - 50);
            }

            // Fix group sender_ids and resolve names (display only)
            // Use a local cache to avoid repeated lookups for the same JID.
            // First, learn push_names from the messages themselves — if sender A
            // has a name in message #5, use it for message #1 where it's missing.
            {
                let s = state.lock().unwrap();
                let mut name_cache: HashMap<String, String> = HashMap::new();
                let resolve_cached =
                    |jid: &str, cache: &mut HashMap<String, String>, s: &RuntimeState| -> String {
                        if let Some(cached) = cache.get(jid) {
                            return cached.clone();
                        }
                        let name = resolve_sender_name(s, jid);
                        cache.insert(jid.to_string(), name.clone());
                        name
                    };
                for m in &mut all_messages {
                    // Fix sender_id = group JID (old sync bug)
                    if m.sender_id == chat_id && chat_id.ends_with("@g.us") {
                        m.sender_id = String::new();
                    }
                    if m.sender_name.is_empty() && !m.sender_id.is_empty() && !m.is_from_me {
                        m.sender_name = resolve_cached(&m.sender_id, &mut name_cache, &s);
                    }
                    // Also resolve quoted sender JID to name
                    if let Some(qs) = &m.quoted_sender {
                        if qs.contains('@') {
                            m.quoted_sender = Some(resolve_cached(qs, &mut name_cache, &s));
                        } else if qs.starts_with('+') || qs.chars().all(|c| c.is_ascii_digit()) {
                            // Raw phone number — try constructing JID variants
                            let num = qs.trim_start_matches('+');
                            let phone_jid = format!("{num}@s.whatsapp.net");
                            let resolved = resolve_cached(&phone_jid, &mut name_cache, &s);
                            if resolved != phone_jid {
                                m.quoted_sender = Some(resolved);
                            }
                        }
                    }
                    // Resolve reaction sender JIDs to display names
                    for (sender, _) in &mut m.reactions {
                        if sender.contains('@') {
                            *sender = resolve_cached(sender, &mut name_cache, &s);
                        }
                    }
                    // Resolve @mentions in message text AND quoted_text
                    if let Some(ref text) = m.text {
                        let resolved = resolve_mentions(text, &s);
                        if resolved != *text {
                            m.text = Some(resolved);
                        }
                    }
                    if let Some(ref qt) = m.quoted_text {
                        let resolved = resolve_mentions(qt, &s);
                        if resolved != *qt {
                            m.quoted_text = Some(resolved);
                        }
                    }
                }
            }

            let messages = all_messages;

            // Populate last_msg_id/sender for mark-as-read from loaded history
            if let Some(last) = messages.last() {
                let mut s = state.lock().unwrap();
                s.last_msg_id.insert(chat_id.clone(), last.id.clone());
                if !last.sender_id.is_empty() && !last.is_from_me {
                    s.last_msg_sender
                        .insert(chat_id.clone(), last.sender_id.clone());
                }
            }

            let name = state
                .lock()
                .unwrap()
                .chat_names
                .get(&chat_id)
                .cloned()
                .unwrap_or(chat_name);

            // Restore pinned message banner if one was saved
            let pinned_msg_id = state
                .lock()
                .unwrap()
                .chats
                .iter()
                .find(|c| c.id == chat_id)
                .and_then(|c| c.pinned_msg_id.clone());

            // Update chat list preview with resolved sender name for group chats.
            // The persisted last_message might have a raw JID/phone number as sender
            // prefix because the name wasn't known when it was saved.
            if chat_id.ends_with("@g.us") {
                if let Some(last) = messages.last() {
                    if !last.is_from_me && !last.sender_name.is_empty() {
                        let preview_text = media_preview(last);
                        let preview = format!("{}: {preview_text}", last.sender_name);
                        // Update persisted chat summary too
                        {
                            let mut s = state.lock().unwrap();
                            if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                                if c.last_message != preview {
                                    c.last_message = preview.clone();
                                    let _ = s.save_tx.send(s.chats.clone());
                                }
                            }
                        }
                        let _ = tx
                            .send(WaEvent::ChatPreviewUpdated {
                                chat_id: chat_id.clone(),
                                preview,
                            })
                            .await;
                    }
                }
            }

            // Collect unresolved LID senders for async resolution
            let unresolved_lids: Vec<String> = {
                let s = state.lock().unwrap();
                messages
                    .iter()
                    .filter(|m| {
                        !m.is_from_me
                            && m.sender_id.ends_with("@lid")
                            && (m.sender_name.contains("@lid")
                                || m.sender_name.starts_with('+')
                                || m.sender_name.is_empty())
                    })
                    .map(|m| m.sender_id.clone())
                    .filter(|lid| !s.lid_to_phone.contains_key(lid))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect()
            };

            let _ = tx
                .send(WaEvent::HistoryMessages {
                    chat_id: chat_id.clone(),
                    chat_name: name,
                    messages,
                })
                .await;

            if let Some(msg_id) = pinned_msg_id {
                let _ = tx.send(WaEvent::MessagePinned { chat_id: chat_id.clone(), msg_id }).await;
            }

            // Async LID→phone resolution for unresolved group participant senders.
            // Uses usync (device-list query) which does a network round-trip and
            // persists mappings. After resolving, re-sends updated messages to UI.
            if !unresolved_lids.is_empty() && chat_id.ends_with("@g.us") {
                log::info!(
                    "LoadChat {chat_id}: {} unresolved LID senders, triggering usync resolution",
                    unresolved_lids.len()
                );
                let client_c = client.clone();
                let state_c = state.clone();
                let tx_c = tx.clone();
                let chat_id_c = chat_id.clone();
                tokio::spawn(async move {
                    // Parse LID JIDs and query the server
                    let jids: Vec<Jid> = unresolved_lids
                        .iter()
                        .filter_map(|s| s.parse::<Jid>().ok())
                        .collect();
                    if jids.is_empty() {
                        return;
                    }
                    match client_c.get_user_devices(&jids).await {
                        Ok(_devices) => {
                            // Usync response stored LID→phone mappings in client cache.
                            // Now pull them into our RuntimeState.
                            let mut newly_resolved = 0u32;
                            for lid_str in &unresolved_lids {
                                if let Some(phone_jid) =
                                    client_c.resolve_lid_to_phone_jid(lid_str).await
                                {
                                    let mut s = state_c.lock().unwrap();
                                    s.insert_lid_phone(lid_str.clone(), phone_jid.clone());
                                    if let Some(name) =
                                        s.contact_names.get(lid_str).cloned()
                                    {
                                        if !s.contact_names.contains_key(&phone_jid) {
                                            s.contact_names
                                                .insert(phone_jid.clone(), name);
                                        }
                                    }
                                    newly_resolved += 1;
                                }
                            }
                            if newly_resolved > 0 {
                                log::info!(
                                    "LoadChat {chat_id_c}: resolved {newly_resolved}/{} LID senders via usync",
                                    unresolved_lids.len()
                                );
                                // Persist updated mappings
                                {
                                    let map = state_c.lock().unwrap().lid_to_phone.clone();
                                    tokio::task::spawn_blocking(move || save_lid_phone_map(&map));
                                }

                                // Re-resolve message sender names in cache and resend to UI.
                                // Two-phase: first collect resolutions, then apply.
                                let updated_msgs = {
                                    let mut s = state_c.lock().unwrap();
                                    // Phase 1: collect (jid → resolved name) while &s is immutable
                                    let resolutions: HashMap<String, String> = unresolved_lids
                                        .iter()
                                        .map(|lid| {
                                            let name = resolve_sender_name(&s, lid);
                                            (lid.clone(), name)
                                        })
                                        .collect();
                                    // Phase 2: apply resolved names to cached messages
                                    if let Some(msgs) = s.history.get_mut(&chat_id_c) {
                                        for m in msgs.iter_mut() {
                                            if let Some(resolved) =
                                                resolutions.get(&m.sender_id)
                                            {
                                                if !resolved.contains("@lid")
                                                    && *resolved != m.sender_name
                                                {
                                                    m.sender_name = resolved.clone();
                                                }
                                            }
                                        }
                                    }
                                    // Return the last 50 for display
                                    s.history.get(&chat_id_c).map(|all| {
                                        let mut filtered: Vec<_> = all
                                            .iter()
                                            .filter(|m| {
                                                m.text.is_some()
                                                    || m.media_type.is_some()
                                                    || m.media_caption.is_some()
                                            })
                                            .cloned()
                                            .collect();
                                        filtered.sort_by_key(|m| m.timestamp);
                                        if filtered.len() > 50 {
                                            filtered =
                                                filtered.split_off(filtered.len() - 50);
                                        }
                                        filtered
                                    })
                                };
                                if let Some(messages) = updated_msgs {
                                    let chat_name = state_c
                                        .lock()
                                        .unwrap()
                                        .chat_names
                                        .get(&chat_id_c)
                                        .cloned()
                                        .unwrap_or_default();
                                    let _ = tx_c
                                        .send(WaEvent::HistoryMessages {
                                            chat_id: chat_id_c,
                                            chat_name,
                                            messages,
                                        })
                                        .await;
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "LoadChat {chat_id_c}: usync LID resolution failed: {e:#}"
                            );
                        }
                    }
                });
            }
        }

        WaCommand::MarkRead { chat_id } => {
            // TWO mechanisms needed for cross-device read sync:
            // 1. <receipt type="read"> — blue tick to sender
            // 2. markChatAsRead app state mutation — syncs to our other devices
            let jid: Jid = chat_id.parse()?;

            // Mechanism 2: App state sync (durable, works across device restarts)
            if let Err(e) = client
                .chat_actions()
                .mark_chat_as_read(&jid, true, None)
                .await
            {
                log::debug!("mark_chat_as_read (app state) failed: {e:#}");
            }

            // Mechanism 1: Read receipt to sender
            let (last_id, last_sender) = {
                let s = state.lock().unwrap();
                (
                    s.last_msg_id.get(&chat_id).cloned(),
                    s.last_msg_sender.get(&chat_id).cloned(),
                )
            };
            if let Some(msg_id) = last_id {
                let jid: Jid = chat_id.parse()?;
                // Groups require the sender JID (keep as LID if that's the original format)
                let sender_jid = if chat_id.ends_with("@g.us") {
                    last_sender.clone().and_then(|s| s.parse::<Jid>().ok())
                } else {
                    None
                };
                log::info!("MarkRead: chat={chat_id} msg={msg_id} sender={sender_jid:?}");
                // Try multiple approaches — LID groups need specific format
                let result = client
                    .mark_as_read(&jid, sender_jid.as_ref(), vec![msg_id.clone()])
                    .await;
                if let Err(e) = &result {
                    log::warn!("MarkRead attempt 1 failed: {e:#}");
                    // Retry without sender
                    let _ = client
                        .mark_as_read(&jid, None, vec![msg_id.clone()])
                        .await
                        .map_err(|e2| log::warn!("MarkRead attempt 2 (no sender): {e2:#}"));
                    // Retry with phone JID if sender was LID
                    if let Some(ref sender) = sender_jid {
                        let sender_str = sender.to_string();
                        if sender_str.ends_with("@lid") {
                            let phone_jid_opt = state
                                .lock()
                                .unwrap()
                                .lid_to_phone
                                .get(&sender_str)
                                .cloned()
                                .and_then(|p| p.parse::<Jid>().ok());
                            if let Some(phone_jid) = phone_jid_opt {
                                let _ = client
                                    .mark_as_read(&jid, Some(&phone_jid), vec![msg_id])
                                    .await
                                    .map_err(|e3| log::warn!("MarkRead attempt 3 (phone): {e3:#}"));
                            }
                        }
                    }
                } else {
                    log::info!("MarkRead succeeded for {chat_id}");
                }
                // Reset unread count locally AND notify UI
                {
                    let mut s = state.lock().unwrap();
                    if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                        if c.unread_count > 0 {
                            c.unread_count = 0;
                            let _ = s.save_tx.send(s.chats.clone());
                        }
                    }
                }
                let _ = tx.send(WaEvent::ChatReadOnOtherDevice { chat_id }).await;
            }
        }

        WaCommand::Logout => {
            client.disconnect().await;
        }

        WaCommand::SetProfilePicture { path } => {
            match std::fs::read(&path) {
                Ok(data) => {
                    match client.profile().set_profile_picture(data).await {
                        Ok(_) => log::info!("Profile picture updated from {path}"),
                        Err(e) => log::warn!("Failed to set profile picture: {e:#}"),
                    }
                }
                Err(e) => log::warn!("Failed to read profile image file {path}: {e}"),
            }
        }

        WaCommand::ArchiveChat { chat_id, archived } => {
            let jid: Jid = chat_id.parse()?;
            if archived {
                client.chat_actions().archive_chat(&jid, None).await?;
            } else {
                client.chat_actions().unarchive_chat(&jid, None).await?;
            }
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.is_archived = archived;
                    let _ = s.save_tx.send(s.chats.clone());
                }
            }
            let _ = tx.send(WaEvent::ChatArchived { chat_id, archived }).await;
        }

        WaCommand::MuteChat { chat_id, muted } => {
            let jid: Jid = chat_id.parse()?;
            if muted {
                client.chat_actions().mute_chat(&jid).await?;
            } else {
                client.chat_actions().unmute_chat(&jid).await?;
            }
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.is_muted = muted;
                    let _ = s.save_tx.send(s.chats.clone());
                }
            }
            let _ = tx.send(WaEvent::ChatMuted { chat_id, muted }).await;
        }

        WaCommand::PinChat { chat_id, pinned } => {
            log::info!("PIN_CMD: chat={chat_id} pinned={pinned}");
            let jid: Jid = chat_id.parse()?;
            if pinned {
                client.chat_actions().pin_chat(&jid).await?;
            } else {
                client.chat_actions().unpin_chat(&jid).await?;
            }
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.is_pinned = pinned;
                    log::info!("PIN_CMD: set is_pinned={} for {}", pinned, chat_id);
                } else {
                    log::warn!("PIN_CMD: chat {} NOT FOUND in chats list!", chat_id);
                }
                let _ = s.save_tx.send(s.chats.clone());
            }
            let _ = tx.send(WaEvent::ChatPinned { chat_id, pinned }).await;
        }

        WaCommand::LabelChat { chat_id, label } => {
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.label = label.clone();
                    let _ = s.save_tx.send(s.chats.clone());
                }
            }
            let _ = tx.send(WaEvent::ChatLabeled { chat_id, label }).await;
        }

        WaCommand::MarkUnread { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            client
                .chat_actions()
                .mark_chat_as_read(&jid, false, None)
                .await?;
            let _ = tx.send(WaEvent::ChatMarkedUnread { chat_id }).await;
        }

        WaCommand::FavoriteChat { chat_id, favorite } => {
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.is_favorite = favorite;
                    let _ = s.save_tx.send(s.chats.clone());
                }
            }
            let _ = tx.send(WaEvent::ChatFavorited { chat_id, favorite }).await;
        }

        WaCommand::BlockContact { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            if let Err(e) = client.blocking().block(&jid).await {
                log::warn!("Block {chat_id} failed: {e:#}");
            }
        }

        WaCommand::ClearChat { chat_id } => {
            {
                let mut s = state.lock().unwrap();
                s.history.remove(&chat_id);
            }
            let _ = tokio::task::spawn_blocking({
                let chat_id = chat_id.clone();
                move || {
                    let _ = std::fs::remove_file(messages_file(&chat_id));
                }
            })
            .await;
            let _ = tx.send(WaEvent::ChatCleared { chat_id }).await;
        }

        WaCommand::DeleteChat { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            if let Err(e) = client.chat_actions().delete_chat(&jid, false, None).await {
                log::warn!("Delete chat {chat_id} failed: {e:#}");
            }
            {
                let mut s = state.lock().unwrap();
                s.chats.retain(|c| c.id != chat_id);
                s.history.remove(&chat_id);
                s.chat_names.remove(&chat_id);
                s.last_msg_id.remove(&chat_id);
                let _ = s.save_tx.send(s.chats.clone());
            }
            let _ = tokio::task::spawn_blocking({
                let chat_id = chat_id.clone();
                move || {
                    let _ = std::fs::remove_file(messages_file(&chat_id));
                }
            })
            .await;
            let _ = tx.send(WaEvent::ChatDeleted { chat_id }).await;
        }

        // ── Message actions ───────────────────────────────────────────────────
        WaCommand::SendReaction {
            chat_id,
            msg_id,
            emoji,
            sender_jid,
            is_from_me,
        } => {
            let jid: Jid = chat_id.parse()?;
            let sender: Jid = sender_jid.parse().unwrap_or(jid.clone());
            let key = wa::MessageKey {
                remote_jid: Some(chat_id.clone()),
                id: Some(msg_id.clone()),
                from_me: Some(is_from_me),
                participant: if chat_id.ends_with("@g.us") && !is_from_me {
                    Some(sender.to_string())
                } else {
                    None
                },
            };
            let reaction = wa::Message {
                reaction_message: Some(wa::message::ReactionMessage {
                    key: Some(key),
                    text: Some(emoji.clone()),
                    sender_timestamp_ms: Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64,
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(e) = client.send_message(jid, reaction).await {
                log::warn!("SendReaction failed: {e:#}");
            } else {
                // Persist reaction to cache
                {
                    let own_jid = {
                        let s = state.lock().unwrap();
                        if s.own_lid.is_empty() {
                            s.own_phone.clone()
                        } else {
                            s.own_lid.clone()
                        }
                    };
                    let mut s = state.lock().unwrap();
                    if let Some(msgs) = s.history.get_mut(&chat_id) {
                        if let Some(m) = msgs.iter_mut().find(|m| m.id == msg_id) {
                            m.reactions.retain(|(s, _)| *s != own_jid);
                            m.reactions.push((own_jid, emoji.clone()));
                            s.queue_save_messages(&chat_id);
                        }
                    }
                }
                let _ = tx
                    .send(WaEvent::ReactionUpdated {
                        chat_id,
                        msg_id,
                        emoji,
                    })
                    .await;
            }
        }

        WaCommand::StarMessage {
            chat_id,
            msg_id,
            starred,
            sender_jid,
            is_from_me,
        } => {
            let jid: Jid = chat_id.parse()?;
            let participant = if chat_id.ends_with("@g.us") && !is_from_me {
                sender_jid.parse().ok()
            } else {
                None
            };
            let result = if starred {
                client
                    .chat_actions()
                    .star_message(&jid, participant.as_ref(), &msg_id, is_from_me)
                    .await
            } else {
                client
                    .chat_actions()
                    .unstar_message(&jid, participant.as_ref(), &msg_id, is_from_me)
                    .await
            };
            if let Err(e) = result {
                log::warn!("StarMessage failed: {e:#}");
            } else {
                let _ = tx
                    .send(WaEvent::MessageStarred {
                        chat_id,
                        msg_id,
                        starred,
                    })
                    .await;
            }
        }

        WaCommand::PinMessage { chat_id, msg_id } => {
            let jid: Jid = chat_id.parse()?;
            // Look up the message to get the correct from_me and participant.
            // In groups, even from_me messages need a participant (our own JID).
            let (from_me, participant) = {
                let s = state.lock().unwrap();
                let msg_info = s
                    .history
                    .get(&chat_id)
                    .and_then(|msgs| msgs.iter().find(|m| m.id == msg_id))
                    .map(|m| (m.is_from_me, m.sender_id.clone()));
                match msg_info {
                    Some((true, _)) if chat_id.ends_with("@g.us") => {
                        // Own message in group: participant = our own JID
                        let own = if !s.own_phone.is_empty() {
                            s.own_phone.clone()
                        } else {
                            String::new()
                        };
                        (true, if own.is_empty() { None } else { Some(own) })
                    }
                    Some((false, sender)) if !sender.is_empty() => (false, Some(sender)),
                    Some((from_me, _)) => (from_me, None),
                    None => (true, None),
                }
            };
            log::info!(
                "PinMessage: chat={chat_id} msg={msg_id} from_me={from_me} participant={participant:?}"
            );
            let pin_msg = wa::Message {
                pin_in_chat_message: Some(wa::message::PinInChatMessage {
                    key: Some(wa::MessageKey {
                        remote_jid: Some(chat_id.clone()),
                        id: Some(msg_id.clone()),
                        from_me: Some(from_me),
                        participant,
                    }),
                    r#type: Some(1), // PIN_FOR_ALL
                    sender_timestamp_ms: Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64,
                    ),
                }),
                ..Default::default()
            };
            match client.send_message(jid, pin_msg).await {
                Ok(_resp) => {
                    log::info!("Message pinned OK: {msg_id} in {chat_id}");
                    // Persist pinned message ID in chat summary
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                            c.pinned_msg_id = Some(msg_id.clone());
                            let _ = s.save_tx.send(s.chats.clone());
                        }
                    }
                    let _ = tx.send(WaEvent::MessagePinned { chat_id, msg_id }).await;
                }
                Err(e) => {
                    log::warn!("PinMessage send_message failed: {e:#}");
                    let _ = tx.send(WaEvent::MessagePinned { chat_id, msg_id }).await;
                }
            }
        }

        WaCommand::DeleteForMe {
            chat_id,
            msg_id,
            sender_jid,
            is_from_me,
        } => {
            let jid: Jid = chat_id.parse()?;
            let participant = if chat_id.ends_with("@g.us") && !is_from_me {
                sender_jid.parse().ok()
            } else {
                None
            };
            // Try server-side delete, but always remove locally
            if let Err(e) = client
                .chat_actions()
                .delete_message_for_me(&jid, participant.as_ref(), &msg_id, is_from_me, false, None)
                .await
            {
                log::warn!("DeleteForMe server error (removing locally): {e:#}");
            }
            {
                let mut s = state.lock().unwrap();
                if let Some(history) = s.history.get_mut(&chat_id) {
                    history.retain(|m| m.id != msg_id);
                    s.queue_save_messages(&chat_id);
                }
            }
            let _ = tx
                .send(WaEvent::MessageDeletedLocal { chat_id, msg_id })
                .await;
        }

        WaCommand::ForwardMessages {
            to_chat_id,
            msg_ids,
        } => {
            let to_jid: Jid = to_chat_id.parse()?;
            let mut count = 0u32;
            // Look up original messages from cache and forward each
            let messages_to_forward: Vec<IncomingMessage> = {
                let s = state.lock().unwrap();
                // Search all chat histories for the requested message IDs
                msg_ids
                    .iter()
                    .filter_map(|id| {
                        s.history
                            .values()
                            .flat_map(|msgs| msgs.iter())
                            .find(|m| m.id == *id)
                            .cloned()
                    })
                    .collect()
            };
            for orig in &messages_to_forward {
                // If message has a local media file, forward as media
                // Forward media messages (images, videos, GIFs, stickers, documents)
                if let Some(local_path) = &orig.media_local_path {
                    if let Ok(data) = std::fs::read(local_path) {
                        let lower = local_path.to_lowercase();
                        let upload_type = match &orig.media_type {
                            Some(crate::bridge::MediaType::Video)
                            | Some(crate::bridge::MediaType::Gif) => {
                                wacore::download::MediaType::Video
                            }
                            Some(crate::bridge::MediaType::Document) => {
                                wacore::download::MediaType::Document
                            }
                            Some(crate::bridge::MediaType::Audio) => {
                                wacore::download::MediaType::Audio
                            }
                            _ => wacore::download::MediaType::Image,
                        };
                        if let Ok(upload) = client.upload(data, upload_type).await {
                            let fwd_ctx = Box::new(wa::ContextInfo {
                                is_forwarded: Some(true),
                                forwarding_score: Some(orig.forwarding_score.saturating_add(1)),
                                ..Default::default()
                            });
                            let msg = match &orig.media_type {
                                Some(crate::bridge::MediaType::Sticker) => wa::Message {
                                    sticker_message: Some(Box::new(wa::message::StickerMessage {
                                        mimetype: Some("image/webp".into()),
                                        url: Some(upload.url),
                                        direct_path: Some(upload.direct_path),
                                        media_key: Some(upload.media_key),
                                        file_enc_sha256: Some(upload.file_enc_sha256),
                                        file_sha256: Some(upload.file_sha256),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                                Some(crate::bridge::MediaType::Video) => wa::Message {
                                    video_message: Some(Box::new(wa::message::VideoMessage {
                                        mimetype: Some("video/mp4".into()),
                                        url: Some(upload.url),
                                        direct_path: Some(upload.direct_path),
                                        media_key: Some(upload.media_key),
                                        file_enc_sha256: Some(upload.file_enc_sha256),
                                        file_sha256: Some(upload.file_sha256),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                                Some(crate::bridge::MediaType::Gif) => wa::Message {
                                    video_message: Some(Box::new(wa::message::VideoMessage {
                                        mimetype: Some("video/mp4".into()),
                                        gif_playback: Some(true),
                                        url: Some(upload.url),
                                        direct_path: Some(upload.direct_path),
                                        media_key: Some(upload.media_key),
                                        file_enc_sha256: Some(upload.file_enc_sha256),
                                        file_sha256: Some(upload.file_sha256),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                                Some(crate::bridge::MediaType::Document) => wa::Message {
                                    document_message: Some(Box::new(
                                        wa::message::DocumentMessage {
                                            mimetype: Some("application/octet-stream".into()),
                                            file_name: orig.media_filename.clone(),
                                            url: Some(upload.url),
                                            direct_path: Some(upload.direct_path),
                                            media_key: Some(upload.media_key),
                                            file_enc_sha256: Some(upload.file_enc_sha256),
                                            file_sha256: Some(upload.file_sha256),
                                            file_length: Some(upload.file_length),
                                            context_info: Some(fwd_ctx),
                                            ..Default::default()
                                        },
                                    )),
                                    ..Default::default()
                                },
                                Some(crate::bridge::MediaType::Audio) => wa::Message {
                                    audio_message: Some(Box::new(wa::message::AudioMessage {
                                        mimetype: Some("audio/ogg".into()),
                                        url: Some(upload.url),
                                        direct_path: Some(upload.direct_path),
                                        media_key: Some(upload.media_key),
                                        file_enc_sha256: Some(upload.file_enc_sha256),
                                        file_sha256: Some(upload.file_sha256),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                                _ => wa::Message {
                                    image_message: Some(Box::new(wa::message::ImageMessage {
                                        mimetype: Some("image/jpeg".into()),
                                        url: Some(upload.url),
                                        direct_path: Some(upload.direct_path),
                                        media_key: Some(upload.media_key),
                                        file_enc_sha256: Some(upload.file_enc_sha256),
                                        file_sha256: Some(upload.file_sha256),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                            };
                            if let Ok(real_id) = client.send_message(to_jid.clone(), msg).await {
                                count += 1;
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs() as i64;
                                let mut fwd_msg = orig.clone();
                                fwd_msg.id = real_id;
                                fwd_msg.chat_id = to_chat_id.clone();
                                fwd_msg.is_from_me = true;
                                fwd_msg.timestamp = now;
                                fwd_msg.is_forwarded = true;
                                persist_new_message(&fwd_msg, state);
                                let _ = tx.send(WaEvent::MessageReceived(fwd_msg)).await;
                            }
                        }
                    }
                    continue;
                }
                // Forward contact cards
                if let (Some(cn), Some(cv)) = (&orig.contact_name, &orig.contact_vcard) {
                    let msg = wa::Message {
                        contact_message: Some(Box::new(wa::message::ContactMessage {
                            display_name: Some(cn.clone()),
                            vcard: Some(cv.clone()),
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    if let Ok(real_id) = client.send_message(to_jid.clone(), msg).await {
                        count += 1;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        let mut fwd_msg = orig.clone();
                        fwd_msg.id = real_id;
                        fwd_msg.chat_id = to_chat_id.clone();
                        fwd_msg.is_from_me = true;
                        fwd_msg.timestamp = now;
                        persist_new_message(&fwd_msg, state);
                        let _ = tx.send(WaEvent::MessageReceived(fwd_msg)).await;
                    }
                    continue;
                }
                let text = orig.text.as_deref().or(orig.media_caption.as_deref());
                if let Some(text) = text {
                    let fwd_msg = wa::Message {
                        extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                            text: Some(text.to_string()),
                            context_info: Some(Box::new(wa::ContextInfo {
                                is_forwarded: Some(true),
                                forwarding_score: Some(orig.forwarding_score.saturating_add(1)),
                                ..Default::default()
                            })),
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    match client.send_message(to_jid.clone(), fwd_msg).await {
                        Ok(real_id) => {
                            count += 1;
                            // Create optimistic local message so it appears immediately
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let local_msg = IncomingMessage {
                                id: real_id,
                                chat_id: to_chat_id.clone(),
                                sender_id: String::new(),
                                sender_name: String::new(),
                                text: Some(text.to_string()),
                                media_type: None,
                                timestamp: now,
                                is_from_me: true,
                                is_forwarded: true,
                                forwarding_score: orig.forwarding_score.saturating_add(1),
                                quoted_msg_id: None,
                                quoted_text: None,
                                quoted_sender: None,
                                reactions: vec![],
                                media_local_path: None,
                                media_filename: None,
                                media_caption: None,
                                contact_name: None,
                                contact_vcard: None,
                                link_title: None,
                                link_description: None,
                                link_url: None,
                                link_thumbnail_path: None,
                                poll_question: None,
                                poll_options: vec![],
                                poll_selectable: 0,
                                poll_secret: vec![],
                                poll_votes: vec![],
                                receipt_status: ReceiptStatus::Sent,
                                is_edited: false,
                                is_system_message: false,
                                quoted_media_path: None,
                            };
                            // Add to cache and notify UI
                            persist_new_message(&local_msg, state);
                            let _ = tx.send(WaEvent::MessageReceived(local_msg)).await;
                        }
                        Err(e) => log::warn!("Forward failed: {e:#}"),
                    }
                }
            }
            let _ = tx
                .send(WaEvent::ForwardComplete { to_chat_id, count })
                .await;
        }

        WaCommand::GetChatList => {
            let chats = state.lock().unwrap().chats_with_best_names();
            let _ = tx.send(WaEvent::ChatListForPicker(chats)).await;
        }

        WaCommand::SyncQuickReplies => {
            let local = crate::ui::quick_replies::load();
            let replies: Vec<crate::bridge::QuickReplyData> = local
                .iter()
                .map(|r| crate::bridge::QuickReplyData {
                    shortcut: r.shortcut.clone(),
                    message: r.text.clone(),
                    keywords: vec![],
                })
                .collect();
            let _ = tx.send(WaEvent::QuickRepliesSynced { replies }).await;
        }

        WaCommand::SaveQuickReply { shortcut, message } => {
            match client
                .chat_actions()
                .save_quick_reply(&shortcut, &message)
                .await
            {
                Ok(()) => {
                    log::info!("Quick reply saved and synced: /{shortcut}");
                    let _ = tx
                        .send(WaEvent::QuickRepliesSynced {
                            replies: vec![crate::bridge::QuickReplyData {
                                shortcut,
                                message,
                                keywords: vec![],
                            }],
                        })
                        .await;
                }
                Err(e) => log::warn!("SaveQuickReply failed: {e:#}"),
            }
        }

        WaCommand::DeleteQuickReply { shortcut } => {
            match client.chat_actions().delete_quick_reply(&shortcut).await {
                Ok(()) => log::info!("Quick reply deleted: /{shortcut}"),
                Err(e) => log::warn!("DeleteQuickReply failed: {e:#}"),
            }
        }

        WaCommand::CreateGroup {
            subject,
            participants,
        } => {
            use wacore::iq::groups::GroupParticipantOptions;
            use whatsapp_rust::GroupCreateOptions;
            let parts: Vec<GroupParticipantOptions> = participants
                .iter()
                .filter_map(|p| p.parse::<Jid>().ok())
                .map(|j| GroupParticipantOptions::new(j))
                .collect();
            let opts = GroupCreateOptions {
                subject: subject.clone(),
                participants: parts,
                ..Default::default()
            };
            match client.groups().create_group(opts).await {
                Ok(result) => {
                    let chat_id = result.gid.to_string();
                    let summary = ChatSummary {
                        id: chat_id.clone(),
                        name: subject,
                        last_message: String::new(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64,
                        unread_count: 0,
                        is_group: true,
                        is_muted: false,
                        is_pinned: false,
                        is_archived: false,
                        is_favorite: false,
                        label: None,
                        pinned_msg_id: None,
                    };
                    persist_chat(state, summary.clone());
                    let _ = tx.send(WaEvent::ChatAdded(summary)).await;
                    log::info!("Created group: {chat_id}");
                }
                Err(e) => log::warn!("CreateGroup failed: {e:#}"),
            }
        }

        WaCommand::GetGroupMembers { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            match client.groups().get_metadata(&jid).await {
                Ok(meta) => {
                    let members: Vec<crate::bridge::GroupMember> = meta
                        .participants
                        .iter()
                        .map(|p| {
                            let pjid = p.jid.to_string();
                            let name = {
                                let s = state.lock().unwrap();
                                resolve_sender_name(&s, &pjid)
                            };
                            crate::bridge::GroupMember {
                                jid: pjid,
                                name,
                                is_admin: p.is_admin,
                            }
                        })
                        .collect();
                    let _ = tx.send(WaEvent::GroupMembers { chat_id, members }).await;
                }
                Err(e) => log::warn!("GetGroupMembers failed: {e:#}"),
            }
        }

        WaCommand::GetOwnProfile => {
            let own_jid = client.get_pn().await;
            let mut name = client.get_push_name().await;
            let mut about = String::new();
            let mut description = String::new();
            let mut email = String::new();
            let mut website = String::new();
            let mut address = String::new();
            let mut category = String::new();

            // Get status/about text
            if let Some(ref jid) = own_jid {
                if let Ok(info) = client.contacts().get_user_info(&[jid.clone()]).await {
                    if let Some(u) = info.get(jid) {
                        about = u.status.clone().unwrap_or_default();
                    }
                }
                // Get business profile
                if let Ok(Some(bp)) = client.get_business_profile(jid).await {
                    description = bp.description;
                    email = bp.email.unwrap_or_default();
                    website = bp.website.first().cloned().unwrap_or_default();
                    address = bp.address.unwrap_or_default();
                    category = bp
                        .categories
                        .first()
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                }
            }

            let _ = tx
                .send(WaEvent::OwnProfile {
                    name,
                    about,
                    description,
                    email,
                    website,
                    address,
                    category,
                })
                .await;
        }

        WaCommand::SendPoll {
            chat_id,
            question,
            options,
            selectable_count,
        } => {
            let jid: Jid = chat_id.parse()?;
            match client
                .polls()
                .create(&jid, &question, &options, selectable_count)
                .await
            {
                Ok((msg_id, secret)) => {
                    log::info!(
                        "Poll created: {msg_id} in {chat_id} secret_len={}",
                        secret.len()
                    );
                    // Create optimistic message for the poll
                    let opts_text = options
                        .iter()
                        .enumerate()
                        .map(|(i, o)| format!("  {}. {o}", i + 1))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let preview = format!("📊 *{question}*\n{opts_text}");
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let poll_msg = IncomingMessage {
                        id: msg_id.clone(),
                        chat_id: chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: Some(format!("📊 {question}")),
                        media_type: None,
                        timestamp: now,
                        is_from_me: true,
                        is_forwarded: false,
                        forwarding_score: 0,
                        quoted_msg_id: None,
                        quoted_text: None,
                        quoted_sender: None,
                        reactions: vec![],
                        media_local_path: None,
                        media_filename: None,
                        media_caption: None,
                        contact_name: None,
                        contact_vcard: None,
                        link_title: None,
                        link_description: None,
                        link_url: None,
                        link_thumbnail_path: None,
                        quoted_media_path: None,
                        poll_question: Some(question),
                        poll_options: options,
                        poll_selectable: selectable_count,
                        poll_secret: secret.clone(),
                        poll_votes: vec![],
                        receipt_status: ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                    };
                    let (_, _) = persist_new_message(&poll_msg, state);
                    let _ = tx.send(WaEvent::MessageReceived(poll_msg)).await;
                }
                Err(e) => log::error!("SendPoll FAILED: {e:#}"),
            }
        }

        WaCommand::VotePoll {
            chat_id,
            poll_msg_id,
            poll_creator,
            poll_secret,
            selected_options,
        } => {
            // Resolve creator to PHONE JID (WhatsApp requires PN format for vote MessageKey)
            let poll_creator = {
                let s = state.lock().unwrap();
                if poll_creator == "self" || poll_creator.is_empty() {
                    // Own poll — use our phone JID
                    s.own_phone.clone()
                } else if poll_creator.ends_with("@lid") {
                    // Resolve LID to phone
                    s.lid_to_phone
                        .get(&poll_creator)
                        .cloned()
                        .unwrap_or(poll_creator)
                } else {
                    poll_creator
                }
            };
            log::info!(
                "VotePoll: chat={chat_id} poll={poll_msg_id} creator={poll_creator} secret_len={} options={selected_options:?}",
                poll_secret.len()
            );
            if poll_secret.is_empty() {
                log::error!(
                    "VotePoll: NO SECRET — cannot encrypt vote. Poll secret was not stored."
                );
                // Try to find it from the message in cache
                let cached_secret = {
                    let s = state.lock().unwrap();
                    s.history
                        .get(&chat_id)
                        .and_then(|msgs| msgs.iter().find(|m| m.id == poll_msg_id))
                        .map(|m| m.poll_secret.clone())
                        .unwrap_or_default()
                };
                if cached_secret.is_empty() {
                    log::error!("VotePoll: Secret not in cache either. Cannot vote.");
                } else {
                    log::info!(
                        "VotePoll: Found secret in cache ({} bytes)",
                        cached_secret.len()
                    );
                    let jid: Jid = chat_id.parse()?;
                    let creator_jid: Jid = poll_creator.parse().unwrap_or(jid.clone());
                    match client
                        .polls()
                        .vote(
                            &jid,
                            &poll_msg_id,
                            &creator_jid,
                            &cached_secret,
                            &selected_options,
                        )
                        .await
                    {
                        Ok(vote_id) => log::info!("Poll vote sent: {vote_id}"),
                        Err(e) => log::error!("VotePoll FAILED: {e:#}"),
                    }
                }
            } else {
                let jid: Jid = chat_id.parse()?;
                let creator_jid: Jid = poll_creator.parse().unwrap_or(jid.clone());
                match client
                    .polls()
                    .vote(
                        &jid,
                        &poll_msg_id,
                        &creator_jid,
                        &poll_secret,
                        &selected_options,
                    )
                    .await
                {
                    Ok(vote_id) => log::info!("Poll vote sent: {vote_id}"),
                    Err(e) => log::error!("VotePoll FAILED: {e:#}"),
                }
            }
            // Persist vote locally immediately — don't wait for server echo
            {
                let voter_name = {
                    let s = state.lock().unwrap();
                    if !s.own_phone.is_empty() {
                        resolve_sender_name(&s, &s.own_phone.clone())
                    } else {
                        "Me".to_string()
                    }
                };
                let mut s = state.lock().unwrap();
                if !s.history.contains_key(&chat_id) {
                    let disk_msgs = load_messages(&chat_id);
                    if !disk_msgs.is_empty() {
                        s.history.insert(chat_id.clone(), disk_msgs);
                    }
                }
                if let Some(msgs) = s.history.get_mut(&chat_id) {
                    if let Some(pm) = msgs.iter_mut().find(|m| m.id == poll_msg_id) {
                        pm.poll_votes.retain(|(n, _)| *n != voter_name);
                        pm.poll_votes.push((voter_name, selected_options));
                        log::info!(
                            "VOTE_LOCAL: persisted vote on {} ({} total voters)",
                            poll_msg_id,
                            pm.poll_votes.len()
                        );
                    }
                    s.queue_save_messages(&chat_id);
                }
            }
        }

        WaCommand::SetPushName { name } => {
            if let Err(e) = client.profile().set_push_name(&name).await {
                log::warn!("SetPushName failed: {e:#}");
            } else {
                log::info!("Push name updated to: {name}");
            }
        }

        WaCommand::SetStatus { text } => {
            if let Err(e) = client.profile().set_status_text(&text).await {
                log::warn!("SetStatus failed: {e:#}");
            } else {
                log::info!("Status updated to: {text}");
            }
        }

        WaCommand::GetContactProfile { chat_id } => {
            // "self" = own profile
            let jid: Jid = if chat_id == "self" {
                match client.get_pn().await {
                    Some(j) => j,
                    None => return Ok(()),
                }
            } else {
                chat_id.parse()?
            };
            let phone = display_name_from_jid(&chat_id);
            // Try to get user info for about/status text
            let about = match client.contacts().get_user_info(&[jid.clone()]).await {
                Ok(info) => info.get(&jid).and_then(|u| u.status.clone()),
                Err(_) => None,
            };
            let safe_id = chat_id.replace(['/', '\\', '@', ':'], "_");
            let avatar_path = std::path::PathBuf::from(AVATARS_DIR).join(format!("{safe_id}.jpg"));
            let avatar = if avatar_path.exists() {
                avatar_path
                    .canonicalize()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            } else {
                None
            };
            let _ = tx
                .send(WaEvent::ContactProfile {
                    chat_id: chat_id.clone(),
                    phone,
                    about,
                    avatar_path: avatar,
                })
                .await;

            // Find groups in common by checking group participant lists
            let contact_jid_str = chat_id.clone();
            let contact_lid = {
                let s = state.lock().unwrap();
                // Find LID for this phone JID (reverse lookup)
                s.lid_to_phone
                    .iter()
                    .find(|(_, phone)| phone.as_str() == contact_jid_str)
                    .map(|(lid, _)| lid.clone())
            };
            let common_groups = match client.groups().get_participating().await {
                Ok(groups) => {
                    let s = state.lock().unwrap();
                    groups
                        .iter()
                        .filter(|(_, meta)| {
                            meta.participants.iter().any(|p| {
                                let pjid = p.jid.to_string();
                                let pphone = p.phone_number.as_ref().map(|j| j.to_string());
                                pjid == contact_jid_str
                                    || pphone.as_deref() == Some(&contact_jid_str)
                                    || contact_lid.as_deref() == Some(pjid.as_str())
                            })
                        })
                        .filter_map(|(gid, meta)| {
                            Some(ChatSummary {
                                id: gid.clone(),
                                name: meta.subject.clone(),
                                last_message: String::new(),
                                timestamp: 0,
                                unread_count: 0,
                                is_group: true,
                                is_muted: false,
                                is_pinned: false,
                                is_archived: false,
                                is_favorite: false,
                                label: None,
                                pinned_msg_id: None,
                            })
                        })
                        .collect()
                }
                Err(e) => {
                    log::warn!("Failed to get groups for common groups: {e:#}");
                    vec![]
                }
            };
            log::info!(
                "Found {} groups in common with {}",
                common_groups.len(),
                contact_jid_str
            );
            let _ = tx
                .send(WaEvent::GroupsInCommon {
                    chat_id,
                    groups: common_groups,
                })
                .await;
        }

        WaCommand::GetGroupInfo { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            match client.groups().get_metadata(&jid).await {
                Ok(meta) => {
                    let participants: Vec<crate::bridge::GroupMember> = meta
                        .participants
                        .iter()
                        .map(|p| {
                            let pjid = p.jid.to_string();
                            let phone_jid = p.phone_number.as_ref().map(|j| j.to_string());
                            let s = state.lock().unwrap();
                            // Try: contact_names by participant JID, then by phone_number JID,
                            // then lid_to_phone resolution, then phone_number formatted
                            let name = s
                                .contact_names
                                .get(&pjid)
                                .cloned()
                                .or_else(|| {
                                    phone_jid
                                        .as_ref()
                                        .and_then(|pn| s.contact_names.get(pn).cloned())
                                })
                                .or_else(|| {
                                    // LID → phone → contact name
                                    if pjid.ends_with("@lid") {
                                        s.lid_to_phone.get(&pjid).and_then(|phone| {
                                            s.contact_names
                                                .get(phone)
                                                .cloned()
                                                .or_else(|| Some(display_name_from_jid(phone)))
                                        })
                                    } else {
                                        None
                                    }
                                })
                                .or_else(|| phone_jid.as_ref().map(|pn| display_name_from_jid(pn)))
                                .unwrap_or_else(|| display_name_from_jid(&pjid));
                            drop(s);
                            // Use the phone JID if available for avatar loading
                            let display_jid = phone_jid.unwrap_or(pjid);
                            crate::bridge::GroupMember {
                                jid: display_jid,
                                name,
                                is_admin: p.is_admin,
                            }
                        })
                        .collect();
                    // Check if current user is admin
                    // get_pn() returns JID with device suffix (e.g. 1234567890:82@s.whatsapp.net)
                    // Strip the device part to match participant JIDs (1234567890@s.whatsapp.net)
                    let own_pn_raw = client
                        .get_pn()
                        .await
                        .map(|j| j.to_string())
                        .unwrap_or_default();
                    let own_pn = if let Some(pos) = own_pn_raw.find(':') {
                        format!(
                            "{}{}",
                            &own_pn_raw[..pos],
                            &own_pn_raw[own_pn_raw.find('@').unwrap_or(own_pn_raw.len())..]
                        )
                    } else {
                        own_pn_raw.clone()
                    };
                    // Also extract just the phone number for flexible matching
                    let own_number = own_pn.split('@').next().unwrap_or("").to_string();
                    let i_am_admin = meta.participants.iter().any(|p| {
                        if !p.is_admin {
                            return false;
                        }
                        let pstr = p.jid.to_string();
                        let pphone = p.phone_number.as_ref().map(|j| j.to_string());
                        pstr == own_pn
                            || pphone.as_deref() == Some(&own_pn)
                            || pstr.starts_with(&own_number)
                            || pphone
                                .as_ref()
                                .map(|pp| pp.starts_with(&own_number))
                                .unwrap_or(false)
                    });
                    log::info!(
                        "Group admin check: own_pn={own_pn} own_number={own_number} i_am_admin={i_am_admin}"
                    );
                    // Update chat name in sidebar
                    if !meta.subject.is_empty() {
                        let _ = tx
                            .send(WaEvent::ChatNameUpdated {
                                chat_id: chat_id.clone(),
                                name: meta.subject.clone(),
                            })
                            .await;
                    }
                    let _ = tx
                        .send(WaEvent::GroupProfile {
                            chat_id,
                            subject: meta.subject,
                            description: meta.description,
                            participants,
                            i_am_admin,
                        })
                        .await;
                }
                Err(e) => log::warn!("GetGroupInfo failed: {e:#}"),
            }
        }

        WaCommand::SetGroupSubject { chat_id, subject } => {
            let jid: Jid = chat_id.parse()?;
            let group_subject = match whatsapp_rust::GroupSubject::new(&subject) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("Invalid group subject: {e}");
                    return Ok(());
                }
            };
            match client.groups().set_subject(&jid, group_subject).await {
                Ok(_) => {
                    log::info!("Group subject updated: {chat_id} → {subject:?}");
                    state.lock().unwrap().rename_chat(&chat_id, &subject);
                    let _ = tx
                        .send(WaEvent::ChatNameUpdated {
                            chat_id,
                            name: subject,
                        })
                        .await;
                }
                Err(e) => log::warn!("SetGroupSubject failed: {e:#}"),
            }
        }

        WaCommand::CheckOnWhatsApp { phone } => {
            // Normalize: strip +, spaces, dashes for the API
            let normalized = phone.replace(['+', ' ', '-', '(', ')'], "");
            log::info!("CheckOnWhatsApp: input='{phone}' normalized='{normalized}'");
            match client.contacts().is_on_whatsapp(&[&normalized]).await {
                Ok(results) => {
                    log::info!("CheckOnWhatsApp result: {} results", results.len());
                    for r in &results {
                        log::info!("  jid={} registered={}", r.jid, r.is_registered);
                    }
                    let (jid, is_reg) = results
                        .first()
                        .map(|r| (Some(r.jid.to_string()), r.is_registered))
                        .unwrap_or((None, false));
                    let _ = tx
                        .send(WaEvent::PhoneLookupResult {
                            phone,
                            jid,
                            is_registered: is_reg,
                        })
                        .await;
                }
                Err(e) => {
                    log::warn!("CheckOnWhatsApp failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::PhoneLookupResult {
                            phone,
                            jid: None,
                            is_registered: false,
                        })
                        .await;
                }
            }
        }

        WaCommand::StartNewChat { jid } => {
            // Resolve LID to phone if possible
            let resolved_jid = {
                let s = state.lock().unwrap();
                if jid.ends_with("@lid") {
                    s.lid_to_phone.get(&jid).cloned().unwrap_or(jid.clone())
                } else {
                    jid.clone()
                }
            };
            // Check if chat already exists
            let exists = {
                let s = state.lock().unwrap();
                s.chats.iter().any(|c| c.id == resolved_jid)
            };
            if !exists {
                let name = {
                    let s = state.lock().unwrap();
                    resolve_sender_name(&s, &resolved_jid)
                };
                let summary = ChatSummary {
                    id: resolved_jid.clone(),
                    name,
                    last_message: String::new(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    unread_count: 0,
                    is_group: resolved_jid.ends_with("@g.us"),
                    is_muted: false,
                    is_pinned: false,
                    is_archived: false,
                    is_favorite: false,
                    label: None,
                    pinned_msg_id: None,
                };
                persist_chat(state, summary.clone());
                let _ = tx.send(WaEvent::ChatAdded(summary)).await;
            }
            // Always send LoadChat so the UI opens it
            let name = {
                let s = state.lock().unwrap();
                resolve_sender_name(&s, &resolved_jid)
            };
            let _ = tx
                .send(WaEvent::HistoryMessages {
                    chat_id: resolved_jid,
                    chat_name: name,
                    messages: vec![],
                })
                .await;
        }

        WaCommand::SendImage {
            chat_id,
            path,
            caption,
            tmp_id,
        } => {
            let jid: Jid = chat_id.parse()?;
            // Check file size first — reject files over 64MB
            let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if file_size > 64 * 1024 * 1024 {
                log::warn!("File too large to send: {} bytes", file_size);
                let _ = tx
                    .send(WaEvent::MessageFailed {
                        msg_id: tmp_id,
                        chat_id,
                    })
                    .await;
                return Ok(());
            }
            let (dl_tx, dl_rx) = tokio::sync::oneshot::channel();
            let p = path.clone();
            std::thread::Builder::new()
                .name("file-read".into())
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    let _ = dl_tx.send(std::fs::read(p));
                })
                .ok();
            let file_data = match dl_rx.await {
                Ok(Ok(d)) => d,
                _ => {
                    log::warn!("Failed to read file");
                    return Ok(());
                }
            };

            // Detect file type from extension
            let lower_path = path.to_lowercase();
            let is_image = lower_path.ends_with(".jpg")
                || lower_path.ends_with(".jpeg")
                || lower_path.ends_with(".png")
                || lower_path.ends_with(".webp")
                || lower_path.ends_with(".gif");
            let is_video = lower_path.ends_with(".mp4")
                || lower_path.ends_with(".mov")
                || lower_path.ends_with(".avi");

            let (upload_type, mime) = if is_image {
                (wacore::download::MediaType::Image, "image/jpeg")
            } else if is_video {
                (wacore::download::MediaType::Video, "video/mp4")
            } else {
                (
                    wacore::download::MediaType::Document,
                    "application/octet-stream",
                )
            };

            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("file")
                .to_string();

            match client.upload(file_data, upload_type).await {
                Ok(upload) => {
                    let file_len = upload.file_length;
                    let msg = if is_image {
                        wa::Message {
                            image_message: Some(Box::new(wa::message::ImageMessage {
                                mimetype: Some(mime.to_string()),
                                caption: caption.clone(),
                                url: Some(upload.url),
                                direct_path: Some(upload.direct_path),
                                media_key: Some(upload.media_key),
                                file_enc_sha256: Some(upload.file_enc_sha256),
                                file_sha256: Some(upload.file_sha256),
                                file_length: Some(file_len),
                                ..Default::default()
                            })),
                            ..Default::default()
                        }
                    } else if is_video {
                        wa::Message {
                            video_message: Some(Box::new(wa::message::VideoMessage {
                                mimetype: Some(mime.to_string()),
                                caption: caption.clone(),
                                url: Some(upload.url),
                                direct_path: Some(upload.direct_path),
                                media_key: Some(upload.media_key),
                                file_enc_sha256: Some(upload.file_enc_sha256),
                                file_sha256: Some(upload.file_sha256),
                                file_length: Some(file_len),
                                ..Default::default()
                            })),
                            ..Default::default()
                        }
                    } else {
                        wa::Message {
                            document_message: Some(Box::new(wa::message::DocumentMessage {
                                mimetype: Some(mime.to_string()),
                                caption: caption.clone(),
                                file_name: Some(filename.clone()),
                                url: Some(upload.url),
                                direct_path: Some(upload.direct_path),
                                media_key: Some(upload.media_key),
                                file_enc_sha256: Some(upload.file_enc_sha256),
                                file_sha256: Some(upload.file_sha256),
                                file_length: Some(file_len),
                                ..Default::default()
                            })),
                            ..Default::default()
                        }
                    };
                    let media_type = if is_image {
                        crate::bridge::MediaType::Image
                    } else if is_video {
                        crate::bridge::MediaType::Video
                    } else {
                        crate::bridge::MediaType::Document
                    };
                    match client.send_message(jid, msg).await {
                        Ok(real_id) => {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let sent_msg = IncomingMessage {
                                id: real_id.clone(),
                                chat_id: chat_id.clone(),
                                sender_id: String::new(),
                                sender_name: String::new(),
                                text: None,
                                media_type: Some(media_type),
                                timestamp: now,
                                is_from_me: true,
                                is_forwarded: false,
                                forwarding_score: 0,
                                quoted_msg_id: None,
                                quoted_text: None,
                                quoted_sender: None,
                                reactions: vec![],
                                media_local_path: Some(path),
                                media_filename: Some(filename),
                                poll_question: None,
                                poll_options: vec![],
                                poll_selectable: 0,
                                poll_secret: vec![],
                                poll_votes: vec![],
                                media_caption: caption,
                                contact_name: None,
                                contact_vcard: None,
                                link_title: None,
                                link_description: None,
                                link_url: None,
                                link_thumbnail_path: None,
                                receipt_status: ReceiptStatus::Sent,
                                is_edited: false,
                                is_system_message: false,
                                quoted_media_path: None,
                            };
                            persist_new_message(&sent_msg, state);
                            let _ = tx
                                .send(WaEvent::MessageConfirmed {
                                    tmp_id,
                                    real_id: real_id.clone(),
                                    chat_id: chat_id.clone(),
                                })
                                .await;
                            let _ = tx.send(WaEvent::MessageReceived(sent_msg)).await;
                            log::info!("Image sent successfully");
                        }
                        Err(e) => {
                            log::warn!("SendImage message failed: {e:#}");
                            let _ = tx
                                .send(WaEvent::MessageFailed {
                                    msg_id: tmp_id,
                                    chat_id,
                                })
                                .await;
                        }
                    }
                }
                Err(e) => {
                    log::warn!("SendImage upload failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                }
            }
        }

        WaCommand::SendGif {
            chat_id,
            mp4_url,
            tmp_id,
        } => {
            let jid: Jid = chat_id.parse()?;
            // Download the GIF MP4 from Tenor on a thread with large stack
            let (dl_tx, dl_rx) = tokio::sync::oneshot::channel::<anyhow::Result<Vec<u8>>>();
            let url = mp4_url.clone();
            std::thread::Builder::new()
                .name("gif-download".into())
                .stack_size(4 * 1024 * 1024) // 4MB stack
                .spawn(move || {
                    use std::io::Read;
                    let result = ureq::get(&url)
                        .call()
                        .map_err(|e| anyhow::anyhow!("{e}"))
                        .and_then(|r| {
                            let mut bytes = Vec::new();
                            r.into_reader().read_to_end(&mut bytes)?;
                            Ok(bytes)
                        });
                    let _ = dl_tx.send(result);
                })
                .ok();
            let data = match dl_rx.await {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    log::warn!("GIF download failed: {e:#}");
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("GIF download channel failed: {e:#}");
                    return Ok(());
                }
            };

            // Save locally so the GIF shows in our app
            let local_path = {
                let dir = std::env::current_dir().unwrap_or_default().join(MEDIA_DIR);
                let _ = std::fs::create_dir_all(&dir);
                let fname = format!(
                    "gif_{}.mp4",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                );
                let path = dir.join(&fname);
                let _ = std::fs::write(&path, &data);
                path.canonicalize()
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string()
            };

            // Upload as video
            match client
                .upload(data, wacore::download::MediaType::Video)
                .await
            {
                Ok(upload) => {
                    let msg = wa::Message {
                        video_message: Some(Box::new(wa::message::VideoMessage {
                            mimetype: Some("video/mp4".to_string()),
                            url: Some(upload.url),
                            direct_path: Some(upload.direct_path),
                            media_key: Some(upload.media_key),
                            file_enc_sha256: Some(upload.file_enc_sha256),
                            file_sha256: Some(upload.file_sha256),
                            file_length: Some(upload.file_length),
                            gif_playback: Some(true),
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    match client.send_message(jid, msg).await {
                        Ok(real_id) => {
                            log::info!("GIF sent: {real_id}");
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let sent = IncomingMessage {
                                id: real_id.clone(),
                                chat_id: chat_id.clone(),
                                sender_id: String::new(),
                                sender_name: String::new(),
                                text: None,
                                media_type: Some(crate::bridge::MediaType::Gif),
                                timestamp: now,
                                is_from_me: true,
                                is_forwarded: false,
                                forwarding_score: 0,
                                quoted_msg_id: None,
                                quoted_text: None,
                                poll_question: None,
                                poll_options: vec![],
                                poll_selectable: 0,
                                poll_secret: vec![],
                                poll_votes: vec![],
                                quoted_sender: None,
                                reactions: vec![],
                                media_local_path: Some(local_path),
                                media_filename: None,
                                media_caption: None,
                                contact_name: None,
                                contact_vcard: None,
                                link_title: None,
                                link_description: None,
                                link_url: None,
                                link_thumbnail_path: None,
                                receipt_status: ReceiptStatus::Sent,
                                is_edited: false,
                                is_system_message: false,
                                quoted_media_path: None,
                            };
                            persist_new_message(&sent, state);
                            let _ = tx.send(WaEvent::MessageReceived(sent)).await;
                            let _ = tx
                                .send(WaEvent::MessageConfirmed {
                                    tmp_id,
                                    real_id,
                                    chat_id,
                                })
                                .await;
                        }
                        Err(e) => log::warn!("GIF send failed: {e:#}"),
                    }
                }
                Err(e) => log::warn!("GIF upload failed: {e:#}"),
            }
        }

        WaCommand::SearchGifs { query } => {
            // Use Tenor API v2 for GIF search (same as WhatsApp Web)
            let tenor_key = tenor_api_key();
            let url = format!(
                "https://tenor.googleapis.com/v2/search?q={}&key={tenor_key}&client_key=gboard&media_filter=mp4,tinygif&limit=20",
                query.replace(' ', "+").replace('&', "%26")
            );
            match tokio::task::spawn_blocking(move || {
                ureq::get(&url)
                    .call()
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .and_then(|resp| {
                        let body = resp.into_string()?;
                        Ok(body)
                    })
            })
            .await
            {
                Ok(Ok(body)) => {
                    // Parse Tenor JSON response
                    let gifs: Vec<crate::bridge::GifResult> =
                        serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| v.get("results")?.as_array().cloned())
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|r| {
                                let media = r.get("media_formats")?;
                                let preview = media.get("tinygif")?.get("url")?.as_str()?;
                                let mp4 = media.get("mp4")?.get("url")?.as_str()?;
                                let title = r.get("content_description")?.as_str().unwrap_or("");
                                Some(crate::bridge::GifResult {
                                    preview_url: preview.to_string(),
                                    mp4_url: mp4.to_string(),
                                    title: title.to_string(),
                                })
                            })
                            .collect();
                    log::info!("GIF search '{}': {} results", query, gifs.len());
                    let _ = tx.send(WaEvent::GifResults { gifs }).await;
                }
                Ok(Err(e)) => log::warn!("GIF search failed: {e:#}"),
                Err(e) => log::warn!("GIF search task failed: {e:#}"),
            }
        }

        WaCommand::SendContact {
            to_chat_id,
            contact_name,
            contact_phone,
            tmp_id,
        } => {
            let jid: Jid = to_chat_id.parse()?;
            // Build vCard
            let vcard = format!(
                "BEGIN:VCARD\nVERSION:3.0\nN:;{contact_name};;;\nFN:{contact_name}\nTEL;type=CELL;waid={phone}:{formatted}\nEND:VCARD",
                phone = contact_phone.replace('+', ""),
                formatted = contact_phone,
            );
            let msg = wa::Message {
                contact_message: Some(Box::new(wa::message::ContactMessage {
                    display_name: Some(contact_name.clone()),
                    vcard: Some(vcard.clone()),
                    ..Default::default()
                })),
                ..Default::default()
            };
            match client.send_message(jid, msg).await {
                Ok(real_id) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let sent = IncomingMessage {
                        id: real_id.clone(),
                        chat_id: to_chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: Some(format!("📇 {contact_name}")),
                        media_type: None,
                        timestamp: now,
                        is_from_me: true,
                        is_forwarded: false,
                        forwarding_score: 0,
                        quoted_msg_id: None,
                        quoted_text: None,
                        quoted_sender: None,
                        reactions: vec![],
                        media_local_path: None,
                        poll_question: None,
                        poll_options: vec![],
                        poll_selectable: 0,
                        poll_secret: vec![],
                        poll_votes: vec![],
                        media_filename: None,
                        media_caption: None,
                        contact_name: Some(contact_name.clone()),
                        contact_vcard: Some(vcard.clone()),
                        link_title: None,
                        link_description: None,
                        link_url: None,
                        link_thumbnail_path: None,
                        quoted_media_path: None,
                        receipt_status: ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                    };
                    persist_new_message(&sent, state);
                    let _ = tx.send(WaEvent::MessageReceived(sent)).await;
                    let _ = tx
                        .send(WaEvent::MessageConfirmed {
                            tmp_id,
                            real_id,
                            chat_id: to_chat_id,
                        })
                        .await;
                    log::info!("Contact shared: {contact_name}");
                }
                Err(e) => log::warn!("SendContact failed: {e:#}"),
            }
        }

        WaCommand::SearchStickers { query } => {
            let tenor_key = tenor_api_key();
            let url = format!(
                "https://tenor.googleapis.com/v2/search?q={}&key={tenor_key}&client_key=gboard&searchfilter=sticker&media_filter=webp_transparent,tinygif&limit=20",
                query.replace(' ', "+").replace('&', "%26")
            );
            match tokio::task::spawn_blocking(move || {
                ureq::get(&url)
                    .call()
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .and_then(|resp| Ok(resp.into_string()?))
            })
            .await
            {
                Ok(Ok(body)) => {
                    let stickers: Vec<crate::bridge::GifResult> =
                        serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| v.get("results")?.as_array().cloned())
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|r| {
                                let media = r.get("media_formats")?;
                                let preview = media
                                    .get("tinygif")
                                    .or(media.get("webp_transparent"))?
                                    .get("url")?
                                    .as_str()?;
                                let webp = media
                                    .get("webp_transparent")
                                    .or(media.get("tinygif"))?
                                    .get("url")?
                                    .as_str()?;
                                let title = r.get("content_description")?.as_str().unwrap_or("");
                                Some(crate::bridge::GifResult {
                                    preview_url: preview.to_string(),
                                    mp4_url: webp.to_string(), // reuse field for webp URL
                                    title: title.to_string(),
                                })
                            })
                            .collect();
                    log::info!("Sticker search '{}': {} results", query, stickers.len());
                    let _ = tx.send(WaEvent::StickerResults { stickers }).await;
                }
                Ok(Err(e)) => log::warn!("Sticker search failed: {e:#}"),
                Err(e) => log::warn!("Sticker search task failed: {e:#}"),
            }
        }

        WaCommand::SendSticker {
            chat_id,
            webp_url,
            tmp_id,
        } => {
            let jid: Jid = chat_id.parse()?;
            let (dl_tx, dl_rx) = tokio::sync::oneshot::channel::<anyhow::Result<Vec<u8>>>();
            let url = webp_url.clone();
            std::thread::Builder::new()
                .name("sticker-download".into())
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    use std::io::Read;
                    let result = ureq::get(&url)
                        .call()
                        .map_err(|e| anyhow::anyhow!("{e}"))
                        .and_then(|r| {
                            let mut b = Vec::new();
                            r.into_reader().read_to_end(&mut b)?;
                            Ok(b)
                        });
                    let _ = dl_tx.send(result);
                })
                .ok();
            let data = match dl_rx.await {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    log::warn!("Sticker download failed: {e:#}");
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("Sticker download channel failed: {e:#}");
                    return Ok(());
                }
            };

            // Save locally
            let local_path = {
                let dir = std::env::current_dir().unwrap_or_default().join(MEDIA_DIR);
                let _ = std::fs::create_dir_all(&dir);
                let fname = format!(
                    "sticker_{}.webp",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                );
                let path = dir.join(&fname);
                let _ = std::fs::write(&path, &data);
                path.canonicalize()
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string()
            };

            match client
                .upload(data, wacore::download::MediaType::Image)
                .await
            {
                Ok(upload) => {
                    let msg = wa::Message {
                        sticker_message: Some(Box::new(wa::message::StickerMessage {
                            mimetype: Some("image/webp".to_string()),
                            url: Some(upload.url),
                            direct_path: Some(upload.direct_path),
                            media_key: Some(upload.media_key),
                            file_enc_sha256: Some(upload.file_enc_sha256),
                            file_sha256: Some(upload.file_sha256),
                            file_length: Some(upload.file_length),
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    match client.send_message(jid, msg).await {
                        Ok(real_id) => {
                            log::info!("Sticker sent: {real_id}");
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let sent = IncomingMessage {
                                id: real_id.clone(),
                                chat_id: chat_id.clone(),
                                sender_id: String::new(),
                                sender_name: String::new(),
                                text: None,
                                media_type: Some(crate::bridge::MediaType::Sticker),
                                poll_question: None,
                                poll_options: vec![],
                                poll_selectable: 0,
                                poll_secret: vec![],
                                poll_votes: vec![],
                                timestamp: now,
                                is_from_me: true,
                                is_forwarded: false,
                                forwarding_score: 0,
                                quoted_msg_id: None,
                                quoted_text: None,
                                quoted_sender: None,
                                reactions: vec![],
                                media_local_path: Some(local_path),
                                media_filename: None,
                                media_caption: None,
                                contact_name: None,
                                contact_vcard: None,
                                link_title: None,
                                link_description: None,
                                link_url: None,
                                link_thumbnail_path: None,
                                receipt_status: ReceiptStatus::Sent,
                                is_edited: false,
                                is_system_message: false,
                                quoted_media_path: None,
                            };
                            persist_new_message(&sent, state);
                            let _ = tx.send(WaEvent::MessageReceived(sent)).await;
                            let _ = tx
                                .send(WaEvent::MessageConfirmed {
                                    tmp_id,
                                    real_id,
                                    chat_id,
                                })
                                .await;
                        }
                        Err(e) => log::warn!("Sticker send failed: {e:#}"),
                    }
                }
                Err(e) => log::warn!("Sticker upload failed: {e:#}"),
            }
        }

        WaCommand::SaveNote { text } => {
            let _ = tokio::task::spawn_blocking(move || {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("wa_notes.txt")
                {
                    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M");
                    let _ = writeln!(f, "[{ts}] {text}\n");
                }
            })
            .await;
        }

        // ── Message editing ──────────────────────────────────────────────────
        WaCommand::EditMessage {
            chat_id,
            msg_id,
            new_text,
        } => {
            let jid: Jid = chat_id.parse()?;
            let new_content = wa::Message {
                conversation: Some(new_text.clone()),
                ..Default::default()
            };
            match client.edit_message(jid, &msg_id, new_content).await {
                Ok(_) => {
                    // Update local cache
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            if let Some(m) = msgs.iter_mut().find(|m| m.id == msg_id) {
                                m.text = Some(new_text.clone());
                                m.is_edited = true;
                            }
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx
                        .send(WaEvent::MessageEdited {
                            chat_id,
                            msg_id,
                            new_text,
                        })
                        .await;
                }
                Err(e) => {
                    log::warn!("EditMessage failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to edit message: {e}")))
                        .await;
                }
            }
        }

        // ── Group management ─────────────────────────────────────────────────
        WaCommand::AddGroupParticipant { chat_id, phone } => {
            let jid: Jid = chat_id.parse()?;
            let normalized = phone.replace(['+', ' ', '-', '(', ')'], "");
            let participant_jid = format!("{normalized}@s.whatsapp.net");
            let p_jid: Jid = participant_jid.parse()?;
            match client.groups().add_participants(&jid, &[p_jid]).await {
                Ok(_) => {
                    log::info!("Added {normalized} to group {chat_id}");
                    // Inject system notification message
                    let name = {
                        let s = state.lock().unwrap();
                        resolve_sender_name(&s, &participant_jid)
                    };
                    let sys_msg = make_system_message(
                        &chat_id,
                        &format!("You added {name}"),
                    );
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(sys_msg)).await;
                    // Refresh member list
                    if let Ok(meta) = client.groups().get_metadata(&jid).await {
                        let members: Vec<crate::bridge::GroupMember> = meta
                            .participants
                            .iter()
                            .map(|p| {
                                let s = state.lock().unwrap();
                                let pjid = p.jid.to_string();
                                let name = resolve_sender_name(&s, &pjid);
                                crate::bridge::GroupMember {
                                    jid: pjid,
                                    name,
                                    is_admin: p.is_admin,
                                }
                            })
                            .collect();
                        let _ = tx.send(WaEvent::GroupMembers { chat_id, members }).await;
                    }
                }
                Err(e) => {
                    log::warn!("AddGroupParticipant failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to add member: {e}")))
                        .await;
                }
            }
        }
        WaCommand::RemoveGroupParticipant {
            chat_id,
            jid: member_jid,
        } => {
            let group_jid: Jid = chat_id.parse()?;
            let p_jid: Jid = member_jid.parse()?;
            match client
                .groups()
                .remove_participants(&group_jid, &[p_jid])
                .await
            {
                Ok(_) => {
                    log::info!("Removed {member_jid} from group {chat_id}");
                    // Inject system notification
                    let name = {
                        let s = state.lock().unwrap();
                        resolve_sender_name(&s, &member_jid)
                    };
                    let sys_msg = make_system_message(&chat_id, &format!("You removed {name}"));
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(sys_msg)).await;
                    if let Ok(meta) = client.groups().get_metadata(&group_jid).await {
                        let members: Vec<crate::bridge::GroupMember> = meta
                            .participants
                            .iter()
                            .map(|p| {
                                let s = state.lock().unwrap();
                                let pjid = p.jid.to_string();
                                let name = resolve_sender_name(&s, &pjid);
                                crate::bridge::GroupMember {
                                    jid: pjid,
                                    name,
                                    is_admin: p.is_admin,
                                }
                            })
                            .collect();
                        let _ = tx.send(WaEvent::GroupMembers { chat_id, members }).await;
                    }
                }
                Err(e) => {
                    log::warn!("RemoveGroupParticipant failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to remove member: {e}")))
                        .await;
                }
            }
        }
        WaCommand::PromoteGroupAdmin {
            chat_id,
            jid: member_jid,
        } => {
            let group_jid: Jid = chat_id.parse()?;
            let p_jid: Jid = member_jid.parse()?;
            match client
                .groups()
                .promote_participants(&group_jid, &[p_jid])
                .await
            {
                Ok(_) => {
                    log::info!("Promoted {member_jid} to admin in {chat_id}");
                    let name = {
                        let s = state.lock().unwrap();
                        resolve_sender_name(&s, &member_jid)
                    };
                    let sys_msg = make_system_message(&chat_id, &format!("You made {name} an admin"));
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(sys_msg)).await;
                }
                Err(e) => {
                    log::warn!("PromoteGroupAdmin failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to promote admin: {e}")))
                        .await;
                }
            }
        }
        WaCommand::DemoteGroupAdmin {
            chat_id,
            jid: member_jid,
        } => {
            let group_jid: Jid = chat_id.parse()?;
            let p_jid: Jid = member_jid.parse()?;
            match client
                .groups()
                .demote_participants(&group_jid, &[p_jid])
                .await
            {
                Ok(_) => {
                    log::info!("Demoted {member_jid} from admin in {chat_id}");
                    let name = {
                        let s = state.lock().unwrap();
                        resolve_sender_name(&s, &member_jid)
                    };
                    let sys_msg = make_system_message(&chat_id, &format!("You removed {name} as admin"));
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(sys_msg)).await;
                }
                Err(e) => {
                    log::warn!("DemoteGroupAdmin failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to demote admin: {e}")))
                        .await;
                }
            }
        }
        WaCommand::LeaveGroup { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            match client.groups().leave(&jid).await {
                Ok(_) => {
                    log::info!("Left group {chat_id}");
                    // Remove from local state
                    {
                        let mut s = state.lock().unwrap();
                        s.chats.retain(|c| c.id != chat_id);
                        let _ = s.save_tx.send(s.chats.clone());
                    }
                    let chats = state.lock().unwrap().chats_with_best_names();
                    let _ = tx.send(WaEvent::ChatsLoaded(chats)).await;
                }
                Err(e) => {
                    log::warn!("LeaveGroup failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to leave group: {e}")))
                        .await;
                }
            }
        }

        WaCommand::GetGroupInviteLink { chat_id } => {
            let jid: Jid = chat_id.parse()?;
            match client.groups().get_invite_link(&jid, false).await {
                Ok(link) => {
                    let _ = tx.send(WaEvent::GroupInviteLink { chat_id, link }).await;
                }
                Err(e) => {
                    log::warn!("GetGroupInviteLink failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!(
                            "Failed to get invite link: {e}"
                        )))
                        .await;
                }
            }
        }

        // ── Disappearing messages ────────────────────────────────────────────
        WaCommand::SetDisappearing {
            chat_id,
            duration_secs,
        } => {
            let jid: Jid = chat_id.parse()?;
            match client.groups().set_ephemeral(&jid, duration_secs).await {
                Ok(_) => log::info!("Set disappearing={duration_secs}s for {chat_id}"),
                Err(e) => {
                    log::warn!("SetDisappearing failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!(
                            "Failed to set disappearing messages: {e}"
                        )))
                        .await;
                }
            }
        }

        // ── Voice note / audio ──────────────────────────────────────────────
        WaCommand::SendAudio {
            chat_id,
            path,
            duration_secs,
            is_voice_note,
            tmp_id,
        } => {
            let jid: Jid = chat_id.parse()?;
            match tokio::fs::read(&path).await {
                Ok(data) => {
                    let mimetype = if path.ends_with(".ogg") || path.ends_with(".opus") {
                        "audio/ogg; codecs=opus"
                    } else {
                        "audio/mpeg"
                    };
                    // Generate waveform data for voice notes.
                    // WhatsApp expects a byte array where each byte (0-100) represents
                    // the audio amplitude for a time slice. We sample ~64 bins.
                    let waveform: Option<Vec<u8>> = if is_voice_note {
                        Some(generate_waveform(&data, 64))
                    } else {
                        None
                    };

                    // Upload, then build + send the AudioMessage.
                    // PTT (voice notes) use a different upload endpoint ("ptt")
                    // than regular audio files ("audio"). Using the wrong endpoint
                    // makes the media unretrievable for the recipient.
                    let upload_type = if is_voice_note {
                        wacore::download::MediaType::Ptt
                    } else {
                        wacore::download::MediaType::Audio
                    };
                    match client
                        .upload(data, upload_type)
                        .await
                    {
                        Ok(upload) => {
                            let now_ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let msg = wa::Message {
                                audio_message: Some(Box::new(wa::message::AudioMessage {
                                    url: Some(upload.url),
                                    direct_path: Some(upload.direct_path),
                                    media_key: Some(upload.media_key),
                                    file_sha256: Some(upload.file_sha256),
                                    file_enc_sha256: Some(upload.file_enc_sha256),
                                    file_length: Some(upload.file_length),
                                    mimetype: Some(mimetype.to_string()),
                                    ptt: Some(is_voice_note),
                                    seconds: Some(duration_secs),
                                    media_key_timestamp: Some(now_ts),
                                    waveform,
                                    ..Default::default()
                                })),
                                ..Default::default()
                            };
                            match client.send_message(jid, msg).await {
                                Ok(real_id) => {
                                    let _ = tx
                                        .send(WaEvent::MessageConfirmed {
                                            tmp_id,
                                            real_id,
                                            chat_id,
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    log::warn!("SendAudio send failed: {e:#}");
                                    let _ = tx
                                        .send(WaEvent::MessageFailed {
                                            msg_id: tmp_id,
                                            chat_id,
                                        })
                                        .await;
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("SendAudio upload failed: {e:#}");
                            let _ = tx
                                .send(WaEvent::MessageFailed {
                                    msg_id: tmp_id,
                                    chat_id,
                                })
                                .await;
                        }
                    }
                }
                Err(e) => {
                    log::warn!("SendAudio: couldn't read file {path}: {e}");
                    let _ = tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                }
            }
        }

        // ── Calls (stub — backend WebRTC support TBD) ───────────────────────
        WaCommand::InitiateCall { chat_id, is_video } => {
            let kind = if is_video { "video" } else { "voice" };
            log::info!("InitiateCall: {kind} call to {chat_id}");
            let _ = tx
                .send(WaEvent::ErrorToast(format!(
                    "{} calls are not yet supported by the protocol library",
                    if is_video { "Video" } else { "Voice" }
                )))
                .await;
        }
        WaCommand::AcceptCall { chat_id } => {
            log::info!("AcceptCall: {chat_id}");
        }
        WaCommand::RejectCall { chat_id } => {
            log::info!("RejectCall: {chat_id}");
        }
        WaCommand::EndCall { chat_id } => {
            log::info!("EndCall: {chat_id}");
        }

        // ── Global search ───────────────────────────────────────────────────
        WaCommand::SearchAllMessages { query } => {
            let results = search_local_messages(&query);
            let _ = tx
                .send(WaEvent::GlobalSearchResults { query, results })
                .await;
        }

        // ── Multi-send with anti-ban delays ─────────────────────────────────
        WaCommand::MultiSend { chat_ids, text } => {
            let total = chat_ids.len() as u32;
            let mut sent = 0u32;
            let mut failed = 0u32;
            for (i, cid) in chat_ids.iter().enumerate() {
                if i > 0 {
                    // Randomised delay 2–6s between sends to avoid ban detection
                    let delay_ms = {
                        use std::collections::hash_map::DefaultHasher;
                        use std::hash::{Hash, Hasher};
                        let mut h = DefaultHasher::new();
                        cid.hash(&mut h);
                        i.hash(&mut h);
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis()
                            .hash(&mut h);
                        (h.finish() % 4000 + 2000) as u64
                    };
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let _ = tx
                    .send(WaEvent::MultiSendProgress {
                        sent: sent + 1,
                        total,
                        current_chat: cid.clone(),
                    })
                    .await;
                let jid: Jid = match cid.parse() {
                    Ok(j) => j,
                    Err(e) => {
                        log::warn!("MultiSend: invalid JID {cid}: {e}");
                        failed += 1;
                        continue;
                    }
                };
                let msg = wa::Message {
                    conversation: Some(text.clone()),
                    ..Default::default()
                };
                match client.send_message(jid, msg).await {
                    Ok(real_id) => {
                        sent += 1;
                        log::info!("MultiSend to {cid} OK → {real_id}");
                        // Emit self-message event so it appears in the target chat locally
                        let now_ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        let self_msg = IncomingMessage::outgoing(
                            real_id.to_string(),
                            cid.clone(),
                            Some(text.clone()),
                            now_ts,
                        );
                        // Add to in-memory cache + queue background disk write
                        {
                            let mut s = state.lock().unwrap();
                            s.history
                                .entry(cid.clone())
                                .or_default()
                                .push(self_msg.clone());
                            s.queue_save_messages(cid);
                        }
                        let _ = tx.send(WaEvent::MessageReceived(self_msg)).await;
                    }
                    Err(e) => {
                        log::warn!("MultiSend to {cid} failed: {e:#}");
                        failed += 1;
                    }
                }
            }
            let _ = tx.send(WaEvent::MultiSendComplete { sent, failed }).await;
        }

        WaCommand::MultiForward {
            chat_ids,
            original_msg_id,
        } => {
            // Forward = re-send with forwarded flag. Look up original message text
            // from cache and send as a new message with forwarding_score.
            let original_text = {
                let s = state.lock().unwrap();
                s.history
                    .values()
                    .flat_map(|msgs| msgs.iter())
                    .find(|m| m.id == original_msg_id)
                    .and_then(|m| m.text.clone())
                    .unwrap_or_default()
            };
            if original_text.is_empty() {
                let _ = tx
                    .send(WaEvent::ErrorToast(
                        "Cannot forward: original message not found in cache".to_string(),
                    ))
                    .await;
                let _ = tx
                    .send(WaEvent::MultiSendComplete { sent: 0, failed: chat_ids.len() as u32 })
                    .await;
            } else {
                let total = chat_ids.len() as u32;
                let mut sent = 0u32;
                let mut failed = 0u32;
                for (i, cid) in chat_ids.iter().enumerate() {
                    if i > 0 {
                        let delay_ms = {
                            use std::collections::hash_map::DefaultHasher;
                            use std::hash::{Hash, Hasher};
                            let mut h = DefaultHasher::new();
                            cid.hash(&mut h);
                            i.hash(&mut h);
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis()
                                .hash(&mut h);
                            (h.finish() % 4000 + 2000) as u64
                        };
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                    let _ = tx
                        .send(WaEvent::MultiSendProgress {
                            sent: sent + 1,
                            total,
                            current_chat: cid.clone(),
                        })
                        .await;
                    let jid: Jid = match cid.parse() {
                        Ok(j) => j,
                        Err(e) => {
                            log::warn!("MultiForward: invalid JID {cid}: {e}");
                            failed += 1;
                            continue;
                        }
                    };
                    let msg = wa::Message {
                        conversation: Some(original_text.clone()),
                        ..Default::default()
                    };
                    match client.send_message(jid, msg).await {
                        Ok(real_id) => {
                            sent += 1;
                            // Emit self-message so it appears locally in target chat
                            let now_ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let self_msg = IncomingMessage::outgoing(
                                real_id.to_string(),
                                cid.clone(),
                                Some(original_text.clone()),
                                now_ts,
                            );
                            {
                                let mut s = state.lock().unwrap();
                                s.history.entry(cid.clone()).or_default().push(self_msg.clone());
                                s.queue_save_messages(cid);
                            }
                            let _ = tx.send(WaEvent::MessageReceived(self_msg)).await;
                        }
                        Err(e) => {
                            log::warn!("MultiForward to {cid} failed: {e:#}");
                            failed += 1;
                        }
                    }
                }
                let _ = tx.send(WaEvent::MultiSendComplete { sent, failed }).await;
            }
        }
    }
    Ok(())
}

/// Search through locally cached messages for a query string.
fn search_local_messages(query: &str) -> Vec<crate::bridge::SearchHit> {
    use crate::bridge::SearchHit;
    let mut hits: Vec<SearchHit> = Vec::new();
    let query_lower = query.to_lowercase();

    let cache_dir = std::path::PathBuf::from("wa_messages");
    if !cache_dir.exists() {
        return hits;
    }
    let entries = match std::fs::read_dir(&cache_dir) {
        Ok(e) => e,
        Err(_) => return hits,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Skip the 4-byte BIN_HEADER ("WA02") prepended by write_bin_path
        if data.len() < 4 {
            continue;
        }
        let messages: Vec<crate::bridge::IncomingMessage> =
            match bincode::deserialize(&data[4..]) {
                Ok(m) => m,
                Err(_) => continue,
            };
        let chat_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        for msg in &messages {
            if let Some(text) = &msg.text {
                if text.to_lowercase().contains(&query_lower) {
                    hits.push(SearchHit {
                        chat_id: chat_id.clone(),
                        chat_name: String::new(),
                        msg_id: msg.id.clone(),
                        sender_name: msg.sender_name.clone(),
                        text: text.clone(),
                        timestamp: msg.timestamp,
                    });
                }
            }
        }
    }
    hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    hits.truncate(100);
    hits
}

/// Generate a waveform byte array for a voice note.
/// Each byte (0-100) represents the audio amplitude for a time slice.
/// We analyze the raw Ogg/Opus file bytes by sampling the energy in chunks.
fn generate_waveform(audio_data: &[u8], num_bins: usize) -> Vec<u8> {
    if audio_data.is_empty() || num_bins == 0 {
        return vec![30; num_bins.max(1)]; // fallback: gentle baseline
    }

    let chunk_size = (audio_data.len() / num_bins).max(1);
    let mut bins: Vec<f64> = Vec::with_capacity(num_bins);

    for i in 0..num_bins {
        let start = i * chunk_size;
        let end = (start + chunk_size).min(audio_data.len());
        if start >= audio_data.len() {
            bins.push(0.0);
            continue;
        }
        // RMS energy of raw bytes (treats them as unsigned audio samples)
        let sum_sq: f64 = audio_data[start..end]
            .iter()
            .map(|&b| {
                let sample = (b as f64) - 128.0; // center around zero
                sample * sample
            })
            .sum();
        let rms = (sum_sq / (end - start) as f64).sqrt();
        bins.push(rms);
    }

    // Normalize to 0-100 range
    let max_rms = bins.iter().cloned().fold(0.0_f64, f64::max).max(1.0);
    bins.iter()
        .map(|&v| {
            let normalized = (v / max_rms * 80.0 + 10.0).round() as u8; // 10-90 range
            normalized.clamp(5, 100)
        })
        .collect()
}

// ── Media download ────────────────────────────────────────────────────────────

const MEDIA_DIR: &str = "wa_media";
const AVATARS_DIR: &str = "wa_avatars";

/// Cloned info from a proto sub-message needed to download + display media.
enum PendingDownload {
    Image {
        msg: Box<wa::message::ImageMessage>,
    },
    Video {
        msg: Box<wa::message::VideoMessage>,
    },
    Document {
        msg: Box<wa::message::DocumentMessage>,
        filename: Option<String>,
    },
    Audio {
        msg: Box<wa::message::AudioMessage>,
    },
    Sticker {
        msg: Box<wa::message::StickerMessage>,
    },
}

fn extract_pending_download(base: &wa::Message) -> Option<PendingDownload> {
    if let Some(img) = &base.image_message {
        if img.direct_path.is_some() {
            return Some(PendingDownload::Image { msg: img.clone() });
        }
    }
    if let Some(vid) = &base.video_message {
        if vid.direct_path.is_some() {
            return Some(PendingDownload::Video { msg: vid.clone() });
        }
    }
    if let Some(doc) = &base.document_message {
        if doc.direct_path.is_some() {
            let filename = doc.file_name.clone();
            return Some(PendingDownload::Document {
                msg: doc.clone(),
                filename,
            });
        }
    }
    if let Some(aud) = &base.audio_message {
        if aud.direct_path.is_some() {
            return Some(PendingDownload::Audio { msg: aud.clone() });
        }
    }
    if let Some(stk) = &base.sticker_message {
        if stk.direct_path.is_some() {
            return Some(PendingDownload::Sticker { msg: stk.clone() });
        }
    }
    None
}

fn ext_from_mime(mime: Option<&str>, default: &str) -> String {
    mime.and_then(|m| m.split('/').nth(1))
        .and_then(|s| s.split(';').next())
        .unwrap_or(default)
        .to_string()
}

fn bridge_media_type_from_pending(dl: &PendingDownload) -> crate::bridge::MediaType {
    match dl {
        PendingDownload::Image { .. } => crate::bridge::MediaType::Image,
        PendingDownload::Video { .. } => crate::bridge::MediaType::Video,
        PendingDownload::Document { .. } => crate::bridge::MediaType::Document,
        PendingDownload::Audio { .. } => crate::bridge::MediaType::Audio,
        PendingDownload::Sticker { .. } => crate::bridge::MediaType::Sticker,
    }
}

async fn execute_media_download(
    client: Arc<Client>,
    tx: Sender<WaEvent>,
    state: &Arc<Mutex<RuntimeState>>,
    msg_id: String,
    chat_id: String,
    dl: PendingDownload,
) {
    let media_type = bridge_media_type_from_pending(&dl);

    let result: anyhow::Result<(Vec<u8>, String, Option<String>)> = async {
        Ok(match &dl {
            PendingDownload::Image { msg } => {
                let bytes = client.download(msg.as_ref() as &dyn Downloadable).await?;
                let ext = ext_from_mime(msg.mimetype.as_deref(), "jpg");
                (bytes, ext, None)
            }
            PendingDownload::Video { msg } => {
                let bytes = client.download(msg.as_ref() as &dyn Downloadable).await?;
                let ext = ext_from_mime(msg.mimetype.as_deref(), "mp4");
                (bytes, ext, None)
            }
            PendingDownload::Document { msg, filename } => {
                let bytes = client.download(msg.as_ref() as &dyn Downloadable).await?;
                let ext = filename
                    .as_deref()
                    .and_then(|f| f.rsplit('.').next())
                    .unwrap_or("bin")
                    .to_string();
                (bytes, ext, filename.clone())
            }
            PendingDownload::Audio { msg } => {
                let bytes = client.download(msg.as_ref() as &dyn Downloadable).await?;
                let ext = ext_from_mime(msg.mimetype.as_deref(), "ogg");
                (bytes, ext, None)
            }
            PendingDownload::Sticker { msg } => {
                let bytes = client.download(msg.as_ref() as &dyn Downloadable).await?;
                (bytes, "webp".to_string(), None)
            }
        })
    }
    .await;

    let (bytes, ext, orig_filename) = match result {
        Ok(v) => v,
        Err(e) => {
            log::warn!("Media download failed for {msg_id}: {e:#}");
            return;
        }
    };

    let dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(MEDIA_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Cannot create media dir: {e}");
        return;
    }

    // Use msg_id as the base name, with original filename appended for documents
    let local_name = match &orig_filename {
        Some(fname) => {
            let safe: String = fname
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '.' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!("{}_{}", &msg_id[..8.min(msg_id.len())], safe)
        }
        None => format!("{msg_id}.{ext}"),
    };
    let path = dir.join(&local_name);

    if let Err(e) = std::fs::write(&path, &bytes) {
        log::warn!("Failed to save media {local_name}: {e}");
        return;
    }

    // Use absolute path so GTK Picture can find it regardless of working directory
    let path_str = path
        .canonicalize()
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    log::info!("Media saved: {path_str}");

    // Update in-memory cache and queue disk write (non-blocking)
    {
        let mut s = state.lock().unwrap();
        if let Some(history) = s.history.get_mut(&chat_id) {
            if let Some(m) = history.iter_mut().find(|m| m.id == msg_id) {
                m.media_local_path = Some(path_str.clone());
            }
            s.queue_save_messages(&chat_id);
        }
    }

    let _ = tx
        .send(WaEvent::MediaReady {
            msg_id,
            chat_id,
            path: path_str,
            media_type,
        })
        .await;
}

// ── LID deduplication ─────────────────────────────────────────────────────────

/// Merge any @lid chats into their @s.whatsapp.net counterpart.
/// WhatsApp sometimes routes history-sync messages to a LID JID rather than the
/// phone JID, creating a duplicate chat entry. This resolves those mappings from
/// the local LID-PN cache (warmed up on startup) and merges the message history.
/// Resolve all known LID JIDs to phone JIDs using the client's LID-PN cache.
/// This populates the lid_to_phone mapping in RuntimeState so that
/// resolve_sender_name can find names for LID-addressed group messages.
async fn resolve_all_lids(client: &Arc<Client>, state: &Arc<Mutex<RuntimeState>>) {
    // Collect all unique LID JIDs from contact_names keys and chat sender_ids
    let lid_jids: Vec<String> = {
        let s = state.lock().unwrap();
        let mut lids: Vec<String> = s
            .contact_names
            .keys()
            .filter(|k| k.ends_with("@lid"))
            .cloned()
            .collect();
        // Also check chats
        for c in &s.chats {
            if c.id.ends_with("@lid") && !lids.contains(&c.id) {
                lids.push(c.id.clone());
            }
        }
        lids
    };

    let mut resolved = 0u32;
    for lid_jid in &lid_jids {
        if let Some(phone_jid) = client.resolve_lid_to_phone_jid(lid_jid).await {
            let mut s = state.lock().unwrap();
            s.insert_lid_phone(lid_jid.clone(), phone_jid.clone());
            // Also store the contact name under the phone JID if we have one for the LID
            if let Some(name) = s.contact_names.get(lid_jid).cloned() {
                if !s.contact_names.contains_key(&phone_jid) {
                    s.contact_names.insert(phone_jid.clone(), name);
                }
            }
            resolved += 1;
        }
    }
    log::info!(
        "Resolved {resolved}/{} LID→phone mappings from client cache",
        lid_jids.len()
    );
    // Persist to disk so next startup has the mappings immediately
    let map = state.lock().unwrap().lid_to_phone.clone();
    if !map.is_empty() {
        std::thread::spawn(move || save_lid_phone_map(&map));
    }
}

async fn merge_lid_chats(
    client: &Arc<Client>,
    state: &Arc<Mutex<RuntimeState>>,
    tx: &Sender<WaEvent>,
) {
    let lid_chats: Vec<ChatSummary> = state
        .lock()
        .unwrap()
        .chats
        .iter()
        .filter(|c| c.id.ends_with("@lid"))
        .cloned()
        .collect();

    if lid_chats.is_empty() {
        return;
    }

    log::info!("Resolving {} @lid chat(s)…", lid_chats.len());
    let mut changed = false;

    for lid_chat in lid_chats {
        let Some(phone_jid) = client.resolve_lid_to_phone_jid(&lid_chat.id).await else {
            log::debug!("No LID→PN mapping for {}", lid_chat.id);
            continue;
        };

        log::info!("Merging {} → {}", lid_chat.id, phone_jid);

        // Store the LID→phone mapping
        state
            .lock()
            .unwrap()
            .lid_to_phone
            .insert(lid_chat.id.clone(), phone_jid.clone());

        // Load messages from both chats
        let mut lid_msgs = load_messages(&lid_chat.id);
        let mut phone_msgs = load_messages(&phone_jid);

        // Remap chat_id on lid messages and merge (dedup by id)
        let mut merged = phone_msgs.clone();
        for mut m in lid_msgs.drain(..) {
            m.chat_id = phone_jid.clone();
            if !merged.iter().any(|x| x.id == m.id) {
                merged.push(m);
            }
        }
        merged.sort_by_key(|m| m.timestamp);
        save_messages(&phone_jid, &merged);

        // Build merged summary and update state (under mutex — serialized with JoinedGroup tasks)
        let last_msg = merged
            .iter()
            .rev()
            .find(|m| m.text.is_some() || m.media_type.is_some());
        let timestamp = last_msg.map(|m| m.timestamp).unwrap_or(lid_chat.timestamp);
        let preview = last_msg.map(|m| media_preview(m)).unwrap_or_default();

        let merged_summary = {
            let mut s = state.lock().unwrap();
            let phone_name = if let Some(existing) = s.chats.iter().find(|c| c.id == phone_jid) {
                if existing.name.contains('@') {
                    lid_chat.name.clone()
                } else {
                    existing.name.clone()
                }
            } else {
                lid_chat.name.clone()
            };
            let summary = ChatSummary {
                id: phone_jid.clone(),
                name: phone_name,
                last_message: preview,
                timestamp,
                unread_count: lid_chat.unread_count,
                is_group: false,
                is_muted: lid_chat.is_muted,
                is_pinned: lid_chat.is_pinned,
                is_archived: lid_chat.is_archived,
                is_favorite: lid_chat.is_favorite,
                label: lid_chat.label.clone(),
                pinned_msg_id: lid_chat.pinned_msg_id.clone(),
            };
            // Remove @lid entry, upsert @s.whatsapp.net — all within the same lock
            s.chats.retain(|c| c.id != lid_chat.id);
            s.chat_names.remove(&lid_chat.id);
            s.upsert_chat(summary.clone());
            s.history.remove(&lid_chat.id);
            s.history.insert(phone_jid.clone(), merged.clone());
            summary
        };

        // Delete the @lid messages file
        let _ = std::fs::remove_file(messages_file(&lid_chat.id));

        let _ = merged_summary; // used above
        changed = true;
    }

    if changed {
        // Persist updated LID→phone map so resolved chats survive restart
        let map = state.lock().unwrap().lid_to_phone.clone();
        if !map.is_empty() {
            std::thread::spawn(move || save_lid_phone_map(&map));
        }
        let chats = state.lock().unwrap().chats_with_best_names();
        let _ = tx.send(WaEvent::ChatsLoaded(chats)).await;
    }
}

// ── Group name refresh ────────────────────────────────────────────────────────

async fn fetch_and_update_group_names(
    client: &Arc<Client>,
    state: &Arc<Mutex<RuntimeState>>,
    tx: &Sender<WaEvent>,
) {
    log::info!("Fetching group names from server…");
    let mut resolved_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    match client.groups().get_participating().await {
        Ok(groups) => {
            // Collect all updates first, then apply in a single lock
            let mut name_updates: Vec<(String, String)> = Vec::new();
            let mut lid_updates: Vec<(String, String)> = Vec::new();

            for (jid_str, meta) in &groups {
                let name = &meta.subject;
                if !name.is_empty() {
                    resolved_ids.insert(jid_str.clone());
                    name_updates.push((jid_str.clone(), name.clone()));
                }
                for p in &meta.participants {
                    let participant_jid = p.jid.to_string();
                    if let Some(phone_jid) = &p.phone_number {
                        let phone_str = phone_jid.to_string();
                        if participant_jid.ends_with("@lid")
                            && phone_str.ends_with("@s.whatsapp.net")
                        {
                            lid_updates.push((participant_jid, phone_str));
                        }
                    }
                }
            }

            // Single lock to apply all collected updates
            {
                let mut s = state.lock().unwrap();
                for (jid_str, name) in &name_updates {
                    s.rename_chat(jid_str, name);
                }
                let mut lid_phone_count = 0u32;
                for (lid_jid, phone_str) in &lid_updates {
                    if !s.lid_to_phone.contains_key(lid_jid) {
                        s.insert_lid_phone(lid_jid.clone(), phone_str.clone());
                        if let Some(lid_name) = s.contact_names.get(lid_jid).cloned() {
                            if !s.contact_names.contains_key(phone_str) {
                                s.contact_names.insert(phone_str.clone(), lid_name);
                            }
                        }
                        if let Some(phone_name) = s.contact_names.get(phone_str).cloned() {
                            if !s.contact_names.contains_key(lid_jid) {
                                s.contact_names.insert(lid_jid.clone(), phone_name);
                            }
                        }
                        lid_phone_count += 1;
                    }
                }
                log::info!(
                    "Group name refresh complete ({} groups, {lid_phone_count} new LID→phone from participants)",
                    groups.len()
                );
            }

            // Send UI updates outside the lock
            for (jid_str, name) in name_updates {
                let _ = tx
                    .send(WaEvent::ChatNameUpdated {
                        chat_id: jid_str,
                        name,
                    })
                    .await;
            }

            // Individually query unresolved groups — limit to 5 to avoid long delays
            let unresolved: Vec<String> = {
                let s = state.lock().unwrap();
                s.chats
                    .iter()
                    .filter(|c| c.id.ends_with("@g.us"))
                    .filter(|c| !resolved_ids.contains(&c.id))
                    .map(|c| c.id.clone())
                    .take(20)
                    .collect()
            };
            if !unresolved.is_empty() {
                log::info!(
                    "Querying {} unresolved group name(s) individually…",
                    unresolved.len()
                );
                for gid in &unresolved {
                    if let Ok(jid) = gid.parse::<Jid>() {
                        match client.groups().get_metadata(&jid).await {
                            Ok(meta) => {
                                if !meta.subject.is_empty() {
                                    log::info!("Resolved group {gid} → {:?}", meta.subject);
                                    state.lock().unwrap().rename_chat(gid, &meta.subject);
                                    let _ = tx
                                        .send(WaEvent::ChatNameUpdated {
                                            chat_id: gid.clone(),
                                            name: meta.subject,
                                        })
                                        .await;
                                }
                            }
                            Err(e) => log::debug!("Failed to query group {gid}: {e:#}"),
                        }
                    }
                }
            }

            // Save enriched mappings
            {
                let s = state.lock().unwrap();
                let map = s.lid_to_phone.clone();
                let names = s.contact_names.clone();
                std::thread::spawn(move || {
                    save_lid_phone_map(&map);
                    save_contact_names(&names);
                });
            }
        }
        Err(e) => log::warn!("Failed to fetch group names: {e:#}"),
    }
}

// ── Profile picture fetch ─────────────────────────────────────────────────────

/// Serve profile pictures that are already cached on disk, without any network requests.
/// Called immediately after connect so avatars appear without waiting for sync to finish.
async fn serve_cached_avatars(state: &Arc<Mutex<RuntimeState>>, tx: &Sender<WaEvent>) {
    let chats = state.lock().unwrap().chats.clone();
    let avatar_dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(AVATARS_DIR);

    for chat in &chats {
        let safe_id = chat.id.replace(['/', '\\', '@', ':'], "_");
        let avatar_path = avatar_dir.join(format!("{safe_id}.jpg"));
        if avatar_path.exists() {
            if let Ok(abs) = avatar_path.canonicalize() {
                let _ = tx
                    .send(WaEvent::AvatarReady {
                        chat_id: chat.id.clone(),
                        path: abs.to_string_lossy().to_string(),
                    })
                    .await;
            }
        }
    }
}

/// Fetch profile pictures for all known chats, caching to disk.
/// Only called after OfflineSyncCompleted — does not interfere with message sync.
async fn fetch_profile_pictures(
    client: &Arc<Client>,
    state: &Arc<Mutex<RuntimeState>>,
    tx: &Sender<WaEvent>,
) {
    let chats = state.lock().unwrap().chats.clone();
    let avatar_dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(AVATARS_DIR);
    let _ = std::fs::create_dir_all(&avatar_dir);

    // Fetch own avatar using BOTH the phone JID path AND "me.jpg"
    // so message bubbles find it under the own_jid sent to ChatView.
    let own_pn = client.get_pn().await;
    let own_pn_str = own_pn.as_ref().map(|j| {
        let s = j.to_string();
        // Strip device suffix (e.g., "16478223279:82@s.whatsapp.net" → "16478223279@s.whatsapp.net")
        if let Some(colon) = s.find(':') {
            if let Some(at) = s.find('@') {
                return format!("{}{}", &s[..colon], &s[at..]);
            }
        }
        s
    });
    log::info!("Own avatar: get_pn={own_pn_str:?}");

    // Determine the avatar file path for our own JID
    let own_safe = own_pn_str
        .as_ref()
        .map(|s| s.replace(['/', '\\', '@', ':'], "_"));
    let own_avatar_path = own_safe
        .as_ref()
        .map(|s| avatar_dir.join(format!("{s}.jpg")));

    if let Some(ref avatar_path) = own_avatar_path {
        if !avatar_path.exists() {
            // Fetch own profile picture from server
            if let Some(ref pn) = own_pn_str {
                if let Ok(jid) = pn.parse::<Jid>() {
                    match client.contacts().get_profile_picture(&jid, true).await {
                        Ok(Some(pic)) => {
                            let url = pic.url.clone();
                            let dest = avatar_path.clone();
                            let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                                use std::io::Read;
                                let resp =
                                    ureq::get(&url).call().map_err(|e| anyhow::anyhow!("{e}"))?;
                                let mut bytes = Vec::new();
                                resp.into_reader().read_to_end(&mut bytes)?;
                                std::fs::write(&dest, &bytes)?;
                                Ok(())
                            })
                            .await;
                            log::info!("Own avatar saved to {}", avatar_path.display());
                        }
                        Ok(None) => log::info!("Own profile picture not set on WhatsApp"),
                        Err(e) => log::warn!("Failed to fetch own avatar: {e:#}"),
                    }
                }
            }
        }
    }

    for chat in &chats {
        let safe_id = chat.id.replace(['/', '\\', '@', ':'], "_");
        let avatar_path = avatar_dir.join(format!("{safe_id}.jpg"));

        if avatar_path.exists() {
            // Serve cached picture immediately
            if let Ok(abs) = avatar_path.canonicalize() {
                let _ = tx
                    .send(WaEvent::AvatarReady {
                        chat_id: chat.id.clone(),
                        path: abs.to_string_lossy().to_string(),
                    })
                    .await;
            }
            continue;
        }

        // Strip device suffix (e.g., ":12@s.whatsapp.net" → "@s.whatsapp.net")
        let clean_id = if let (Some(colon), Some(at)) = (chat.id.find(':'), chat.id.find('@')) {
            if colon < at {
                format!("{}{}", &chat.id[..colon], &chat.id[at..])
            } else {
                chat.id.clone()
            }
        } else {
            chat.id.clone()
        };
        let jid: Jid = match clean_id.parse() {
            Ok(j) => j,
            Err(_) => continue,
        };

        match client.contacts().get_profile_picture(&jid, true).await {
            Ok(Some(pic)) => {
                let url = pic.url.clone();
                let dest = avatar_path.clone();
                let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    use std::io::Read;
                    let resp = ureq::get(&url).call().map_err(|e| anyhow::anyhow!("{e}"))?;
                    let mut bytes = Vec::new();
                    resp.into_reader().read_to_end(&mut bytes)?;
                    std::fs::write(&dest, bytes)?;
                    Ok(())
                })
                .await;

                match result {
                    Ok(Ok(())) => {
                        if let Ok(abs) = avatar_path.canonicalize() {
                            log::info!("Avatar saved for {}: {}", chat.id, abs.display());
                            let _ = tx
                                .send(WaEvent::AvatarReady {
                                    chat_id: chat.id.clone(),
                                    path: abs.to_string_lossy().to_string(),
                                })
                                .await;
                        }
                    }
                    Ok(Err(e)) => log::warn!("Avatar download failed for {}: {e:#}", chat.id),
                    Err(e) => log::warn!("Avatar join failed for {}: {e}", chat.id),
                }
            }
            Ok(None) => log::debug!("No profile picture for {}", chat.id),
            Err(e) => log::info!("get_profile_picture for {}: {e:#}", chat.id),
        }

        // Rate-limit requests to avoid throttling
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
    log::info!("Profile picture fetch done ({} chats)", chats.len());
}

// ── Message mappers ───────────────────────────────────────────────────────────

fn map_history_message(h: &wa::HistorySyncMsg, chat_id: &str) -> Option<IncomingMessage> {
    let web_msg = h.message.as_ref()?;
    let key = &web_msg.key;
    let msg_id = key.id.clone()?;
    let from_me = key.from_me.unwrap_or(false);
    let sender_id = if from_me {
        String::new()
    } else {
        // Try key.participant first, then web_msg.participant (LID groups),
        // then remote_jid as last resort
        key.participant
            .clone()
            .or_else(|| web_msg.participant.clone())
            .filter(|p| !p.is_empty() && p != chat_id) // Don't use group JID as sender
            .unwrap_or_default()
    };
    let sender_name = web_msg.push_name.clone().unwrap_or_default();
    let timestamp = web_msg.message_timestamp.unwrap_or(0) as i64;

    let (
        text,
        media_type,
        media_filename,
        media_caption,
        contact_name,
        contact_vcard,
        link_title,
        link_description,
        link_url,
    ) = if let Some(proto_msg) = web_msg.message.as_ref() {
        let base = proto_msg.get_base_message();
        let t = proto_msg.text_content().map(|s| s.to_string());
        let mt = extract_media_type(base);
        let fname = base
            .document_message
            .as_deref()
            .and_then(|d| d.file_name.clone());
        let cap = base
            .image_message
            .as_deref()
            .and_then(|i| i.caption.clone())
            .or_else(|| {
                base.video_message
                    .as_deref()
                    .and_then(|v| v.caption.clone())
            })
            .or_else(|| {
                base.document_message
                    .as_deref()
                    .and_then(|d| d.caption.clone())
            });
        let cn = base
            .contact_message
            .as_deref()
            .and_then(|c| c.display_name.clone());
        let cv = base
            .contact_message
            .as_deref()
            .and_then(|c| c.vcard.clone());
        let lt = base
            .extended_text_message
            .as_deref()
            .and_then(|e| e.title.clone());
        let ld = base
            .extended_text_message
            .as_deref()
            .and_then(|e| e.description.clone());
        let lu = base
            .extended_text_message
            .as_deref()
            .and_then(|e| e.matched_text.clone());
        (t, mt, fname, cap, cn, cv, lt, ld, lu)
    } else {
        (None, None, None, None, None, None, None, None, None)
    };

    // Skip protocol messages with no displayable content
    if text.is_none() && media_type.is_none() && media_caption.is_none() && contact_name.is_none() {
        return None;
    }

    // For contact messages, set text to display name
    let text = text.or_else(|| contact_name.as_ref().map(|n| format!("📇 {n}")));

    // Extract quoted message context from history sync
    let (quoted_msg_id, quoted_sender, quoted_text_hist, is_forwarded_hist, forwarding_score_hist) =
        if let Some(proto_msg) = web_msg.message.as_ref() {
            let base = proto_msg.get_base_message();
            let ctx = base
                .extended_text_message
                .as_deref()
                .and_then(|m| m.context_info.as_deref())
                .or_else(|| {
                    base.image_message
                        .as_deref()
                        .and_then(|m| m.context_info.as_deref())
                })
                .or_else(|| {
                    base.video_message
                        .as_deref()
                        .and_then(|m| m.context_info.as_deref())
                })
                .or_else(|| {
                    base.audio_message
                        .as_deref()
                        .and_then(|m| m.context_info.as_deref())
                })
                .or_else(|| {
                    base.document_message
                        .as_deref()
                        .and_then(|m| m.context_info.as_deref())
                });
            let qid = ctx.and_then(|c| c.stanza_id.clone());
            let qs = ctx.and_then(|c| c.participant.clone());
            let qt = ctx.and_then(|c| {
                let qm = c.quoted_message.as_deref()?;
                if let Some(t) = qm.text_content() {
                    return Some(t.to_string());
                }
                let qbase = qm.get_base_message();
                if let Some(img) = &qbase.image_message {
                    return Some(img.caption.clone().unwrap_or("📷 Photo".to_string()));
                }
                if let Some(vid) = &qbase.video_message {
                    return Some(vid.caption.clone().unwrap_or("🎥 Video".to_string()));
                }
                if qbase.audio_message.is_some() {
                    return Some("🎵 Audio".to_string());
                }
                if let Some(doc) = &qbase.document_message {
                    return Some(format!(
                        "📄 {}",
                        doc.file_name.as_deref().unwrap_or("Document")
                    ));
                }
                if qbase.sticker_message.is_some() {
                    return Some("🎭 Sticker".to_string());
                }
                None
            });
            let fwd = ctx.and_then(|c| c.is_forwarded).unwrap_or(false);
            let fs = ctx.and_then(|c| c.forwarding_score).unwrap_or(0);
            (qid, qs, qt, fwd, fs)
        } else {
            (None, None, None, false, 0)
        };

    Some(IncomingMessage {
        id: msg_id,
        chat_id: chat_id.to_string(),
        sender_id,
        sender_name,
        text,
        media_type,
        timestamp,
        is_from_me: from_me,
        quoted_msg_id: quoted_msg_id,
        quoted_text: quoted_text_hist,
        quoted_sender: quoted_sender,
        is_forwarded: is_forwarded_hist,
        forwarding_score: forwarding_score_hist,
        reactions: web_msg
            .reactions
            .iter()
            .filter_map(|r| {
                let sender = r
                    .key
                    .as_ref()?
                    .participant
                    .clone()
                    .or_else(|| r.key.as_ref()?.remote_jid.clone())
                    .unwrap_or_default();
                let emoji = r.text.clone()?;
                if emoji.is_empty() {
                    return None;
                }
                Some((sender, emoji))
            })
            .collect(),
        poll_question: None,
        poll_options: vec![],
        poll_selectable: 0,
        poll_secret: vec![],
        poll_votes: vec![],
        media_local_path: None,
        media_filename,
        media_caption,
        contact_name,
        contact_vcard,
        link_title,
        link_description,
        link_url,
        link_thumbnail_path: None,
        receipt_status: if from_me {
            ReceiptStatus::Sent
        } else {
            ReceiptStatus::Pending
        },
        quoted_media_path: None,
        is_edited: false,
        is_system_message: false,
    })
}

fn extract_media_type(base: &wa::Message) -> Option<crate::bridge::MediaType> {
    if base.image_message.is_some() {
        return Some(crate::bridge::MediaType::Image);
    }
    if base
        .video_message
        .as_deref()
        .map(|v| v.gif_playback.unwrap_or(false))
        .unwrap_or(false)
    {
        return Some(crate::bridge::MediaType::Gif);
    }
    if base.video_message.is_some() {
        return Some(crate::bridge::MediaType::Video);
    }
    if base.audio_message.is_some() {
        return Some(crate::bridge::MediaType::Audio);
    }
    if base.document_message.is_some() {
        return Some(crate::bridge::MediaType::Document);
    }
    if base.sticker_message.is_some() {
        return Some(crate::bridge::MediaType::Sticker);
    }
    None
}

fn map_message(msg: wa::Message, info: MessageInfo) -> Option<IncomingMessage> {
    let text = msg.text_content().map(|s| s.to_string());
    let base = msg.get_base_message();

    let media_type = extract_media_type(base);
    let media_filename = base
        .document_message
        .as_deref()
        .and_then(|d| d.file_name.clone());
    let media_caption = base
        .image_message
        .as_deref()
        .and_then(|i| i.caption.clone())
        .or_else(|| {
            base.video_message
                .as_deref()
                .and_then(|v| v.caption.clone())
        })
        .or_else(|| {
            base.document_message
                .as_deref()
                .and_then(|d| d.caption.clone())
        });

    let ctx = base
        .extended_text_message
        .as_deref()
        .and_then(|m| m.context_info.as_deref())
        .or_else(|| {
            base.image_message
                .as_deref()
                .and_then(|m| m.context_info.as_deref())
        })
        .or_else(|| {
            base.video_message
                .as_deref()
                .and_then(|m| m.context_info.as_deref())
        })
        .or_else(|| {
            base.audio_message
                .as_deref()
                .and_then(|m| m.context_info.as_deref())
        })
        .or_else(|| {
            base.document_message
                .as_deref()
                .and_then(|m| m.context_info.as_deref())
        });

    let quoted_msg_id = ctx.and_then(|c| c.stanza_id.clone());
    let quoted_sender = ctx.and_then(|c| c.participant.clone());
    let quoted_text = ctx.and_then(|c| {
        let qm = c.quoted_message.as_deref()?;
        // Try text first
        if let Some(t) = qm.text_content() {
            return Some(t.to_string());
        }
        // Try media captions
        let base = qm.get_base_message();
        if let Some(img) = &base.image_message {
            return Some(
                img.caption
                    .clone()
                    .unwrap_or_else(|| "📷 Photo".to_string()),
            );
        }
        if let Some(vid) = &base.video_message {
            return Some(
                vid.caption
                    .clone()
                    .unwrap_or_else(|| "🎥 Video".to_string()),
            );
        }
        if let Some(_) = &base.audio_message {
            return Some("🎵 Audio".to_string());
        }
        if let Some(doc) = &base.document_message {
            return Some(format!(
                "📄 {}",
                doc.file_name.as_deref().unwrap_or("Document")
            ));
        }
        if let Some(_) = &base.sticker_message {
            return Some("🎭 Sticker".to_string());
        }
        if let Some(_) = &base.contact_message {
            return Some("📇 Contact".to_string());
        }
        None
    });
    let is_forwarded = ctx.and_then(|c| c.is_forwarded).unwrap_or(false);
    let forwarding_score = ctx.and_then(|c| c.forwarding_score).unwrap_or(0);

    // Extract contact card
    let (contact_name, contact_vcard) = if let Some(cm) = &base.contact_message {
        (cm.display_name.clone(), cm.vcard.clone())
    } else {
        (None, None)
    };

    // Extract link preview from ExtendedTextMessage
    let (link_title, link_description, link_url) = if let Some(etm) = &base.extended_text_message {
        (
            etm.title.clone(),
            etm.description.clone(),
            etm.matched_text.clone(),
        )
    } else {
        (None, None, None)
    };

    // Extract poll data
    let poll = base
        .poll_creation_message
        .as_deref()
        .or(base.poll_creation_message_v2.as_deref())
        .or(base.poll_creation_message_v3.as_deref());
    let (poll_question, poll_options, poll_selectable, poll_secret) = if let Some(p) = poll {
        let q = p.name.clone().unwrap_or_default();
        let opts: Vec<String> = p
            .options
            .iter()
            .filter_map(|o| o.option_name.clone())
            .collect();
        let sel = p.selectable_options_count.unwrap_or(1);
        // The secret is in the OUTER message's messageContextInfo.messageSecret
        let has_outer = msg
            .message_context_info
            .as_ref()
            .and_then(|mci| mci.message_secret.as_ref())
            .is_some();
        let has_base = base
            .message_context_info
            .as_ref()
            .and_then(|mci| mci.message_secret.as_ref())
            .is_some();
        let has_enc = p.enc_key.is_some();
        let secret = msg
            .message_context_info
            .as_ref()
            .and_then(|mci| mci.message_secret.clone())
            .or_else(|| {
                base.message_context_info
                    .as_ref()
                    .and_then(|mci| mci.message_secret.clone())
            })
            .or_else(|| p.enc_key.clone())
            .unwrap_or_default();
        log::info!(
            "Poll mapped: q={:?} opts={} secret_len={} sources: outer={has_outer} base={has_base} enc_key={has_enc}",
            p.name,
            p.options.len(),
            secret.len()
        );
        (Some(q), opts, sel, secret)
    } else {
        (None, vec![], 0, vec![])
    };

    // Skip protocol messages with no displayable content
    if text.is_none()
        && media_type.is_none()
        && media_caption.is_none()
        && contact_name.is_none()
        && poll_question.is_none()
    {
        return None;
    }

    // For contact messages, set text to display name if no text
    let text = text
        .or_else(|| contact_name.as_ref().map(|n| format!("📇 {n}")))
        .or_else(|| poll_question.as_ref().map(|q| format!("📊 {q}")));

    Some(IncomingMessage {
        id: info.id.to_string(),
        chat_id: info.source.chat.to_string(),
        sender_id: info.source.sender.to_string(),
        sender_name: info.push_name.clone(),
        text,
        media_type,
        timestamp: info.timestamp.timestamp(),
        is_from_me: info.source.is_from_me,
        quoted_msg_id,
        quoted_text,
        quoted_sender,
        is_forwarded,
        forwarding_score,
        reactions: vec![],
        media_local_path: None,
        media_filename,
        media_caption,
        contact_name: None,
        contact_vcard: None,
        link_title: None,
        link_description: None,
        link_url: None,
        link_thumbnail_path: None,
        receipt_status: if info.source.is_from_me {
            ReceiptStatus::Sent
        } else {
            ReceiptStatus::Pending
        },
        quoted_media_path: None,
        poll_question,
        poll_options,
        poll_selectable,
        poll_secret,
        poll_votes: vec![],
        is_edited: false,
        is_system_message: false,
    })
}

fn make_outgoing_message(
    msg_id: String,
    chat_id: &str,
    text: String,
    when: std::time::SystemTime,
) -> IncomingMessage {
    let timestamp = when
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    IncomingMessage {
        id: msg_id,
        chat_id: chat_id.to_string(),
        sender_id: String::new(),
        sender_name: String::new(),
        text: Some(text),
        media_type: None,
        timestamp,
        is_from_me: true,
        quoted_msg_id: None,
        quoted_text: None,
        poll_question: None,
        poll_options: vec![],
        poll_selectable: 0,
        poll_secret: vec![],
        poll_votes: vec![],
        quoted_sender: None,
        is_forwarded: false,
        forwarding_score: 0,
        reactions: vec![],
        media_local_path: None,
        media_filename: None,
        media_caption: None,
        contact_name: None,
        contact_vcard: None,
        link_title: None,
        link_description: None,
        link_url: None,
        link_thumbnail_path: None,
        quoted_media_path: None,
        receipt_status: ReceiptStatus::Sent,
        is_edited: false,
        is_system_message: false,
    }
}

fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", nanos)
}
