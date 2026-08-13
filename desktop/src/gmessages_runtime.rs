//! Google Messages integration shim.
//!
//! Wraps [`gmessages_rust::Client`] and translates its events into the
//! desktop's existing [`WaEvent`] / [`WaCommand`] shapes so the rest of the
//! UI doesn't need to know whether a chat is WhatsApp or SMS/RCS.
//!
//! ## Status
//!
//! Phase 1 — runs alongside the WhatsApp client without touching any UI
//! code. Pumps gmessages events through the same channel using
//! `chat_id = "gm:" + conversation_id` so they don't collide with WhatsApp
//! JIDs (`@s.whatsapp.net`, `@g.us`).
//!
//! The merger logic for "one chat per phone number across both protocols"
//! is intentionally deferred to Phase 2 — first we want to confirm both
//! clients can coexist in the same Tokio runtime and that the event
//! translation is correct.
//!
//! ## Activation
//!
//! Set the `GMESSAGES_ENABLE=1` environment variable. Without it this
//! module is a no-op; the existing WhatsApp behavior is unchanged.
//!
//! ## State
//!
//! `gmessages-auth.json` is stored next to `whatsapp.db` in the desktop's
//! data directory. If absent, [`spawn`] runs a QR pairing flow on stdout
//! (terminal-only for now — Phase 2 will hook the QR into the GTK UI).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_channel::Sender;
use gmessages_rust::gmproto::conversations::{MediaContent, Message as GmMessage, message_info};
use gmessages_rust::{AuthData, Client, Event};
use tokio::sync::mpsc::UnboundedReceiver as TokioUnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender as TokioUnboundedSender;

use crate::bridge::WaCommand;

use crate::bridge::{
    ChatSummary, IncomingMessage, MediaType, MessageSource, ReceiptStatus,
    VERIFICATION_CODES_CHAT_ID, WaEvent, detect_two_factor_code,
};

const AUTH_FILE: &str = "gmessages-auth.json";
const CHAT_PREFIX: &str = "gm:";

/// Spawn the gmessages runtime alongside the existing WhatsApp runtime.
///
/// Returns `Some(cmd_tx)` if the runtime started, `None` otherwise.
/// Caller should forward any `WaCommand` whose `chat_id` starts with `gm:`
/// (see [`is_gm_chat`]) to the returned sender. The WhatsApp runtime should
/// drop those commands so they don't try to parse `gm:...` as a JID.
pub fn spawn(
    data_dir: &Path,
    event_tx: Sender<WaEvent>,
    wa_cmd_tx: TokioUnboundedSender<WaCommand>,
) -> Option<TokioUnboundedSender<WaCommand>> {
    if std::env::var("GMESSAGES_ENABLE").as_deref() != Ok("1") {
        log::info!(
            "gmessages: GMESSAGES_ENABLE not set; skipping (set GMESSAGES_ENABLE=1 to enable)"
        );
        return None;
    }
    let data_dir = data_dir.to_path_buf();
    log::info!(
        "gmessages: ENABLED — spawning runtime; data_dir={}",
        data_dir.display()
    );
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run(data_dir, event_tx.clone(), cmd_rx, wa_cmd_tx).await {
            log::error!("gmessages runtime error: {e:#}");
            let _ = event_tx
                .send(WaEvent::ErrorToast(format!("gmessages: {e}")))
                .await;
        }
    });
    Some(cmd_tx)
}

/// True if the chat ID belongs to a Google Messages chat (`gm:<conversation_id>`).
pub fn is_gm_chat(chat_id: &str) -> bool {
    chat_id.starts_with(CHAT_PREFIX)
}

/// Strip the `gm:` prefix to recover the underlying conversation_id.
pub fn strip_prefix(chat_id: &str) -> &str {
    chat_id.strip_prefix(CHAT_PREFIX).unwrap_or(chat_id)
}

/// Data dir for the gmessages runtime, captured once at `run()` start so
/// free functions (e.g. `message_to_incoming`) can resolve `gm_media/`
/// paths without threading it through every call site.
static GM_DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Deterministic on-disk path for a downloaded gm media blob. Mirrors the
/// naming `pending_downloads` uses so a path computed here finds the file
/// the downloader actually wrote.
fn gm_media_dest(media_id: &str, mime: &str, data_dir: &Path) -> PathBuf {
    let ext = match mime.split_once('/').map(|(_, sub)| sub) {
        Some(s) if !s.is_empty() => s,
        _ => "bin",
    };
    // Google media ids contain '/' (e.g. "<uuid>/<blob>"), which would turn
    // the filename into a nested path whose parent dir doesn't exist — the
    // write then fails with ENOENT and the media silently never saves.
    // Flatten any path-unsafe character to '_'.
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let safe_id = sanitize(media_id);
    let safe_ext = sanitize(ext);
    data_dir
        .join("gm_media")
        .join(format!("{safe_id}.{safe_ext}"))
}

/// GTK/gdk-pixbuf has no HEIC/HEIF loader, so iPhone MMS/RCS photos download fine
/// but can't be rendered — they show as a "📷 Photo" placeholder. Transcode a
/// saved HEIC to a sibling JPEG via `heif-convert` and return that path. No-op
/// (returns the original) for non-HEIC files or if the conversion fails; the
/// produced .jpg is reused on subsequent calls. Blocking — run off the async
/// runtime (decoding a multi-MB HEIC is heavy CPU).
fn convert_heic_to_jpg(dest: &Path) -> PathBuf {
    let is_heic = dest
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("heic") || e.eq_ignore_ascii_case("heif"))
        .unwrap_or(false);
    if !is_heic {
        return dest.to_path_buf();
    }
    let jpg = dest.with_extension("jpg");
    if jpg.exists() {
        return jpg;
    }
    match std::process::Command::new("heif-convert")
        .arg(dest)
        .arg(&jpg)
        .output()
    {
        Ok(o) if o.status.success() && jpg.exists() => {
            log::info!("gmessages: transcoded HEIC → {}", jpg.display());
            jpg
        }
        result => {
            log::warn!(
                "gmessages: HEIC→JPEG failed for {} ({result:?}); leaving original (won't render)",
                dest.display()
            );
            dest.to_path_buf()
        }
    }
}

/// Best-effort MIME type from a file path's extension. Used when sending
/// an outbound MMS — the relay needs a content type for the attachment.
fn guess_mime(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "3gp" | "3gpp" => "video/3gpp",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" => "audio/ogg",
        "amr" => "audio/amr",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Incrementally upsert one gm chat's entry in `gm_chats.bin`.
///
/// `gm_chats.bin` used to be written ONLY by `list_conversations`, which
/// runs once per startup. Any SMS chat created or updated mid-session
/// therefore never reached disk and vanished from the chat list on the
/// next restart. This keeps the cache current message-by-message:
/// load → upsert the one entry → write back. The file is small (tens of
/// KB) and SMS volume is low, so a full rewrite per message is fine.
/// Load the gm chat-list cache, tolerating corruption / schema drift. A present
/// but undecodable file is preserved as `.corrupt` (never silently wiped) and
/// treated as empty so the server reseed can rebuild it.
fn gm_load_chats_cache(path: &Path) -> Vec<ChatSummary> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    if bytes.is_empty() {
        return Vec::new();
    }
    if let Ok(v) = bincode::deserialize::<Vec<ChatSummary>>(&bytes) {
        return v;
    }
    crate::ui::runtime::backup_corrupt_once(path);
    log::warn!(
        "gm_load_chats_cache: {} is undecodable (schema drift?); preserved as .corrupt, treating as empty",
        path.display()
    );
    Vec::new()
}

/// Save the gm chat-list cache atomically, and NEVER overwrite a non-empty
/// on-disk cache with an empty vector — that turned a one-restart schema blip
/// (a `ChatSummary` field added without a fallback) into permanent loss of every
/// SMS-only chat.
fn gm_save_chats_cache(path: &Path, chats: &[ChatSummary]) {
    if chats.is_empty() {
        if let Ok(bytes) = std::fs::read(path) {
            if !bytes.is_empty() {
                log::warn!(
                    "gm_save_chats_cache: refusing to overwrite non-empty {} with an empty cache",
                    path.display()
                );
                return;
            }
        }
    }
    match bincode::serialize(&chats.to_vec()) {
        Ok(payload) => {
            if let Err(e) = crate::ui::runtime::atomic_write(path, &payload) {
                log::warn!("gm_save_chats_cache({}): {e}", path.display());
            }
        }
        Err(e) => log::warn!("gm_save_chats_cache serialize failed: {e}"),
    }
}

/// Derive a sidebar preview string from an [`IncomingMessage`]: its text /
/// caption, else a media-type placeholder. Shared by the STEP 3/4 handlers and
/// the send-echo paths so their previews stay identical.
fn gm_preview_text(im: &IncomingMessage) -> String {
    im.text
        .clone()
        .or_else(|| im.media_caption.clone())
        .unwrap_or_else(|| match im.media_type {
            Some(MediaType::Image) => "📷 Photo".into(),
            Some(MediaType::Video) => "🎥 Video".into(),
            Some(MediaType::Audio) => "🎵 Audio".into(),
            Some(MediaType::Document) => "📄 Document".into(),
            Some(MediaType::Sticker) => "🎭 Sticker".into(),
            Some(MediaType::Gif) => "🎞 GIF".into(),
            None => String::new(),
        })
}

/// Incrementally upsert one gm chat's entry in `gm_chats.bin` and return the
/// POST-upsert summary (so the caller can emit an authoritative
/// `ChatRowChanged`). `None` only if the chat is neither present nor creatable
/// (never happens — we always create).
///
/// `is_active` = the conversation is the one the user is currently viewing, and
/// `read_wm` = the gm read watermark for this conv. Together they gate the live
/// unread increment (G2): an incoming background message past the watermark
/// bumps the badge (previously this NEVER incremented for an existing chat — D2).
fn upsert_gm_chat_cache(
    path: &Path,
    chat_id: &str,
    preview: &str,
    timestamp: i64,
    is_from_me: bool,
    sender_name: &str,
    is_active: bool,
    read_wm: i64,
) -> Option<ChatSummary> {
    let mut cache: Vec<ChatSummary> = gm_load_chats_cache(path);

    let result;
    if let Some(existing) = cache.iter_mut().find(|c| c.id == chat_id) {
        // Move forward only — an out-of-order older message in a batch
        // must not rewrite a newer preview / timestamp. Never overwrite a
        // non-empty preview with an empty one (an outgoing RCS with empty
        // display_content used to blank the row).
        if timestamp >= existing.timestamp {
            if !preview.is_empty() {
                existing.last_message = preview.to_string();
            }
            existing.timestamp = timestamp;
        }
        if is_from_me {
            existing.unread_count = 0;
        } else if !is_active && timestamp > read_wm {
            // Live unread increment for a background gm chat (G2/D2 fix).
            existing.unread_count = existing.unread_count.saturating_add(1);
        }
        // Never overwrite an existing (likely better-resolved) name.
        result = existing.clone();
    } else {
        // Brand-new gm chat. Best-effort name: an incoming message's
        // resolved sender name if it looks real, else the conversation
        // id as a placeholder — the next `list_conversations` overlays
        // the authoritative name.
        let name = if !is_from_me
            && !sender_name.is_empty()
            && sender_name.chars().any(|c| c.is_alphabetic())
        {
            sender_name.to_string()
        } else {
            strip_prefix(chat_id).to_string()
        };
        let summary = ChatSummary {
            id: chat_id.to_string(),
            name,
            last_message: preview.to_string(),
            timestamp,
            // A brand-new incoming chat is unread unless it's actively viewed.
            unread_count: if is_from_me || is_active { 0 } else { 1 },
            is_group: false,
            is_muted: false,
            is_pinned: false,
            is_archived: false,
            is_favorite: false,
            label: None,
            pinned_msg_id: None,
            auto_mark_read: false,
        };
        cache.push(summary.clone());
        result = summary;
    }

    gm_save_chats_cache(path, &cache);
    Some(result)
}

/// A conversation-update payload is authoritative for identity/flags, while
/// the message stream is authoritative for preview/time/unread. Merge only the
/// former into an existing row so an update racing a newer message cannot move
/// the row backwards or clear its badge.
fn merge_gm_conversation_metadata(
    cache: &mut Vec<ChatSummary>,
    fresh: &ChatSummary,
) -> ChatSummary {
    if let Some(existing) = cache.iter_mut().find(|c| c.id == fresh.id) {
        // Never replace a real contact name with a numeric conversation id or
        // phone placeholder. Conversely, a full conversation update is the
        // best source for replacing such a placeholder with a saved name.
        let fresh_quality = chat_name_quality(&fresh.name);
        let existing_quality = chat_name_quality(&existing.name);
        if fresh_quality > 0 && fresh_quality >= existing_quality {
            existing.name = fresh.name.clone();
        }
        existing.is_group = fresh.is_group;
        existing.is_pinned = fresh.is_pinned;
        existing.is_archived = fresh.is_archived;
        return existing.clone();
    }

    cache.push(fresh.clone());
    fresh.clone()
}

fn upsert_gm_conversation_metadata_cache(
    path: &Path,
    fresh: &ChatSummary,
    conversation: &gmessages_rust::gmproto::conversations::Conversation,
) -> ChatSummary {
    let mut cache = gm_load_chats_cache(path);
    if let Some(existing) = cache.iter_mut().find(|chat| chat.id == fresh.id)
        && conversation_name_is_self(conversation, &existing.name)
    {
        // Repair names poisoned by older builds before applying quality rules;
        // a self name must never outrank the other party's phone fallback.
        existing.name = fresh.name.clone();
    }
    let result = merge_gm_conversation_metadata(&mut cache, fresh);
    gm_save_chats_cache(path, &cache);
    result
}

fn conversation_name_is_self(
    conversation: &gmessages_rust::gmproto::conversations::Conversation,
    candidate: &str,
) -> bool {
    let candidate = candidate.trim();
    !candidate.is_empty()
        && conversation
            .participants
            .iter()
            .filter(|participant| participant.is_me)
            .flat_map(|participant| [&participant.full_name, &participant.first_name])
            .map(|value| value.trim())
            .any(|value| !value.is_empty() && value.eq_ignore_ascii_case(candidate))
}

/// Name quality used throughout the SMS identity pipeline:
///   0 = empty/internal conversation or participant id (`25`, `gm:6689`)
///   1 = a plausible full phone number (correct fallback for unsaved contacts)
///   2 = a human/contact/group name
fn chat_name_quality(name: &str) -> u8 {
    let name = name.trim();
    if name.is_empty()
        || name.starts_with(CHAT_PREFIX)
        || name.ends_with("@lid")
        || name.ends_with("@s.whatsapp.net")
        || name.ends_with("@g.us")
    {
        return 0;
    }
    let digits = name.chars().filter(|c| c.is_ascii_digit()).count();
    let phone_chars_only = name
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '(' | ')' | '-' | '.' | '\u{a0}'));
    if phone_chars_only {
        return if digits >= 7 { 1 } else { 0 };
    }
    2
}

fn chat_name_is_named(name: &str) -> bool {
    chat_name_quality(name) == 2
}

fn chat_name_is_usable(name: &str) -> bool {
    chat_name_quality(name) > 0
}

/// Short SMS sender codes are valid identities despite being fewer than seven
/// digits. They are only safe in conversation context: a value equal to the
/// conversation's own numeric id is an internal placeholder (for example the
/// old `gm:25` / `25` bug), while a different value such as `87225` is the
/// actual SMS sender.
fn conversation_shortcode<'a>(conversation_id: &str, name: &'a str) -> Option<&'a str> {
    let name = name.trim();
    let digits = name.chars().filter(|c| c.is_ascii_digit()).count();
    (name != conversation_id
        && (3..=6).contains(&digits)
        && name.chars().all(|c| c.is_ascii_digit()))
    .then_some(name)
}

fn participant_phone(
    participant: &gmessages_rust::gmproto::conversations::Participant,
) -> Option<String> {
    let id = participant.id.as_ref();
    [
        id.map(|v| v.number.as_str()),
        Some(participant.formatted_number.as_str()),
        id.map(|v| v.participant_id.as_str()),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| chat_name_quality(value) == 1)
    .map(str::to_string)
}

fn participant_display_name(
    participant: &gmessages_rust::gmproto::conversations::Participant,
    contacts: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if participant.is_me {
        return None;
    }
    [&participant.full_name, &participant.first_name]
        .into_iter()
        .map(|value| value.trim())
        .find(|value| chat_name_is_named(value))
        .map(str::to_string)
        .or_else(|| {
            participant_phone(participant).and_then(|phone| {
                lookup_contact_name(contacts, &phone)
                    .filter(|name| chat_name_is_named(name))
                    .or(Some(phone))
            })
        })
}

fn update_gm_chat_name_cache(path: &Path, chat_id: &str, name: &str) -> Option<ChatSummary> {
    if !chat_name_is_usable(name) {
        return None;
    }
    let mut cache = gm_load_chats_cache(path);
    let existing = cache.iter_mut().find(|chat| chat.id == chat_id)?;
    if chat_name_quality(name) >= chat_name_quality(&existing.name) {
        existing.name = name.to_string();
    }
    let result = existing.clone();
    gm_save_chats_cache(path, &cache);
    Some(result)
}

/// Build phone-digits → wa_chat_id (JID) index from the persisted WhatsApp
/// chat list. Used to detect gm chats that should merge into existing
/// WhatsApp chats for the same person.
///
/// Only individual chats (`@s.whatsapp.net` / `@lid`) are indexed — we
/// don't merge SMS into WhatsApp groups.
fn build_phone_to_wa_index() -> std::collections::HashMap<String, String> {
    let mut idx: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for chat in crate::ui::runtime::load_chats() {
        if chat.id.ends_with("@g.us") {
            continue;
        }
        let digits = crate::contacts::digits_only(&chat.id);
        if digits.is_empty() {
            continue;
        }
        // Prefer phone JIDs over LID JIDs when both are present.
        let prefer_this = chat.id.ends_with("@s.whatsapp.net");
        match idx.get(&digits) {
            Some(existing) if existing.ends_with("@s.whatsapp.net") && !prefer_this => {}
            _ => {
                idx.insert(digits, chat.id.clone());
            }
        }
    }
    idx
}

/// Rewrite a `WaEvent`'s chat_id in-place using the merge map. Used to
/// redirect gm chat traffic onto the matching WhatsApp chat row.
fn redirect_chat_id(event: &mut WaEvent, merge_map: &std::collections::HashMap<String, String>) {
    let map_id = |id: &mut String| {
        if let Some(conv_id) = id.strip_prefix(CHAT_PREFIX)
            && let Some(target) = merge_map.get(conv_id)
        {
            *id = target.clone();
        }
    };
    match event {
        WaEvent::MessageReceived(im) => map_id(&mut im.chat_id),
        WaEvent::TypingIndicator { chat_id, .. }
        | WaEvent::ChatNameUpdated { chat_id, .. }
        | WaEvent::ChatReadOnOtherDevice { chat_id, .. }
        | WaEvent::HistoryMessages { chat_id, .. } => map_id(chat_id),
        WaEvent::MessageConfirmed { chat_id, .. } | WaEvent::MessageFailed { chat_id, .. } => {
            map_id(chat_id)
        }
        WaEvent::ChatAdded(s) => map_id(&mut s.id),
        _ => {}
    }
}

/// Resolve a gm chat to its unified chat_id. If the gm chat's participant
/// has a phone-digit match in the WhatsApp index, returns the WhatsApp JID.
/// Otherwise returns the original `gm:` ID.
fn unified_chat_id(
    gm_chat_id: &str,
    phone: Option<&str>,
    phone_to_wa_chat: &std::collections::HashMap<String, String>,
) -> String {
    if let Some(p) = phone {
        let digits = crate::contacts::digits_only(p);
        if !digits.is_empty()
            && let Some(jid) = phone_to_wa_chat.get(&digits)
        {
            return jid.clone();
        }
        // Try last-10-digit fuzzy match.
        if digits.len() >= 10 {
            let s10 = &digits[digits.len() - 10..];
            for (k, v) in phone_to_wa_chat {
                if k.ends_with(s10) {
                    return v.clone();
                }
            }
        }
    }
    gm_chat_id.to_string()
}

/// Shared, mutable cache of phone→name mappings. Populated at startup via
/// ListContacts/ListTopContacts and enriched at runtime from incoming
/// `sender_participant` payloads, so a chat's true name surfaces as soon
/// as we see a message from someone we know.
type ContactCache = std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>;

/// Bounded LRU set of recently-emitted gmessages message IDs. The server
/// retransmits batches with overlapping content (especially right after
/// long-poll reconnect), so we keep a moving window of message IDs we've
/// already forwarded to the UI to avoid re-rendering the same bubble.
const RECENT_MSG_RING_SIZE: usize = 1024;

#[derive(Default)]
struct RecentMsgRing {
    set: std::collections::HashSet<String>,
    queue: std::collections::VecDeque<String>,
}

impl RecentMsgRing {
    /// Returns true if this is a duplicate (already seen recently).
    fn check_and_record(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return true;
        }
        if self.queue.len() >= RECENT_MSG_RING_SIZE
            && let Some(old) = self.queue.pop_front()
        {
            self.set.remove(&old);
        }
        self.queue.push_back(id.to_string());
        self.set.insert(id.to_string());
        false
    }
}

/// Per-conversation read watermark for SMS/gm chats (raw conversation_id →
/// timestamp of the latest message the user had seen when they read the chat).
/// SMS read state has no other durable local representation — it is otherwise
/// reconstructed from Google's `unread` flag on every connect — so without this
/// a chat the user read keeps reverting to unread on every restart. Consulted
/// when hydrating the cache and when reseeding from `list_conversations`.
fn load_gm_watermarks(path: &std::path::Path) -> std::collections::HashMap<String, i64> {
    std::fs::read(path)
        .ok()
        .and_then(|b| bincode::deserialize::<std::collections::HashMap<String, i64>>(&b).ok())
        .unwrap_or_default()
}

fn save_gm_watermarks(path: &std::path::Path, map: &std::collections::HashMap<String, i64>) {
    if let Ok(bytes) = bincode::serialize(map) {
        // Atomic tmp+rename (unique tmp name) so a crash mid-write can't leave a
        // truncated watermark file that would decode empty and revert SMS chats
        // to unread on the next restart.
        if let Err(e) = crate::ui::runtime::atomic_write(path, &bytes) {
            log::warn!("save_gm_watermarks({}): {e}", path.display());
        }
    }
}

/// Set of gm conversation_ids whose live SMS have been detected as 2FA /
/// verification codes and rerouted into the synthetic "Verification Codes"
/// inbox. Persisted so that on the next restart the reseed can SUPPRESS these
/// standalone shortcode rows (which otherwise reappear with Google's `unread`
/// flag every launch), and so MarkRead on the synthetic inbox knows which real
/// convs to watermark + ACK. Without this, the "Verification Codes" chat could
/// never be marked read and its shortcode rows kept re-flagging unread — a
/// residual of the "SMS unread reappearing" bug.
fn load_gm_verification_convs(path: &std::path::Path) -> std::collections::HashSet<String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| bincode::deserialize::<std::collections::HashSet<String>>(&b).ok())
        .unwrap_or_default()
}

fn save_gm_verification_convs(path: &std::path::Path, set: &std::collections::HashSet<String>) {
    if let Ok(bytes) = bincode::serialize(set) {
        if let Err(e) = crate::ui::runtime::atomic_write(path, &bytes) {
            log::warn!("save_gm_verification_convs({}): {e}", path.display());
        }
    }
}

/// Clamp a summary's unread badge to 0 if our read watermark already covers its
/// latest activity (no new message since the user last read it). A genuinely
/// newer message (timestamp past the watermark) is left untouched, so nothing
/// is hidden.
fn apply_gm_read_watermark(
    summary: &mut ChatSummary,
    watermarks: &std::collections::HashMap<String, i64>,
) {
    if summary.unread_count == 0 {
        return;
    }
    let conv_id = strip_prefix(&summary.id);
    if let Some(&wm) = watermarks.get(conv_id) {
        if wm >= summary.timestamp {
            summary.unread_count = 0;
        }
    }
}

async fn run(
    data_dir: PathBuf,
    event_tx: Sender<WaEvent>,
    mut cmd_rx: TokioUnboundedReceiver<WaCommand>,
    wa_cmd_tx: TokioUnboundedSender<WaCommand>,
) -> Result<()> {
    // Capture the data dir so free functions can resolve gm_media/ paths.
    let _ = GM_DATA_DIR.set(data_dir.clone());
    let auth_path = resolve_auth_path(&data_dir);
    log::info!(
        "gmessages: looking for auth file at {}",
        auth_path.display()
    );
    let auth = load_auth(&auth_path).await?;
    if !auth.is_paired() {
        log::warn!("gmessages: auth file empty or missing — will run pairing flow now");
    } else {
        log::info!("gmessages: loaded auth (paired)");
        // Prime the live-cookie cache so the first connect's HTTP call
        // already has fresh cookies. Every subsequent request inside
        // apply_cookie_auth re-reads from this cache (30s TTL) — the
        // cookies stored on AuthData are no longer the source of truth.
        if auth.gaia_authuser.is_some() {
            let n = gmessages_rust::cookies::get_cached_firefox_cookies().len();
            log::info!("gmessages: primed live-cookie cache ({n} entries)");

            // No Firefox spin-up on startup. The first request the long
            // poll makes after connect() returns a Set-Cookie response
            // from Google that we capture in http.rs, self-rotating the
            // session without needing to nudge FF. As long as
            // `__Secure-1PSID` (the long-lived session id) is still
            // valid, Google will issue a fresh `__Secure-1PSIDTS` on
            // that response. If 1PSID itself is dead (real logout /
            // revocation) no amount of nudging will recover anyway —
            // that lands in the AuthRevoked recovery path.
            if let Some(age) = gmessages_rust::cookies::rotating_cookie_max_age() {
                log::info!(
                    "gmessages: freshest rotating cookie is {:.1}h old (will self-rotate on next request)",
                    age.as_secs() as f64 / 3600.0
                );
            }
        }
    }

    // Build a phone→wa_chat_id index from the WhatsApp chat list cache.
    // Used to merge gm chats into existing WhatsApp chats for the same
    // person — Phase 2 of the unified-chat-list goal. Without this, every
    // SMS contact who's also on WhatsApp shows up as TWO rows.
    //
    // We compute this once at runtime startup. Phase 3 will also re-key
    // newly-added WhatsApp chats live, but for now a stale index just
    // means "merging happens after a restart" which is acceptable.
    let phone_to_wa_chat: std::sync::Arc<std::collections::HashMap<String, String>> =
        std::sync::Arc::new(build_phone_to_wa_index());
    log::info!(
        "gmessages: built phone→wa_chat index with {} entries",
        phone_to_wa_chat.len()
    );

    // Routing table: gm conversation_id → unified chat_id (either the
    // matching wa JID, or the original gm:N if no merge target). Updated
    // as we learn participant data.
    let merge_map: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // The conversation the user is currently viewing (raw conv id, gm-prefix
    // stripped). Updated INLINE by the SetActiveChat command (see the select
    // loop) — never via the spawned handle_command, which would race rapid
    // A→B→A switches. G2's live-unread increment consults this to suppress a
    // badge bump for a message arriving in the open chat. A merged WA id is
    // reverse-mapped through merge_map to its conv id before storing.
    let active_conv: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    // Local SMS read watermarks (raw conversation_id → last-read timestamp),
    // persisted so a chat the user read stays read across restart instead of
    // being reseeded unread from Google's `unread` flag every launch.
    let gm_read_wm_path = data_dir.join("gm_read_watermarks.bin");
    let gm_read_watermarks: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, i64>>,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(load_gm_watermarks(
        &gm_read_wm_path,
    )));

    // Conversation_ids known to route into the synthetic "Verification Codes"
    // inbox (learned from live 2FA detection, persisted). Consulted at reseed to
    // suppress the standalone shortcode rows and at MarkRead to fan the read
    // watermark + server ACK to the real underlying convs.
    let gm_verif_path = data_dir.join("gm_verification_convs.bin");
    let gm_verification_convs: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashSet<String>>,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(load_gm_verification_convs(
        &gm_verif_path,
    )));

    let client = Arc::new(Client::new(auth));
    let mut events = client
        .take_event_receiver()
        .await
        .context("gmessages: event receiver already taken")?;

    // Persist refreshed auth back to disk on every change.
    let path_for_cb = auth_path.clone();
    client
        .set_auth_changed_callback(Arc::new(move |a| {
            if let Ok(json) = serde_json::to_vec_pretty(a)
                && let Err(e) = std::fs::write(&path_for_cb, &json)
            {
                log::warn!("gmessages: failed to persist auth: {e}");
            }
        }))
        .await;

    // If we don't have valid auth, run the pairing flow before connecting.
    // We render the QR to stderr so the user can scan it from the terminal.
    if !client.auth_snapshot().await.is_paired() {
        run_pair_flow(&client, &auth_path, &mut events, &event_tx).await?;
    }

    // Shared phone→name cache. Built up at startup from ListContacts +
    // ListTopContacts and enriched at runtime from message
    // `sender_participant` data so names flow into the chat list as soon
    // as we see anyone we know.
    let contact_cache: ContactCache =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Per-message dedup ring, used to suppress server retransmissions of
    // batches we've already forwarded.
    let mut recent_msgs = RecentMsgRing::default();

    // Message ids we've already sent a full-size-image request for, so a
    // thumbnail re-relayed several times only triggers one request.
    let mut requested_full_image: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Media payloads are held as complete byte vectors while decrypting and
    // writing. Bound concurrency so a large incoming batch cannot multiply
    // that memory cost without limit.
    let media_download_limit = std::sync::Arc::new(tokio::sync::Semaphore::new(4));

    // Pin the synthetic "Verification Codes" inbox at the top of the chat
    // list. All 2FA / OTP SMS get routed here instead of creating a new
    // per-shortcode chat row (TD, Aeroplan, Google, Uber, etc.). Searchable
    // in one place; the originating sender lives on each message.
    {
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let summary = ChatSummary {
            id: VERIFICATION_CODES_CHAT_ID.into(),
            name: "Verification Codes".into(),
            last_message: String::new(),
            timestamp: now_s,
            unread_count: 0,
            is_group: false,
            is_muted: false,
            is_pinned: true,
            is_archived: false,
            is_favorite: false,
            label: None,
            pinned_msg_id: None,
            auto_mark_read: true,
        };
        let _ = event_tx.send(WaEvent::ChatAdded(summary)).await;
    }

    // Hydrate the chat list from local cache BEFORE we connect, so gm chats
    // show up instantly on restart instead of after the ~3s server fetch.
    let gm_chats_cache_path = data_dir.join("gm_chats.bin");
    {
        let cached = gm_load_chats_cache(&gm_chats_cache_path);
        if !cached.is_empty() {
            log::info!("gmessages: hydrating {} chats from cache", cached.len());
            let wm_snap = gm_read_watermarks.lock().await.clone();
            for mut summary in cached {
                apply_gm_read_watermark(&mut summary, &wm_snap);
                let _ = event_tx.send(WaEvent::ChatAdded(summary)).await;
            }
        }
    }

    log::info!("gmessages: connecting…");
    if let Err(e) = client.connect().await {
        log::error!("gmessages: connect failed: {e}");
        // Stale auth — wipe it and run pairing fresh, then retry.
        if matches!(e, gmessages_rust::Error::AuthRevoked) {
            // Before tearing the whole pair down: if this is a Gaia
            // session, the most likely cause is rotated Firefox cookies
            // (1PSIDTS rotates daily). Try a SECOND connect with fresh
            // cookies before assuming we're permanently revoked.
            let is_gaia = client.auth_snapshot().await.gaia_authuser.is_some();
            let mut recovered = false;
            if is_gaia {
                if let Ok(fresh) = gmessages_rust::cookies::read_default_firefox_cookies() {
                    log::warn!(
                        "gmessages: AuthRevoked — re-reading {} FF cookies and retrying connect",
                        fresh.len()
                    );
                    client.set_cookies(fresh).await;
                    if let Err(e2) = client.connect().await {
                        log::warn!("gmessages: retry-with-fresh-cookies still failed: {e2}");
                    } else {
                        log::info!("gmessages: recovered with fresh cookies");
                        recovered = true;
                    }
                }
            }
            if !recovered {
                log::warn!("gmessages: auth revoked — wiping stale auth file and re-pairing");
                // Intentionally NOT wiping the auth file — notify_auth_changed
                // overwrites it with fresh contents on a successful re-pair,
                // and if the new pair fails we'd rather keep the old (possibly
                // recoverable) auth than be left with nothing. User asked for
                // this explicitly: "why are you letting the auth file be
                // deleted".
                // Reset in-memory auth so is_paired() returns false.
                run_pair_flow(&client, &auth_path, &mut events, &event_tx).await?;
                client
                    .connect()
                    .await
                    .context("gmessages: connect after re-pair")?;
            }
        } else {
            return Err(e.into());
        }
    }
    log::info!("gmessages: connect() returned; long-poll task running in background");

    // No more hourly background cookie-refresh task — apply_cookie_auth
    // does a live FF read (cached with a 30s TTL) on every request that
    // needs Google cookies, so the snapshot stays fresh automatically.

    // Pull contacts + conversation list so the desktop chat list has rows
    // with proper names. Order: contacts first (so we can resolve names
    // when building summaries), then conversations.
    {
        let client = client.clone();
        let event_tx = event_tx.clone();
        let gm_chats_cache_path = gm_chats_cache_path.clone();
        let contact_cache = contact_cache.clone();
        let merge_map = merge_map.clone();
        let phone_to_wa_chat = phone_to_wa_chat.clone();
        let gm_read_watermarks = gm_read_watermarks.clone();
        let gm_verification_convs = gm_verification_convs.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            // Fetch contacts (both full + top) and build a phone→name map.
            // We try both because some phones report nothing for ListContacts
            // (no permission, fresh install) but ListTopContacts covers
            // recent senders.
            // multiple normalized variants (with/without +, with/without
            // country code, digits-only) so we hit regardless of the
            // format the conversation participant ends up using.
            let mut contact_map: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut insert =
                |key: &str, name: &str, map: &mut std::collections::HashMap<String, String>| {
                    let key = key.trim();
                    if key.is_empty() {
                        return;
                    }
                    map.insert(key.to_string(), name.to_string());
                    // Digits-only variant.
                    let digits: String = key.chars().filter(|c| c.is_ascii_digit()).collect();
                    if !digits.is_empty() && digits != key {
                        map.insert(digits.clone(), name.to_string());
                    }
                    // With and without leading +.
                    if let Some(stripped) = key.strip_prefix('+') {
                        map.insert(stripped.to_string(), name.to_string());
                    } else if !key.starts_with('+') {
                        map.insert(format!("+{key}"), name.to_string());
                    }
                };
            let mut feed = |contacts: Vec<gmessages_rust::gmproto::conversations::Contact>,
                            map: &mut std::collections::HashMap<String, String>,
                            insert_fn: &mut dyn FnMut(
                &str,
                &str,
                &mut std::collections::HashMap<String, String>,
            )| {
                for c in contacts {
                    if c.name.is_empty() {
                        continue;
                    }
                    insert_fn(&c.participant_id, &c.name, map);
                    if let Some(n) = &c.number {
                        insert_fn(&n.number, &c.name, map);
                        insert_fn(&n.number2, &c.name, map);
                        if let Some(fn_) = &n.formatted_number {
                            insert_fn(fn_, &c.name, map);
                        }
                    }
                }
            };

            // Helper that feeds both the local contact_map AND the global
            // cross-protocol directory.
            let mut feed_to_global =
                |contacts: Vec<gmessages_rust::gmproto::conversations::Contact>,
                 map: &mut std::collections::HashMap<String, String>,
                 insert_fn: &mut dyn FnMut(
                    &str,
                    &str,
                    &mut std::collections::HashMap<String, String>,
                )| {
                    let global = crate::contacts::global();
                    for c in contacts {
                        if c.name.is_empty() {
                            continue;
                        }
                        insert_fn(&c.participant_id, &c.name, map);
                        global.insert(&c.participant_id, &c.name, "gmessages");
                        if let Some(n) = &c.number {
                            insert_fn(&n.number, &c.name, map);
                            insert_fn(&n.number2, &c.name, map);
                            global.insert(&n.number, &c.name, "gmessages");
                            global.insert(&n.number2, &c.name, "gmessages");
                            if let Some(fn_) = &n.formatted_number {
                                insert_fn(fn_, &c.name, map);
                                global.insert(fn_, &c.name, "gmessages");
                            }
                        }
                    }
                };
            let _ = feed; // silence "unused"; we still need it for the feed() helper variable scope

            match client.list_contacts().await {
                Ok(resp) => {
                    log::info!(
                        "gmessages: ListContacts returned {} contacts",
                        resp.contacts.len()
                    );
                    if log::log_enabled!(log::Level::Debug) {
                        for c in resp.contacts.iter().take(3) {
                            log::debug!(
                                "gmessages: sample contact: name={:?} pid={:?} num={:?}",
                                c.name,
                                c.participant_id,
                                c.number
                                    .as_ref()
                                    .map(|n| (n.number.as_str(), n.number2.as_str())),
                            );
                        }
                    }
                    feed_to_global(resp.contacts, &mut contact_map, &mut insert);
                }
                Err(e) => log::warn!("gmessages: list_contacts failed: {e}"),
            }
            match client.list_top_contacts(50).await {
                Ok(resp) => {
                    log::info!(
                        "gmessages: ListTopContacts returned {} contacts",
                        resp.contacts.len()
                    );
                    feed_to_global(resp.contacts, &mut contact_map, &mut insert);
                }
                Err(e) => log::warn!("gmessages: list_top_contacts failed: {e}"),
            }
            log::info!(
                "gmessages: contact map size = {}; global directory size = {}",
                contact_map.len(),
                crate::contacts::global().len()
            );
            crate::contacts::global().save_if_dirty();

            // Publish to the shared cache so the event pump can read it.
            *contact_cache.lock().await = contact_map.clone();

            log::info!("gmessages: fetching conversation list to seed chat rows");
            // Fetch a generous window. A single timeout must not permanently
            // strand cached rows under numeric internal IDs for this entire
            // session. Start at the phone's observed 300-conversation response
            // ceiling, retry with smaller requests, then keep retrying at a bounded
            // backoff until the phone answers.
            let mut list_attempt = 0_u32;
            let resp = loop {
                let count = match list_attempt {
                    0 => 300,
                    1 => 200,
                    2 => 100,
                    _ => 300,
                };
                match client.list_conversations(count).await {
                    Ok(resp) => break resp,
                    Err(e) => {
                        list_attempt = list_attempt.saturating_add(1);
                        let delay = (list_attempt * 3).min(30);
                        log::warn!(
                            "gmessages: list_conversations({count}) failed (attempt {list_attempt}): {e}; retrying in {delay}s"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(delay.into())).await;
                    }
                }
            };
            {
                log::info!("gmessages: got {} conversations", resp.conversations.len());
                // Clamp unread against our local read watermarks BEFORE
                // emitting or caching, so a chat the user already read isn't
                // reseeded unread from Google's stale `unread` flag. Flows to
                // both the ChatAdded events and the persisted gm_chats.bin.
                let wm_snap = gm_read_watermarks.lock().await.clone();
                // Conversations we've previously seen route into the synthetic
                // "Verification Codes" inbox. Their standalone shortcode rows
                // must NOT be reseeded here — they carry Google's `unread` flag
                // and would re-appear unread every launch (and can never be
                // marked read as a standalone row). They live inside the
                // Verification Codes inbox instead.
                let verif_snap = gm_verification_convs.lock().await.clone();
                let summaries: Vec<ChatSummary> = resp
                    .conversations
                    .iter()
                    .map(|c| {
                        let mut s = conversation_to_summary(c, &contact_map);
                        apply_gm_read_watermark(&mut s, &wm_snap);
                        s
                    })
                    .collect();
                // Persist the chat-list cache so the next startup shows
                // gm chats instantly. Persistence happens AFTER the
                // merge_map is built below, so we can exclude rows that
                // would be merged — otherwise the next startup hydrates
                // duplicates before merge data arrives.
                // Build the merge map AND register chat IDs in the
                // global contact directory. The directory becomes the
                // persistent source of truth for cross-protocol merge —
                // the merge_map is just a hot cache of it.
                {
                    let mut mm = merge_map.lock().await;
                    let global = crate::contacts::global();
                    for (summary, conv) in summaries.iter().zip(resp.conversations.iter()) {
                        let conv_id = &conv.conversation_id;
                        // A GROUP SMS thread has no single counterpart, so it must
                        // never be merged: picking its first participant would fold
                        // the whole group into that person's 1:1 WhatsApp chat.
                        let is_group = conv.is_group_chat
                            || conv
                                .participants
                                .iter()
                                .filter(|p| p.is_visible && !p.is_me)
                                .count()
                                > 1
                            || conv.other_participants.len() > 1;
                        let phone = if is_group {
                            None
                        } else {
                            conv.participants
                                .iter()
                                .find(|p| p.is_visible && !p.is_me)
                                .and_then(|p| p.id.as_ref())
                                .filter(|id| !id.number.is_empty())
                                .map(|id| id.number.clone())
                                .or_else(|| conv.other_participants.first().cloned())
                        };
                        if is_group {
                            // Heal a previously-recorded bad merge: this thread was
                            // once linked to whichever participant happened to be
                            // first, which both showed the group under that person's
                            // 1:1 chat and could route their SMS to everyone.
                            let healed = global.clear_chat_id("gmessages", &summary.id);
                            if healed > 0 {
                                log::warn!(
                                    "gmessages: cleared {healed} stale merge link(s) pointing at group conv {conv_id}"
                                );
                            }
                            mm.remove(conv_id);
                        }
                        // Record the gm chat_id in the global directory.
                        if let Some(p) = &phone {
                            global.record_chat_id(p, "gmessages", &summary.id);
                        }
                        let unified =
                            unified_chat_id(&summary.id, phone.as_deref(), &phone_to_wa_chat);
                        if unified != summary.id {
                            log::info!(
                                "gmessages: MERGE gm:{} (gm-name={:?}) → wa={} (gm phone {:?})",
                                conv_id,
                                summary.name,
                                unified,
                                phone,
                            );
                            if let Some(p) = &phone {
                                global.record_chat_id(p, "whatsapp", &unified);
                            }
                            mm.insert(conv_id.clone(), unified);
                        }
                    }
                    global.save_if_dirty();
                }
                let mm_snap = merge_map.lock().await.clone();

                // Persist the cache NOW. CRITICAL: this MERGES into
                // the previous cache instead of overwriting. The
                // server's list_conversations returns at most N
                // chats per call (top N by recency). If we
                // overwrote, every chat past the top N silently
                // dropped off the cache and never came back on
                // restart — exactly the "SMS not persisting across
                // restart" the user has been chasing.
                //
                // Merge semantics:
                //   - For chats in the FRESH response, fresh wins.
                //   - For chats in the OLD cache but absent from
                //     the fresh response, keep the old entry (still
                //     a real chat, server just didn't include it).
                //   - Always exclude entries that are now merged
                //     (in mm_snap) — those are absorbed by the WA row.
                use std::collections::HashMap as StdHashMap;
                let mut merged_cache: StdHashMap<String, ChatSummary> = StdHashMap::new();
                // Seed with the OLD cache contents (if any).
                for s in gm_load_chats_cache(&gm_chats_cache_path) {
                    let conv = strip_prefix(&s.id);
                    if !mm_snap.contains_key(conv) && !verif_snap.contains(conv) {
                        merged_cache.insert(s.id.clone(), s);
                    }
                }
                // Overlay the fresh response. Fresh activity/flags win, but
                // an unresolved internal id must never erase a previously
                // resolved contact name or phone number.
                for (s, conversation) in summaries.iter().zip(resp.conversations.iter()) {
                    let conv = strip_prefix(&s.id);
                    if mm_snap.contains_key(conv) {
                        // Merged into a WA row — drop it from the
                        // gm cache so it doesn't get hydrated again.
                        merged_cache.remove(&s.id);
                        continue;
                    }
                    if verif_snap.contains(conv) {
                        // 2FA shortcode — lives in the Verification Codes
                        // inbox; never a standalone row in the cache.
                        merged_cache.remove(&s.id);
                        continue;
                    }
                    let mut fresh = s.clone();
                    if let Some(old) = merged_cache.get(&s.id)
                        && chat_name_quality(&old.name) > chat_name_quality(&fresh.name)
                        && !conversation_name_is_self(conversation, &old.name)
                    {
                        fresh.name = old.name.clone();
                    }
                    merged_cache.insert(s.id.clone(), fresh);
                }
                let mut to_persist: Vec<ChatSummary> = merged_cache.into_values().collect();
                // Sort by timestamp desc so the file is stable + easy
                // to inspect.
                to_persist.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                log::info!(
                    "gmessages: persisting {} chats to cache (fresh: {}, merge_map: {})",
                    to_persist.len(),
                    summaries.len(),
                    mm_snap.len(),
                );
                gm_save_chats_cache(&gm_chats_cache_path, &to_persist);

                // First, evict stale gm rows that may have been hydrated
                // from gm_chats.bin cache on startup but should now be
                // merged into a WhatsApp row. Without this, the user
                // sees BOTH a gm:NN row AND the matching WA row, can
                // click the gm row, but messages get redirected by the
                // merge_map to the WA row's view — so they vanish into
                // the wrong chat. Emit ChatDeleted for each merged id.
                for conv_id in mm_snap.keys() {
                    let stale_id = format!("{CHAT_PREFIX}{conv_id}");
                    log::info!(
                        "gmessages: evicting stale gm row {stale_id} (merged into {})",
                        mm_snap.get(conv_id).map(|s| s.as_str()).unwrap_or("?")
                    );
                    let _ = event_tx
                        .send(WaEvent::ChatDeleted { chat_id: stale_id })
                        .await;
                }
                // Evict any standalone shortcode row that was hydrated from
                // cache on startup: its SMS belong in the Verification Codes
                // inbox, not a per-shortcode row that keeps re-flagging unread.
                for conv_id in &verif_snap {
                    let stale_id = format!("{CHAT_PREFIX}{conv_id}");
                    log::info!(
                        "gmessages: evicting stale 2FA shortcode row {stale_id} (→ {VERIFICATION_CODES_CHAT_ID})"
                    );
                    let _ = event_tx
                        .send(WaEvent::ChatDeleted { chat_id: stale_id })
                        .await;
                }
                // G6 monotonic overlay: re-read gm_chats.bin (kept current
                // message-by-message by G2/G4) so a stale server reseed can't
                // regress a row's preview/timestamp. The UI-side strict->
                // guard that used to absorb stale reseeds is gone, so any
                // staleness must die HERE. Keyed by conv id.
                let live_overlay: std::collections::HashMap<String, ChatSummary> =
                    gm_load_chats_cache(&gm_chats_cache_path)
                        .into_iter()
                        .map(|c| (strip_prefix(&c.id).to_string(), c))
                        .collect();
                for summary in &summaries {
                    let conv_id = strip_prefix(&summary.id);
                    if mm_snap.contains_key(conv_id) {
                        // Don't add a duplicate row — the existing
                        // WhatsApp row will absorb this conversation's
                        // messages via the merge_map redirect.
                        continue;
                    }
                    if verif_snap.contains(conv_id) {
                        // 2FA shortcode — routed into the Verification Codes
                        // inbox; don't reseed a standalone (unread) row.
                        continue;
                    }
                    // Overlay a newer live preview/timestamp if gm_chats.bin
                    // holds one for this conv (a message arrived after the
                    // server list was fetched).
                    let mut summary = summary.clone();
                    if let Some(live) = live_overlay.get(conv_id) {
                        if chat_name_quality(&live.name) > chat_name_quality(&summary.name) {
                            summary.name = live.name.clone();
                        }
                        if live.timestamp > summary.timestamp {
                            summary.timestamp = live.timestamp;
                            if !live.last_message.is_empty() {
                                summary.last_message = live.last_message.clone();
                            }
                            summary.unread_count = live.unread_count;
                        }
                    }
                    log::debug!(
                        "gmessages → desktop: ChatAdded({} \"{}\")",
                        summary.id,
                        summary.name
                    );
                    if event_tx
                        .send(WaEvent::ChatAdded(summary.clone()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    // Also push a ChatNameUpdated so any cached row (from a
                    // previous session) refreshes its title now that
                    // contacts are loaded.
                    if chat_name_is_usable(&summary.name) {
                        let _ = event_tx
                            .send(WaEvent::ChatNameUpdated {
                                chat_id: summary.id.clone(),
                                name: summary.name.clone(),
                            })
                            .await;
                    }
                }

                // EAGERLY enrich chat names: for any conversation whose
                // resolved name still looks like a phone number / numeric
                // ID, fetch one message and pull the real name out of
                // sender_participant. Done in parallel so 50 chats don't
                // serialize.
                let conversations = resp.conversations.clone();
                let unresolved: Vec<_> = summaries
                    .iter()
                    .zip(conversations.iter())
                    .filter(|(s, _)| {
                        // Name looks unresolved if it's all digits, or a
                        // formatted phone number, or starts with +.
                        !chat_name_is_named(&s.name)
                    })
                    .map(|(s, _)| s.id.clone())
                    .collect();
                log::info!(
                    "gmessages: eagerly enriching {} chats with unresolved names",
                    unresolved.len()
                );
                // The relay/phone starts timing out when a large startup list
                // fires one history RPC per unresolved row at once. Keep a
                // small bounded queue: recent conversations still resolve in
                // order, while normal event and command traffic remains
                // responsive.
                let enrichment_slots = Arc::new(tokio::sync::Semaphore::new(4));
                for chat_id in unresolved {
                    let client = client.clone();
                    let event_tx = event_tx.clone();
                    let contacts = contact_map.clone();
                    let cache_path = gm_chats_cache_path.clone();
                    let conv_id = strip_prefix(&chat_id).to_string();
                    let enrichment_slots = enrichment_slots.clone();
                    tokio::spawn(async move {
                        let Ok(_permit) = enrichment_slots.acquire_owned().await else {
                            return;
                        };
                        match client.fetch_messages(&conv_id, 5).await {
                            Ok(resp) => {
                                for m in resp.messages {
                                    if let Some(sp) = &m.sender_participant {
                                        // Skip outgoing — `is_me` is set
                                        // when this participant is the
                                        // desktop user.
                                        if gm_message_is_from_me(&m) {
                                            continue;
                                        }
                                        // A phone fallback is already present
                                        // from the conversation payload. Only
                                        // stop when history actually improves
                                        // it to a saved/human contact name.
                                        let name = participant_display_name(sp, &contacts)
                                            .filter(|name| chat_name_is_named(name));
                                        if let Some(name) = name {
                                            if let Some(summary) = update_gm_chat_name_cache(
                                                &cache_path,
                                                &chat_id,
                                                &name,
                                            ) {
                                                let _ = event_tx
                                                    .send(WaEvent::ChatRowChanged(summary))
                                                    .await;
                                            }
                                            let _ = event_tx
                                                .send(WaEvent::ChatNameUpdated { chat_id, name })
                                                .await;
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(error) => log::debug!(
                                "gmessages: startup name enrichment failed for {conv_id}: {error}"
                            ),
                        }
                    });
                }
            }
        });
    }

    // Pump events + commands. select! lets us drive both directions.
    loop {
        // Poll the cross-thread re-pair signals at the top of each loop
        // iteration. Putting them inside a `select!` sleep arm starves
        // them while events are flowing — the sleep is recreated each
        // iteration and the events branch always wins.
        if crate::gm_qr_state::take_repair_request() {
            log::warn!("gmessages: re-pair requested from settings UI");
            let _ = client.disconnect().await;
            // Intentionally NOT wiping the auth file — notify_auth_changed
            // overwrites it with fresh contents on a successful re-pair,
            // and if the new pair fails we'd rather keep the old (possibly
            // recoverable) auth than be left with nothing. User asked for
            // this explicitly: "why are you letting the auth file be
            // deleted".
            if let Err(e) = run_pair_flow(&client, &auth_path, &mut events, &event_tx).await {
                log::warn!("gmessages: re-pair failed: {e}");
            } else if let Err(e) = client.connect().await {
                log::warn!("gmessages: reconnect after re-pair failed: {e}");
            }
            continue;
        }
        if crate::gm_qr_state::take_gaia_request() {
            log::warn!("gmessages: Gaia (Firefox cookies) pairing requested from settings UI");
            crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Starting);
            let _ = client.disconnect().await;
            // Intentionally NOT wiping the auth file — notify_auth_changed
            // overwrites it with fresh contents on a successful re-pair,
            // and if the new pair fails we'd rather keep the old (possibly
            // recoverable) auth than be left with nothing. User asked for
            // this explicitly: "why are you letting the auth file be
            // deleted".
            match run_gaia_pair_flow(&client, &auth_path, &mut events, &event_tx).await {
                Ok(()) => {
                    crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Finalizing);
                    if let Err(e) = client.connect().await {
                        log::warn!("gmessages: reconnect after Gaia pair failed: {e}");
                        crate::gm_qr_state::set_gaia_status(
                            crate::gm_qr_state::GaiaStatus::Failed(format!(
                                "reconnect failed: {e}"
                            )),
                        );
                    } else {
                        crate::gm_qr_state::set_gaia_status(
                            crate::gm_qr_state::GaiaStatus::Success,
                        );
                    }
                }
                Err(e) => {
                    log::warn!("gmessages: Gaia pairing failed: {e}");
                    crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Failed(
                        format!("{e}"),
                    ));
                }
            }
            continue;
        }
        tokio::select! {
            Some(event) = events.recv() => {
                let event_kind = describe_event(&event);
                log::info!("gmessages → desktop: received {event_kind}");

                if matches!(event, Event::AuthRevoked) {
                    log::warn!("gmessages: AuthRevoked received — attempting recovery");
                    let _ = client.disconnect().await;
                    // First-line defense for Gaia sessions: Firefox may
                    // have rotated session cookies since the long-poll
                    // started. Re-read them and reconnect before tearing
                    // the pair down. Without this, every cookie rotation
                    // wipes the user's session and forces a re-pair.
                    let is_gaia = client
                        .auth_snapshot()
                        .await
                        .gaia_authuser
                        .is_some();
                    let mut recovered = false;
                    if is_gaia
                        && let Ok(fresh) =
                            gmessages_rust::cookies::read_default_firefox_cookies()
                    {
                        log::warn!(
                            "gmessages: AuthRevoked recovery — re-read {} FF cookies; retrying connect",
                            fresh.len()
                        );
                        client.set_cookies(fresh).await;
                        if let Err(e) = client.connect().await {
                            log::warn!("gmessages: retry-with-fresh-cookies still failed: {e}");
                        } else {
                            log::info!("gmessages: AuthRevoked recovery — reconnected with fresh cookies");
                            recovered = true;
                        }
                    }
                    if !recovered {
                        log::warn!(
                            "gmessages: AuthRevoked recovery failed — wiping auth and re-pairing"
                        );
                        // Intentionally NOT wiping the auth file — notify_auth_changed
            // overwrites it with fresh contents on a successful re-pair,
            // and if the new pair fails we'd rather keep the old (possibly
            // recoverable) auth than be left with nothing. User asked for
            // this explicitly: "why are you letting the auth file be
            // deleted".
                        run_pair_flow(&client, &auth_path, &mut events, &event_tx).await?;
                        client.connect().await.context("gmessages: reconnect after re-pair")?;
                    }
                    continue;
                }

                // Conversation updates carry the recipient identity even when
                // the accompanying message was sent FROM the paired phone. An
                // outgoing Message payload identifies its sender as `is_me`, so
                // message-only name resolution intentionally skips it; dropping
                // this conversation payload was why a new row stayed as "25"
                // until opening the chat triggered a separate history fetch.
                if let Event::ConversationUpdate { conversation } = &event {
                    let contacts = contact_cache.lock().await.clone();
                    let mut summary = conversation_to_summary(conversation, &contacts);
                    let conv_id = conversation.conversation_id.as_str();
                    let wm_snap = gm_read_watermarks.lock().await.clone();
                    apply_gm_read_watermark(&mut summary, &wm_snap);

                    // A merged SMS conversation is displayed on its WhatsApp
                    // row; standalone verification senders live in the shared
                    // Verification Codes inbox. Neither should create a gm row.
                    let visible_chat_id = merge_map
                        .lock()
                        .await
                        .get(conv_id)
                        .cloned()
                        .unwrap_or_else(|| summary.id.clone());
                    if gm_verification_convs.lock().await.contains(conv_id) {
                        continue;
                    }

                    if visible_chat_id == summary.id {
                        summary = upsert_gm_conversation_metadata_cache(
                            &gm_chats_cache_path,
                            &summary,
                            conversation,
                        );
                        let _ = event_tx
                            .send(WaEvent::ChatRowChanged(summary.clone()))
                            .await;
                    }

                    if chat_name_is_usable(&summary.name) {
                        log::info!(
                            "gmessages: conversation update resolved {} → {:?}",
                            visible_chat_id,
                            summary.name,
                        );
                        let _ = event_tx
                            .send(WaEvent::ChatNameUpdated {
                                chat_id: visible_chat_id,
                                name: summary.name,
                            })
                            .await;
                    }
                    continue;
                }

                // For Messages events, also: (a) opportunistically learn the
                // sender's name via `sender_participant` data and emit a
                // `ChatNameUpdated` so chat rows that started life with an
                // internal numeric ID (like "16") can be retitled the moment
                // we see a message from them, and (b) kick off media
                // downloads in parallel.
                if let Event::Messages { messages, .. } = &event {
                    let cache = contact_cache.lock().await.clone();
                    let global = crate::contacts::global();
                    let name_mm = merge_map.lock().await.clone();
                    for m in messages {
                        if let Some(sp) = &m.sender_participant {
                            // Skip OUTGOING messages: the proto's
                            // `Participant.is_me` flag is the canonical
                            // "this participant is the desktop user"
                            // signal. Status-based detection misses
                            // status=0 (Unknown) cases — using is_me is
                            // reliable for every code path.
                            if gm_message_is_from_me(m) {
                                continue;
                            }
                            let name = participant_display_name(sp, &cache);
                            // Feed the global directory so other protocols
                            // can use this name later.
                            if let Some(name) = &name
                                && chat_name_is_named(name)
                                && let Some(phone) = participant_phone(sp)
                            {
                                global.insert(&phone, name, "gmessages-msg");
                            }
                            if let Some(name) = name {
                                let raw_chat_id = format!("{CHAT_PREFIX}{}", m.conversation_id);
                                let chat_id = name_mm
                                    .get(&m.conversation_id)
                                    .cloned()
                                    .unwrap_or_else(|| raw_chat_id.clone());
                                if chat_id == raw_chat_id
                                    && let Some(summary) = update_gm_chat_name_cache(
                                        &gm_chats_cache_path,
                                        &raw_chat_id,
                                        &name,
                                    )
                                {
                                    let _ = event_tx
                                        .send(WaEvent::ChatRowChanged(summary))
                                        .await;
                                }
                                let _ = event_tx
                                    .send(WaEvent::ChatNameUpdated {
                                        chat_id,
                                        name,
                                    })
                                    .await;
                            }
                        }
                    }
                    global.save_if_dirty();
                    let dl_mm = name_mm;
                    for m in messages {
                        let downloads = pending_downloads(m, &data_dir);
                        if m.r#type != 1 {
                            log::info!(
                                "gm media diag: download pass msg={} type={} info_entries={} → {} download(s)",
                                m.message_id,
                                m.r#type,
                                m.message_info.len(),
                                downloads.len(),
                            );
                        }
                        for pm in downloads {
                            // chat_id and msg_id must match where the bubble
                            // actually lives: the merged WhatsApp jid if this
                            // conversation is merged (else the gm: chat id),
                            // and the gm:-tagged message id the bubble is
                            // keyed by. Emitting the raw ids meant downloaded
                            // media never attached to its bubble.
                            let chat_id = dl_mm
                                .get(&m.conversation_id)
                                .cloned()
                                .unwrap_or_else(|| format!("{CHAT_PREFIX}{}", m.conversation_id));
                            let msg_id = format!("{CHAT_PREFIX}{}", m.message_id);
                            // RCS images arrive thumbnail-only — ask the
                            // phone to upload the full-size version. It
                            // re-relays the message with a real media_id,
                            // which then downloads via this same path.
                            // Request once per message.
                            if pm.is_thumbnail
                                && {
                                    if requested_full_image.len() >= 4096 {
                                        requested_full_image.clear();
                                    }
                                    requested_full_image.insert(m.message_id.clone())
                                }
                            {
                                let client = client.clone();
                                let raw_msg = m.message_id.clone();
                                let ami = pm.action_message_id.clone();
                                tokio::spawn(async move {
                                    if let Err(e) =
                                        client.get_full_size_image(&raw_msg, &ami).await
                                    {
                                        log::warn!(
                                            "gmessages: get_full_size_image failed: {e}"
                                        );
                                    }
                                });
                            }
                            let media_id = pm.blob_id;
                            let key = pm.key;
                            let dest = pm.dest;
                            let kind = pm.kind;
                            let client = client.clone();
                            let event_tx = event_tx.clone();
                            let media_download_limit = media_download_limit.clone();
                            tokio::spawn(async move {
                                let Ok(_permit) = media_download_limit.acquire_owned().await else {
                                    return;
                                };
                                if dest.exists() {
                                    log::debug!(
                                        "gmessages: media already downloaded at {}",
                                        dest.display()
                                    );
                                    // HEIC can't render in GTK — transcode to JPEG off the runtime.
                                    let dest_fallback = dest.clone();
                                    let render_path = tokio::task::spawn_blocking(move || {
                                        convert_heic_to_jpg(&dest)
                                    })
                                    .await
                                    .unwrap_or(dest_fallback);
                                    let _ = event_tx
                                        .send(WaEvent::MediaReady {
                                            msg_id,
                                            chat_id,
                                            path: render_path.to_string_lossy().into_owned(),
                                            media_type: kind,
                                        })
                                        .await;
                                    return;
                                }
                                log::info!(
                                    "gmessages: downloading media {media_id} → {}",
                                    dest.display()
                                );
                                match client.download_media(&media_id, &key).await {
                                    Ok(bytes) => {
                                        // Keep the potentially large write and
                                        // HEIC conversion off Tokio workers.
                                        let render_path = match tokio::task::spawn_blocking(
                                            move || -> std::io::Result<PathBuf> {
                                                std::fs::write(&dest, &bytes)?;
                                                Ok(convert_heic_to_jpg(&dest))
                                            },
                                        )
                                        .await
                                        {
                                            Ok(Ok(path)) => path,
                                            Ok(Err(e)) => {
                                                log::warn!("gmessages: write media file: {e}");
                                                return;
                                            }
                                            Err(e) => {
                                                log::warn!("gmessages: media writer task failed: {e}");
                                                return;
                                            }
                                        };
                                        let _ = event_tx
                                            .send(WaEvent::MediaReady {
                                                msg_id,
                                                chat_id,
                                                path: render_path.to_string_lossy().into_owned(),
                                                media_type: kind,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        log::warn!("gmessages: media download failed: {e}");
                                    }
                                }
                            });
                        }
                    }
                }

                let wa_events = translate_event(event);
                if wa_events.is_empty() {
                    log::debug!("gmessages → desktop: {event_kind} consumed internally (no UI forward)");
                    continue;
                }
                // Two-phase per-event handling so persistence and UI routing
                // can use DIFFERENT chat_ids:
                //   1. PERSIST under the original `gm:` chat_id so the
                //      WhatsApp runtime never touches our message file
                //      (its LoadChat overwrites with server-fetched data).
                //   2. REDIRECT chat_id to the merged WhatsApp JID for the
                //      UI event, so the unified chat row receives it.
                let mm = merge_map.lock().await.clone();
                for mut wa_event in wa_events {
                    // Per-message dedup: server retransmits batches.
                    if let WaEvent::MessageReceived(im) = &wa_event
                        && recent_msgs.check_and_record(&im.id)
                    {
                        log::debug!(
                            "gmessages: dropping duplicate {} for {}",
                            im.id,
                            im.chat_id
                        );
                        continue;
                    }
                    // 2FA inbox routing: if this incoming SMS is detected as
                    // a verification code (bank/login/OTP/etc.), reroute it
                    // to the synthetic "Verification Codes" chat instead of
                    // letting each shortcode (TD, Aeroplan, …) spawn its own
                    // chat row.  Original sender is preserved on the message
                    // so the inbox shows where each code came from. Only
                    // applies to incoming gm messages.
                    if let WaEvent::MessageReceived(im) = &mut wa_event
                        && !im.is_from_me
                        && im.chat_id.starts_with("gm:")
                        && let Some(text) = im.text.as_deref()
                        && detect_two_factor_code(text).is_some()
                    {
                        // Preserve the original sender for display in the
                        // shared inbox. If the sender_name is just a
                        // shortcode digit blob, keep it; otherwise use it.
                        if im.sender_name.is_empty() {
                            im.sender_name = im
                                .chat_id
                                .strip_prefix("gm:")
                                .unwrap_or(&im.chat_id)
                                .to_string();
                        }
                        log::info!(
                            "gmessages: routing 2FA from {} → {}",
                            im.sender_name,
                            VERIFICATION_CODES_CHAT_ID
                        );
                        // Remember the REAL underlying conv id so reseed can
                        // suppress its standalone shortcode row and MarkRead on
                        // the synthetic inbox can watermark + ACK it. Persist on
                        // first sighting of a new conv.
                        let real_conv = strip_prefix(&im.chat_id).to_string();
                        let newly_recorded = {
                            let mut vc = gm_verification_convs.lock().await;
                            vc.insert(real_conv)
                        };
                        if newly_recorded {
                            let snap = gm_verification_convs.lock().await.clone();
                            save_gm_verification_convs(&gm_verif_path, &snap);
                        }
                        im.chat_id = VERIFICATION_CODES_CHAT_ID.into();
                    }
                    // STEP 1: rewrite chat_id for UI routing (gm:N → wa_jid)
                    // if this conversation is merged into a WhatsApp row.
                    // CRITICAL: this MUST happen BEFORE persistence, so the
                    // message file path matches where it'll be rendered.
                    // Previously we persisted first and then redirected —
                    // SMS got saved to wa_messages/gm_6101.bin but the
                    // chat list rendered it under wa_messages/<wa_jid>.bin,
                    // and on restart the WA chat row loaded the wrong
                    // file and your SMS appeared "lost".
                    redirect_chat_id(&mut wa_event, &mm);
                    // STEP 2: persist using the FINAL chat_id (post-2FA,
                    // post-merge-redirect). Path == render target.
                    if let WaEvent::MessageReceived(im) = &wa_event {
                        crate::ui::runtime::save_messages_append(&im.chat_id, im);
                        // The WhatsApp runtime caches each chat's history in
                        // memory and serves chat-opens from that cache without
                        // re-reading disk. We're a separate thread and can't
                        // touch that cache, so flag the chat: LoadChat will
                        // reconcile this just-written message from disk on the
                        // next open instead of showing stale history (which is
                        // why merged SMS used to vanish until an app restart).
                        crate::ui::runtime::mark_gm_dirty(&im.chat_id);
                    }
                    // STEP 3: if the redirect targeted a WhatsApp chat row (a
                    // MERGED gm conversation), route the preview/timestamp
                    // update to the WA runtime via TouchChatSummary. The WA
                    // runtime is the sole owner of non-gm summaries — it applies
                    // the monotonic guards, persists wa_chats.bin through its own
                    // save_tx flusher, and emits ChatRowChanged. (This replaces
                    // the old touch_wa_chat_preview behind-the-back disk write
                    // that raced the flusher.)
                    if let WaEvent::MessageReceived(im) = &wa_event
                        && (im.chat_id.ends_with("@s.whatsapp.net") || im.chat_id.ends_with("@lid"))
                    {
                        let preview = gm_preview_text(im);
                        let _ = wa_cmd_tx.send(WaCommand::TouchChatSummary {
                            chat_id: im.chat_id.clone(),
                            preview,
                            timestamp: im.timestamp,
                            is_from_me: im.is_from_me,
                            ephemeral: false,
                        });
                    }
                    // STEP 4: for an UNMERGED gm chat (chat_id still
                    // `gm:N` after redirect), incrementally update
                    // gm_chats.bin AND emit an authoritative ChatRowChanged from
                    // the returned summary. The cache was previously written
                    // ONLY by list_conversations (once per startup), so
                    // any SMS chat created or updated mid-session never
                    // made it to disk and vanished on the next restart.
                    if let WaEvent::MessageReceived(im) = &wa_event
                        && is_gm_chat(&im.chat_id)
                    {
                        let preview = gm_preview_text(im);
                        let conv = strip_prefix(&im.chat_id).to_string();
                        let is_active =
                            active_conv.lock().unwrap().as_deref() == Some(conv.as_str());
                        let read_wm =
                            gm_read_watermarks.lock().await.get(&conv).copied().unwrap_or(0);
                        if let Some(summary) = upsert_gm_chat_cache(
                            &gm_chats_cache_path,
                            &im.chat_id,
                            &preview,
                            im.timestamp,
                            im.is_from_me,
                            &im.sender_name,
                            is_active,
                            read_wm,
                        ) {
                            let _ = event_tx.send(WaEvent::ChatRowChanged(summary)).await;
                        }
                    }
                    log::info!(
                        "gmessages → desktop: forwarding {}",
                        describe_wa_event(&wa_event)
                    );
                    if event_tx.send(wa_event).await.is_err() {
                        log::info!("gmessages: event channel closed; shutting down");
                        return Ok(());
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                // SetActiveChat is applied INLINE (not via the spawned
                // handle_command): its handler is tokio::spawn'd, so a rapid
                // A→B→A switch could land a stale `Some(A)` after `Some(B)` and
                // mis-suppress B's unread bumps — the same race the WA runtime
                // fixes inline. Store the conv id (gm-prefix stripped, or the
                // merge_map reverse-lookup for a merged WA id) so G2's increment
                // can suppress a bump for the actively-viewed conversation.
                if let WaCommand::SetActiveChat { chat_id } = &cmd {
                    let conv = match chat_id {
                        Some(id) if is_gm_chat(id) => Some(strip_prefix(id).to_string()),
                        Some(id) => {
                            // Merged WA id → find the conv that maps to it.
                            let mm = merge_map.lock().await;
                            mm.iter()
                                .find(|(_, target)| *target == id)
                                .map(|(conv, _)| conv.clone())
                        }
                        None => None,
                    };
                    *active_conv.lock().unwrap() = conv;
                    continue;
                }
                if matches!(cmd, WaCommand::GmessagesRepair) {
                    log::warn!("gmessages: re-pair requested via WaCommand");
                    let _ = client.disconnect().await;
                    // Intentionally NOT wiping the auth file — notify_auth_changed
            // overwrites it with fresh contents on a successful re-pair,
            // and if the new pair fails we'd rather keep the old (possibly
            // recoverable) auth than be left with nothing. User asked for
            // this explicitly: "why are you letting the auth file be
            // deleted".
                    if let Err(e) = run_pair_flow(&client, &auth_path, &mut events, &event_tx).await {
                        log::warn!("gmessages: re-pair failed: {e}");
                    } else if let Err(e) = client.connect().await {
                        log::warn!("gmessages: reconnect after re-pair failed: {e}");
                    }
                    continue;
                }
                let client = client.clone();
                let event_tx = event_tx.clone();
                let merge_map = merge_map.clone();
                let gm_read_watermarks = gm_read_watermarks.clone();
                let gm_read_wm_path = gm_read_wm_path.clone();
                let gm_verification_convs = gm_verification_convs.clone();
                let wa_cmd_tx = wa_cmd_tx.clone();
                let gm_chats_cache_path = gm_chats_cache_path.clone();
                let contact_cache = contact_cache.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_command(
                        &client,
                        &event_tx,
                        &merge_map,
                        &gm_read_watermarks,
                        &gm_read_wm_path,
                        &gm_verification_convs,
                        &wa_cmd_tx,
                        &gm_chats_cache_path,
                        &contact_cache,
                        cmd,
                    )
                    .await
                    {
                        log::warn!("gmessages: command error: {e:#}");
                    }
                });
            }
            // 2-second tick used to wake the loop so the top-of-iteration
            // signal polls (take_repair_request / take_gaia_request) get
            // a chance to run when events are quiet. The actual checks
            // live at the top of the loop body, not here.
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            else => {
                log::info!("gmessages: all channels closed; shutting down");
                return Ok(());
            }
        }
    }
}

/// Persist + emit the sidebar-row update for a locally-built SEND echo (an
/// SMS/MMS we just sent). `chat_id` is the `gm:` pair id the UI sent with (never
/// merge-redirected — see correction 6). If that conversation is merged into a
/// WhatsApp row, we route the update to the WA runtime (the sole owner of that
/// row) via `TouchChatSummary` with the WA id; otherwise we update `gm_chats.bin`
/// and emit a `ChatRowChanged` for the gm: id. Without this a sent SMS never
/// moved its row once `bump_chat_to_top` was deleted.
async fn emit_sent_echo_row(
    echo: &IncomingMessage,
    merge_map: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
    wa_cmd_tx: &TokioUnboundedSender<WaCommand>,
    gm_chats_cache_path: &std::path::Path,
    event_tx: &Sender<WaEvent>,
) {
    let preview = gm_preview_text(echo);
    let conv = strip_prefix(&echo.chat_id).to_string();
    let merged_target = merge_map.lock().await.get(&conv).cloned();
    if let Some(wa_id) = merged_target {
        // Merged chat: the visible row is the WA row — route via the WA runtime.
        let _ = wa_cmd_tx.send(WaCommand::TouchChatSummary {
            chat_id: wa_id,
            preview,
            timestamp: echo.timestamp,
            is_from_me: true,
            ephemeral: false,
        });
    } else {
        // Unmerged gm chat: is_from_me clears unread; watermark/active irrelevant.
        if let Some(summary) = upsert_gm_chat_cache(
            gm_chats_cache_path,
            &echo.chat_id,
            &preview,
            echo.timestamp,
            true,
            &echo.sender_name,
            false,
            0,
        ) {
            let _ = event_tx.send(WaEvent::ChatRowChanged(summary)).await;
        }
    }
}

/// Handle a `WaCommand` whose `chat_id` belongs to a gmessages chat.
async fn handle_command(
    client: &Arc<Client>,
    event_tx: &Sender<WaEvent>,
    merge_map: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
    read_watermarks: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, i64>>>,
    wm_path: &std::path::Path,
    verification_convs: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    wa_cmd_tx: &TokioUnboundedSender<WaCommand>,
    gm_chats_cache_path: &std::path::Path,
    contact_cache: &ContactCache,
    cmd: WaCommand,
) -> Result<()> {
    use crate::bridge::IncomingMessage;
    match cmd {
        // SMS/RCS has no reply-quoting, so a SendReply to a gm chat is downgraded
        // to a plain text SMS (the reply text still sends) rather than being
        // silently dropped and leaving the optimistic bubble stuck forever.
        WaCommand::SendText {
            chat_id,
            text,
            tmp_id,
            ..
        }
        | WaCommand::SendReply {
            chat_id,
            text,
            tmp_id,
            ..
        } => {
            let conv = strip_prefix(&chat_id);
            log::info!("gmessages: SendText to {conv}: {text:?}");
            match client.send_text(conv, &text).await {
                Ok(real_id) => {
                    // Tag both the temporary and server-assigned IDs with the
                    // `gm:` prefix so MessageConfirmed re-keys the optimistic
                    // bubble correctly and the bubble renderer keeps SMS
                    // styling.
                    let tagged_real = if real_id.starts_with(CHAT_PREFIX) {
                        real_id.clone()
                    } else {
                        format!("{CHAT_PREFIX}{real_id}")
                    };
                    let _ = event_tx
                        .send(WaEvent::MessageConfirmed {
                            tmp_id: tmp_id.clone(),
                            real_id: tagged_real.clone(),
                            chat_id: chat_id.clone(),
                        })
                        .await;
                    // Also push a MessageReceived for the sent message so it
                    // gets persisted into the chat_view cache and survives a
                    // chat switch. The phone will eventually echo it back via
                    // the long-poll, which dedups by message_id.
                    let now_s = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let echo = IncomingMessage {
                        media_download: None,
                        id: tagged_real.clone(),
                        chat_id: chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: Some(text),
                        media_type: None,
                        timestamp: now_s,
                        is_from_me: true,
                        quoted_msg_id: None,
                        quoted_text: None,
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
                        poll_question: None,
                        poll_options: vec![],
                        poll_selectable: 0,
                        poll_secret: vec![],
                        poll_votes: vec![],
                        receipt_status: crate::bridge::ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                    };
                    // Persist + bump the row (merged → WA runtime, else gm cache).
                    emit_sent_echo_row(&echo, merge_map, wa_cmd_tx, gm_chats_cache_path, event_tx)
                        .await;
                    let _ = event_tx
                        .send(WaEvent::MessageReceived(Box::new(echo)))
                        .await;
                }
                Err(e) => {
                    log::warn!("gmessages: send_text failed: {e}");
                    let _ = event_tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                }
            }
        }
        // Resend of a previously-failed SMS bubble. Re-send via the same
        // send_text path as SendText and re-key the existing failed bubble
        // (msg_id) via MessageConfirmed on success, or re-emit MessageFailed
        // on error so the red ✗ / Resend affordance stays.
        WaCommand::ResendMessage {
            chat_id,
            msg_id,
            text,
        } => {
            let conv = strip_prefix(&chat_id);
            log::info!("gmessages: ResendMessage to {conv}: {text:?}");
            match client.send_text(conv, &text).await {
                Ok(real_id) => {
                    let tagged_real = if real_id.starts_with(CHAT_PREFIX) {
                        real_id.clone()
                    } else {
                        format!("{CHAT_PREFIX}{real_id}")
                    };
                    let _ = event_tx
                        .send(WaEvent::MessageConfirmed {
                            tmp_id: msg_id.clone(),
                            real_id: tagged_real.clone(),
                            chat_id: chat_id.clone(),
                        })
                        .await;
                    let now_s = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let echo = IncomingMessage {
                        media_download: None,
                        id: tagged_real.clone(),
                        chat_id: chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: Some(text),
                        media_type: None,
                        timestamp: now_s,
                        is_from_me: true,
                        quoted_msg_id: None,
                        quoted_text: None,
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
                        poll_question: None,
                        poll_options: vec![],
                        poll_selectable: 0,
                        poll_secret: vec![],
                        poll_votes: vec![],
                        receipt_status: crate::bridge::ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                    };
                    emit_sent_echo_row(&echo, merge_map, wa_cmd_tx, gm_chats_cache_path, event_tx)
                        .await;
                    let _ = event_tx
                        .send(WaEvent::MessageReceived(Box::new(echo)))
                        .await;
                }
                Err(e) => {
                    log::warn!("gmessages: ResendMessage send_text failed: {e}");
                    let _ = event_tx
                        .send(WaEvent::MessageFailed { msg_id, chat_id })
                        .await;
                }
            }
        }
        WaCommand::LoadChat { chat_id, chat_name } => {
            let conv = strip_prefix(&chat_id);
            log::info!("gmessages: LoadChat for {conv}");

            // Start from on-disk cache so locally-known history (including
            // anything we sent since the last server fetch) shows up.
            let mut merged: Vec<IncomingMessage> = crate::ui::runtime::load_messages(&chat_id);
            log::debug!(
                "gmessages: LoadChat starting with {} cached messages",
                merged.len()
            );
            let mut have: std::collections::HashSet<String> =
                merged.iter().map(|m| m.id.clone()).collect();

            match client.fetch_messages(conv, 100).await {
                Ok(resp) => {
                    // Mine sender_participant for live name updates —
                    // but ONLY from incoming messages. The user's own
                    // sender_participant on outgoing messages would
                    // rename the chat to the user's own name otherwise.
                    let contacts = contact_cache.lock().await.clone();
                    for m in &resp.messages {
                        if gm_message_is_from_me(m) {
                            continue;
                        }
                        if let Some(sp) = &m.sender_participant {
                            if let Some(mut name) = participant_display_name(sp, &contacts) {
                                if let Some(summary) =
                                    update_gm_chat_name_cache(gm_chats_cache_path, &chat_id, &name)
                                {
                                    name = summary.name.clone();
                                    let _ = event_tx.send(WaEvent::ChatRowChanged(summary)).await;
                                }
                                let _ = event_tx
                                    .send(WaEvent::ChatNameUpdated {
                                        chat_id: chat_id.clone(),
                                        name,
                                    })
                                    .await;
                                break;
                            }
                        }
                    }
                    // Server returns newest-first; iterate reversed so we
                    // append in chronological order. Dedupe by message_id
                    // against the disk cache.
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    for m in resp.messages.iter().rev() {
                        if have.contains(&m.message_id) {
                            continue;
                        }
                        if let Some(im) = message_to_incoming(m) {
                            have.insert(im.id.clone());
                            // First sighting of this message, and it never came
                            // through live push — scan it here or the code is
                            // lost. Age-gated so opening a chat can't resurrect
                            // a months-old OTP into the clipboard.
                            let ts_secs = if im.timestamp > 10_000_000_000 {
                                im.timestamp / 1000
                            } else {
                                im.timestamp
                            };
                            if !im.is_from_me
                                && (now_secs - ts_secs).abs() < 600
                                && let Some(text) = im.text.as_deref()
                                && let Some(code) = detect_two_factor_code(text)
                            {
                                log::info!(
                                    "gmessages: 2FA {} found on server-fetch path (msg {}, {}s old)",
                                    code,
                                    im.id,
                                    now_secs - ts_secs
                                );
                                let _ = event_tx
                                    .send(WaEvent::TwoFactorCodeDetected {
                                        msg_id: im.id.clone(),
                                        code,
                                        sender: if im.sender_name.is_empty() {
                                            chat_name.clone()
                                        } else {
                                            im.sender_name.clone()
                                        },
                                    })
                                    .await;
                            }
                            merged.push(im);
                        }
                    }
                    // Persist the merged set so future restarts see the same
                    // ordering. Scoped to SMS — a WhatsApp save for this
                    // (possibly merged) chat must not be able to drop it.
                    crate::ui::runtime::save_messages_scoped(
                        &chat_id,
                        crate::bridge::MessageSource::GoogleMessages,
                        &merged,
                    );
                }
                Err(e) => {
                    log::warn!("gmessages: fetch_messages failed (using disk cache only): {e}")
                }
            }

            merged.sort_by_key(|m| m.timestamp);
            log::info!(
                "gmessages: LoadChat returning {} messages (disk + server merged)",
                merged.len()
            );
            let _ = event_tx
                .send(WaEvent::HistoryMessages {
                    chat_id,
                    chat_name,
                    messages: merged,
                })
                .await;
        }
        WaCommand::MarkRead { chat_id } => {
            // Resolve `chat_id` to a gm conversation_id. Two cases:
            //
            // 1. `gm:N` — direct (unmerged gm chat). conv = N.
            // 2. A WhatsApp JID — could be a MERGED gm chat. Look up
            //    the merge_map in reverse: any conv that mapped to
            //    this WA jid. There may be more than one (multiple
            //    gm threads for same person eventually merged into
            //    one WA row) — mark all of them read.
            //
            // Without this, every list_conversations() on restart
            // still flags `unread=true` on conversations the user
            // already read on the desktop, and they keep popping
            // back into the Unread filter.
            // The synthetic "Verification Codes" inbox has no real
            // conversation_id of its own ("verification-codes" is a bogus id
            // that ACKs a nonexistent conv). Its messages come from many real
            // shortcode convs recorded in `verification_convs` — fan the read
            // watermark to ALL of them and SKIP the bogus server ACK. Without
            // this the inbox could never be marked read and its shortcodes kept
            // re-flagging unread on every reseed.
            let is_verification_inbox = chat_id == VERIFICATION_CODES_CHAT_ID;
            let mut convs: Vec<String> = Vec::new();
            if is_verification_inbox {
                convs.extend(verification_convs.lock().await.iter().cloned());
            } else if is_gm_chat(&chat_id) {
                convs.push(strip_prefix(&chat_id).to_string());
            } else {
                let mm = merge_map.lock().await;
                for (conv, target) in mm.iter() {
                    if target == &chat_id {
                        convs.push(conv.clone());
                    }
                }
            }
            if convs.is_empty() {
                // Not a gm-relevant chat. Silently ignore (this gets
                // fanned out to every MarkRead now, so WhatsApp-only
                // chats land here too).
                return Ok(());
            }

            // We need a message_id per conv. Load whatever we have
            // persisted under the rendered chat_id (post-redirect, so
            // merged chats share one file). Find the most recent
            // message tagged with `gm:` — that's a gm message.
            let messages = crate::ui::runtime::load_messages(&chat_id);

            // Stamp the LOCAL read watermark first — before any early return —
            // so the badge stays cleared across restart even if we can't find a
            // gm message_id to ACK to Google.
            //
            // Watermark = "read up to NOW", floored by the newest message we
            // have. The earlier version used only max(persisted message ts),
            // which broke two ways and left SMS stuck unread:
            //   • verification-code / notification chats keep no persisted
            //     messages (their SMS route to the synthetic Verification Codes
            //     inbox), so max(..) was 0 and the `> 0` guard wrote NO watermark;
            //   • a conversation's `last_message_timestamp` (used on reseed) can
            //     sit AHEAD of the newest message we persisted, so the watermark
            //     landed below summary.timestamp and the `wm >= ts` clamp missed.
            // `now` is always > 0 and >= any past message, so opening a chat
            // always records a usable watermark and the clamp fires; a genuinely
            // newer message (ts past the read time) still shows unread.
            let now_s = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let watermark_ts = now_s.max(messages.iter().map(|m| m.timestamp).max().unwrap_or(0));
            if watermark_ts > 0 {
                let snapshot = {
                    let mut wm = read_watermarks.lock().await;
                    let mut changed = false;
                    for conv in &convs {
                        if wm.get(conv).copied().unwrap_or(0) < watermark_ts {
                            wm.insert(conv.clone(), watermark_ts);
                            changed = true;
                        }
                    }
                    if changed { Some(wm.clone()) } else { None }
                };
                if let Some(snap) = snapshot {
                    save_gm_watermarks(wm_path, &snap);
                }
            }

            // Also clear unread_count in gm_chats.bin for the marked conv(s), so a
            // boot that fails to reseed (e.g. the observed `list_conversations`
            // rpc timeout) doesn't depend solely on the watermark clamp to hide an
            // already-read badge. gm_chats.bin lives beside the watermark file.
            if let Some(parent) = wm_path.parent() {
                let cache_path = parent.join("gm_chats.bin");
                let mut cache = gm_load_chats_cache(&cache_path);
                let mut changed = false;
                // Collect the cleared summaries so we can emit an authoritative
                // ChatRowChanged for each (the WA runtime's emit_row is a no-op
                // for gm: ids — gm rows are not in RuntimeState.chats).
                let mut cleared: Vec<ChatSummary> = Vec::new();
                for c in cache.iter_mut() {
                    let cid = strip_prefix(&c.id);
                    if c.unread_count != 0 && convs.iter().any(|conv| conv == cid) {
                        c.unread_count = 0;
                        changed = true;
                        cleared.push(c.clone());
                    }
                }
                if changed {
                    gm_save_chats_cache(&cache_path, &cache);
                    for summary in cleared {
                        let _ = event_tx.send(WaEvent::ChatRowChanged(summary)).await;
                    }
                }
            }

            // For the synthetic Verification Codes inbox we've watermarked all
            // real convs above; the inbox file mixes msg_ids from many convs, so
            // we can't reliably pair a msg_id to its conv for a server ACK.
            // Skip the ACK (the local watermark keeps them read across restart).
            if is_verification_inbox {
                return Ok(());
            }

            let latest_gm_msg_id = messages
                .iter()
                .rev()
                .find_map(|m| m.id.strip_prefix(CHAT_PREFIX).map(|s| s.to_string()));
            let Some(msg_id) = latest_gm_msg_id else {
                log::debug!(
                    "gmessages: MarkRead for {chat_id} — no persisted gm messages, skipping"
                );
                return Ok(());
            };
            for conv in convs {
                log::info!("gmessages: marking {conv} read via msg {msg_id}");
                if let Err(e) = client.mark_read(&conv, &msg_id).await {
                    log::warn!("gmessages: mark_read({conv}) failed: {e}");
                }
            }
        }
        // Mark-as-unread: the user deliberately flagged an SMS chat unread. The
        // WA runtime handles the live UI/badge, but on reseed our local read
        // watermark would re-clamp the chat back to read (unread=0). Roll the gm
        // watermark BACK below the chat's latest activity so `apply_gm_read_
        // watermark` no longer clamps and Google's `unread` flag survives the
        // reseed. (Routing that makes MarkUnread reach the gm runtime is
        // coordinator/runtime.rs work — reported in api_changes.)
        WaCommand::MarkUnread { chat_id } => {
            let mut convs: Vec<String> = Vec::new();
            if chat_id == VERIFICATION_CODES_CHAT_ID {
                convs.extend(verification_convs.lock().await.iter().cloned());
            } else if is_gm_chat(&chat_id) {
                convs.push(strip_prefix(&chat_id).to_string());
            } else {
                let mm = merge_map.lock().await;
                for (conv, target) in mm.iter() {
                    if target == &chat_id {
                        convs.push(conv.clone());
                    }
                }
            }
            if convs.is_empty() {
                // Not a gm-relevant chat (fanned out to every MarkUnread now).
                return Ok(());
            }
            let snapshot = {
                let mut wm = read_watermarks.lock().await;
                let mut changed = false;
                for conv in &convs {
                    // Removing the watermark drops it below any real activity
                    // timestamp, so the reseed clamp never fires for this chat.
                    if wm.remove(conv).is_some() {
                        changed = true;
                    }
                }
                if changed { Some(wm.clone()) } else { None }
            };
            if let Some(snap) = snapshot {
                log::info!("gmessages: MarkUnread cleared read watermark for {convs:?}");
                save_gm_watermarks(wm_path, &snap);
            }
            // Persist unread=1 into gm_chats.bin AND emit ChatRowChanged for each
            // affected gm row. With ChatMarkedUnread deleted and the WA inline
            // block a no-op for gm ids, this is the ONLY thing that gives an
            // SMS-only chat its badge — and makes it survive restart + reseed.
            {
                let mut cache = gm_load_chats_cache(gm_chats_cache_path);
                let mut changed = false;
                let mut marked: Vec<ChatSummary> = Vec::new();
                for c in cache.iter_mut() {
                    let cid = strip_prefix(&c.id);
                    if convs.iter().any(|conv| conv == cid) {
                        if c.unread_count == 0 {
                            c.unread_count = 1;
                        }
                        changed = true;
                        marked.push(c.clone());
                    }
                }
                if changed {
                    gm_save_chats_cache(gm_chats_cache_path, &cache);
                    for summary in marked {
                        let _ = event_tx.send(WaEvent::ChatRowChanged(summary)).await;
                    }
                }
            }
        }
        WaCommand::SetTyping { chat_id, is_typing } => {
            let conv = strip_prefix(&chat_id);
            if let Err(e) = client.set_typing(conv, is_typing).await {
                log::warn!("gmessages: set_typing failed: {e}");
            }
        }
        WaCommand::SendReaction {
            chat_id,
            msg_id,
            emoji,
            ..
        } => {
            let conv = strip_prefix(&chat_id).to_string();
            let raw_msg_id = strip_prefix(&msg_id).to_string();
            // Google Messages reaction action: 1 = Add, 2 = Remove. The UI
            // sends an empty emoji to clear an existing reaction.
            let action = if emoji.is_empty() { 2 } else { 1 };
            log::info!("gmessages: SendReaction {emoji:?} on msg {raw_msg_id} (action {action})");
            match client
                .send_reaction(&conv, &raw_msg_id, &emoji, action)
                .await
            {
                Ok(()) => {
                    // Persist directly so the reaction survives a restart —
                    // the long-poll echo path doesn't re-save existing
                    // messages, so without this it would be lost. Do this FIRST
                    // (and synchronously) so we can emit the FULL merged reaction
                    // set: sending just our own `("", emoji)` would wipe every
                    // other participant's reaction from the bubble until reload.
                    let cid = chat_id.clone();
                    let mid = msg_id.clone();
                    let emo = emoji.clone();
                    let merged = tokio::task::spawn_blocking(move || {
                        let mut msgs = crate::ui::runtime::load_messages(&cid);
                        let is_latest = msgs
                            .iter()
                            .max_by_key(|m| m.timestamp)
                            .map(|m| m.id == mid)
                            .unwrap_or(false);
                        if let Some(m) = msgs.iter_mut().find(|m| m.id == mid) {
                            m.reactions.retain(|(who, _)| who != "me");
                            if !emo.is_empty() {
                                m.reactions.push(("me".to_string(), emo));
                            }
                            let merged = m.reactions.clone();
                            crate::ui::runtime::save_messages_scoped(
                                &cid,
                                MessageSource::GoogleMessages,
                                &msgs,
                            );
                            Some((merged, is_latest))
                        } else {
                            None
                        }
                    })
                    .await
                    .unwrap_or(None);
                    // Optimistic UI update — show it immediately rather than
                    // waiting for the phone's long-poll echo. Prefer the merged
                    // persisted set (others' reactions + our add/removal); fall
                    // back to just our own change if the message wasn't cached.
                    // The bubble renders an EMPTY sender as "You", so translate
                    // the persisted self key ("me") back to "" for the event.
                    let (reactions, is_latest): (Vec<(String, String)>, bool) = match merged {
                        Some((r, is_latest)) => (
                            r.into_iter()
                                .map(|(who, e)| {
                                    if who == "me" {
                                        (String::new(), e)
                                    } else {
                                        (who, e)
                                    }
                                })
                                .collect(),
                            is_latest,
                        ),
                        None if emoji.is_empty() => (Vec::new(), false),
                        None => (vec![(String::new(), emoji.clone())], false),
                    };
                    // Ephemeral "Reacted 👍" sidebar override on the latest
                    // message (adds only, never persisted — restart shows the
                    // underlying text). For a gm: row, emit ChatRowChanged from
                    // the gm store clone; for a merged conv, route it to the WA
                    // runtime via TouchChatSummary with ephemeral:true so the
                    // visible WA row shows the reaction (correction 3).
                    if is_latest && !emoji.is_empty() {
                        let ephem_preview = format!("You reacted {emoji}");
                        let merged_target = merge_map.lock().await.get(&conv).cloned();
                        if let Some(wa_id) = merged_target {
                            let _ = wa_cmd_tx.send(WaCommand::TouchChatSummary {
                                chat_id: wa_id,
                                preview: ephem_preview,
                                timestamp: 0,
                                is_from_me: true,
                                ephemeral: true,
                            });
                        } else if is_gm_chat(&chat_id) {
                            if let Some(mut summary) = gm_load_chats_cache(gm_chats_cache_path)
                                .into_iter()
                                .find(|c| c.id == chat_id)
                            {
                                summary.last_message = ephem_preview;
                                let _ = event_tx.send(WaEvent::ChatRowChanged(summary)).await;
                            }
                        }
                    }
                    let _ = event_tx
                        .send(WaEvent::ReactionUpdated {
                            chat_id: chat_id.clone(),
                            msg_id: msg_id.clone(),
                            reactions,
                            is_latest,
                        })
                        .await;
                }
                Err(e) => log::warn!("gmessages: send_reaction failed: {e}"),
            }
        }
        WaCommand::SendImage {
            chat_id,
            path,
            caption,
            tmp_id,
        } => {
            let conv = strip_prefix(&chat_id).to_string();
            let data = match tokio::fs::read(&path).await {
                Ok(d) => d,
                Err(e) => {
                    log::warn!("gmessages: SendImage cannot read {path}: {e}");
                    let _ = event_tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                    return Ok(());
                }
            };
            let mime = guess_mime(&path);
            let file_name = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".to_string());
            log::info!(
                "gmessages: SendImage {file_name} ({mime}, {} bytes) → {conv}",
                data.len()
            );
            match client
                .send_media(&conv, &data, &file_name, &mime, caption.as_deref())
                .await
            {
                Ok(real_tmp) => {
                    let tagged = format!("{CHAT_PREFIX}{real_tmp}");
                    // Re-key the optimistic bubble to the relay's id.
                    let _ = event_tx
                        .send(WaEvent::MessageConfirmed {
                            tmp_id: tmp_id.clone(),
                            real_id: tagged.clone(),
                            chat_id: chat_id.clone(),
                        })
                        .await;
                    // Echo it back as a received message so it shows + is
                    // cached immediately. We still have the file locally, so
                    // point media_local_path straight at it. The phone also
                    // echoes the real message via the long-poll, which
                    // dedups by message_id.
                    let now_s = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let kind = if mime.starts_with("video/") {
                        MediaType::Video
                    } else if mime.starts_with("audio/") {
                        MediaType::Audio
                    } else if mime == "image/gif" {
                        MediaType::Gif
                    } else if mime.starts_with("image/") {
                        MediaType::Image
                    } else {
                        MediaType::Document
                    };
                    let echo = IncomingMessage {
                        media_download: None,
                        id: tagged,
                        chat_id: chat_id.clone(),
                        sender_id: String::new(),
                        sender_name: String::new(),
                        text: None,
                        media_type: Some(kind),
                        timestamp: now_s,
                        is_from_me: true,
                        quoted_msg_id: None,
                        quoted_text: None,
                        quoted_sender: None,
                        is_forwarded: false,
                        forwarding_score: 0,
                        reactions: vec![],
                        media_local_path: Some(path.clone()),
                        media_filename: Some(file_name),
                        media_caption: caption.clone(),
                        contact_name: None,
                        contact_vcard: None,
                        link_title: None,
                        link_description: None,
                        link_url: None,
                        link_thumbnail_path: None,
                        quoted_media_path: None,
                        poll_question: None,
                        poll_options: vec![],
                        poll_selectable: 0,
                        poll_secret: vec![],
                        poll_votes: vec![],
                        receipt_status: ReceiptStatus::Sent,
                        is_edited: false,
                        is_system_message: false,
                    };
                    emit_sent_echo_row(&echo, merge_map, wa_cmd_tx, gm_chats_cache_path, event_tx)
                        .await;
                    let _ = event_tx
                        .send(WaEvent::MessageReceived(Box::new(echo)))
                        .await;
                }
                Err(e) => {
                    log::warn!("gmessages: SendImage failed: {e}");
                    let _ = event_tx
                        .send(WaEvent::MessageFailed {
                            msg_id: tmp_id,
                            chat_id,
                        })
                        .await;
                }
            }
        }
        // GIFs and stickers cannot be reconstructed as SMS/RCS attachments.
        // Mark the optimistic bubble failed and explain the protocol limitation;
        // a bare red X looked like a transient network error and invited a retry
        // that could never succeed.
        WaCommand::SendGif {
            chat_id, tmp_id, ..
        }
        | WaCommand::SendSticker {
            chat_id, tmp_id, ..
        } => {
            log::warn!("gmessages: {chat_id} — GIF/sticker not supported over SMS; marking failed");
            let _ = event_tx
                .send(WaEvent::MessageFailed {
                    msg_id: tmp_id,
                    chat_id,
                })
                .await;
            let _ = event_tx
                .send(WaEvent::ErrorToast(
                    "GIFs and stickers can’t be sent over SMS/RCS. Choose a WhatsApp chat instead."
                        .to_string(),
                ))
                .await;
        }
        // Voice notes are likewise unsupported, but retain their existing
        // failure-bubble behavior independently from picker media.
        WaCommand::SendAudio {
            chat_id, tmp_id, ..
        } => {
            log::warn!("gmessages: {chat_id} — voice notes not supported over SMS; marking failed");
            let _ = event_tx
                .send(WaEvent::MessageFailed {
                    msg_id: tmp_id,
                    chat_id,
                })
                .await;
        }
        // Contact cards can't be sent over SMS/RCS via gmessages; the UI
        // already showed an optimistic ⏳ bubble (tmp_id) and cleared the
        // composer. Mark it FAILED (red ✗ + Resend) instead of leaving the
        // bubble hanging on ⏳ forever.
        WaCommand::SendContact {
            to_chat_id, tmp_id, ..
        } => {
            log::warn!(
                "gmessages: {to_chat_id} — contact card not supported over SMS; marking failed"
            );
            let _ = event_tx
                .send(WaEvent::MessageFailed {
                    msg_id: tmp_id,
                    chat_id: to_chat_id,
                })
                .await;
        }
        other => {
            log::debug!(
                "gmessages: dropping unsupported command for gm chat: {}",
                std::any::type_name_of_val(&other)
            );
        }
    }
    Ok(())
}

fn describe_event(e: &Event) -> &'static str {
    match e {
        Event::Ready => "Ready",
        Event::QrCode { .. } => "QrCode",
        Event::PairingEmoji { .. } => "PairingEmoji",
        Event::AvailableGoogleAccounts { .. } => "AvailableGoogleAccounts",
        Event::PairSuccess => "PairSuccess",
        Event::PairFailed { .. } => "PairFailed",
        Event::PhoneNotResponding => "PhoneNotResponding",
        Event::PhoneRespondingAgain => "PhoneRespondingAgain",
        Event::Messages { .. } => "Messages",
        Event::ConversationUpdate { .. } => "ConversationUpdate",
        Event::Typing { .. } => "Typing",
        Event::AuthRevoked => "AuthRevoked",
    }
}

fn describe_wa_event(e: &WaEvent) -> String {
    match e {
        WaEvent::MessageReceived(im) => format!(
            "MessageReceived(chat_id={}, from={}, body={:?})",
            im.chat_id,
            if im.is_from_me {
                "<self>"
            } else {
                im.sender_id.as_str()
            },
            // Truncate to 40 *chars*, not bytes. `&s[..40]` panics when byte
            // 40 lands inside a multi-byte char (smart quote, emoji, accent)
            // — that crashed the whole gmessages task on a real message (see
            // crash.log). char_indices yields valid byte boundaries.
            im.text.as_deref().map(|s| match s.char_indices().nth(40) {
                Some((i, _)) => &s[..i],
                None => s,
            }),
        ),
        WaEvent::TypingIndicator {
            chat_id, is_typing, ..
        } => {
            format!("TypingIndicator(chat_id={chat_id}, on={is_typing})")
        }
        WaEvent::ErrorToast(s) => format!("ErrorToast({s:?})"),
        WaEvent::Disconnected(s) => format!("Disconnected({s:?})"),
        other => format!("{other:?}"),
    }
}

/// Drive a fresh QR pairing flow. Renders the QR to stderr so the user can
/// scan it from the terminal where they launched the desktop. Blocks until
/// the phone confirms pairing or the flow fails.
async fn run_pair_flow(
    client: &Arc<Client>,
    auth_path: &Path,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    event_tx: &Sender<WaEvent>,
) -> Result<()> {
    use qrcode::QrCode;
    use qrcode::render::unicode::Dense1x2;

    // Big visible banner on stderr so the user notices when the QR is up.
    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════╗");
    eprintln!("║                                                          ║");
    eprintln!("║   GOOGLE MESSAGES PAIRING REQUIRED                       ║");
    eprintln!("║                                                          ║");
    eprintln!("║   A QR code will appear below. Open Google Messages     ║");
    eprintln!("║   on your phone → Settings → Device pairing → QR code   ║");
    eprintln!("║   scanner. SMS won't arrive until you scan it.          ║");
    eprintln!("║                                                          ║");
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprintln!();
    log::warn!(
        "gmessages: PAIRING REQUIRED — a QR code will appear on stderr. Open Google Messages on your phone → Device pairing → QR code scanner."
    );
    // Fire a desktop notification too so the user sees it even when they
    // alt-tabbed away from the terminal.
    let _ = std::process::Command::new("notify-send")
        .arg("-u")
        .arg("critical")
        .arg("-t")
        .arg("0") // never auto-dismiss
        .arg("Google Messages: pairing required")
        .arg("SMS will not arrive until you scan the QR code in the terminal.")
        .spawn();
    let _ = event_tx
        .send(WaEvent::ErrorToast(
            "Google Messages: scan the QR code in your terminal — SMS paused until then".into(),
        ))
        .await;

    // Run the pairing handshake on a background task so we can pump events here.
    let pair_task = {
        let c = client.clone();
        tokio::spawn(async move { c.start_pairing().await })
    };

    loop {
        // Escape hatch: if the user clicked "Pair via Firefox" while we
        // were stuck on the QR flow, abort the QR pair_task and run the
        // Gaia flow inline. We CONSUME the flag here so the outer loop's
        // `take_gaia_request()` is a no-op when we return — the work
        // is already done.
        if crate::gm_qr_state::take_gaia_request() {
            log::warn!("gmessages: QR pair flow aborting — Gaia pair requested");
            crate::gm_qr_state::set(None);
            pair_task.abort();
            // Surface the Starting status immediately so the dialog
            // doesn't sit on whatever it was before.
            crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Starting);
            return run_gaia_pair_flow(client, auth_path, events, event_tx).await;
        }
        let event = match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv())
            .await
        {
            Ok(Some(e)) => e,
            Ok(None) => break,  // channel closed
            Err(_) => continue, // timer tick — re-check escape hatch
        };
        match event {
            Event::QrCode { url } => {
                // Publish to the in-app settings page.
                crate::gm_qr_state::set(Some(url.clone()));
                // Also render in the terminal for users not in settings.
                eprintln!(
                    "\n=== Google Messages pairing — scan with phone (or open Settings) ===\n"
                );
                let code = QrCode::new(url.as_bytes()).context("gmessages: failed to encode QR")?;
                let rendered = code
                    .render::<Dense1x2>()
                    .dark_color(Dense1x2::Light)
                    .light_color(Dense1x2::Dark)
                    .quiet_zone(true)
                    .build();
                eprintln!("{rendered}");
                eprintln!("(also: {url})\n");
            }
            Event::PairSuccess => {
                crate::gm_qr_state::set(None);
                eprintln!("=== Paired ===\n");
                log::info!("gmessages: paired; auth saved to {}", auth_path.display());
                break;
            }
            Event::PairFailed { reason } => {
                anyhow::bail!("gmessages: pair failed: {reason}");
            }
            Event::AuthRevoked => {
                anyhow::bail!("gmessages: auth revoked during pairing");
            }
            other => log::debug!(
                "gmessages: ignoring event during pair: {}",
                describe_event(&other)
            ),
        }
    }

    pair_task
        .await
        .context("gmessages: pair task panicked")?
        .context("gmessages: start_pairing returned error")?;
    Ok(())
}

/// Drive a Gaia (Google account) pairing flow.
///
/// 1. Read Google session cookies from the user's Firefox profile.
/// 2. Hand them to the client.
/// 3. Spawn `client.start_gaia_pairing()` on a background task.
/// 4. Pump events: when `Event::PairingEmoji` arrives, publish to the
///    settings page; poll `take_gaia_confirmation` for the user's answer
///    and forward it via `client.confirm_pairing_emoji`.
/// 5. Block until `Event::PairSuccess` or failure.
async fn run_gaia_pair_flow(
    client: &Arc<Client>,
    auth_path: &Path,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    event_tx: &Sender<WaEvent>,
) -> Result<()> {
    log::info!("gmessages: gaia pair: reading Firefox cookies");
    crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::ReadingCookies);
    let cookies = match gmessages_rust::cookies::read_default_firefox_cookies() {
        Ok(c) => c,
        Err(e) => {
            anyhow::bail!(
                "gaia: couldn't read Firefox cookies: {e}. \
                 Sign into https://messages.google.com/web/ in Firefox first."
            );
        }
    };
    log::info!("gmessages: gaia pair: loaded {} cookies", cookies.len());
    client.set_cookies(cookies).await;

    crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::ContactingGoogle);

    // Spawn pairing on a background task; pump events here AND watch
    // pair_task itself, so when start_gaia_pairing returns Err early
    // (e.g. SignInGaia 4xx, no PairFailed event ever fires) we don't
    // sit forever in events.recv().
    let mut pair_task = {
        let c = client.clone();
        tokio::spawn(async move { c.start_gaia_pairing().await })
    };

    loop {
        let event = tokio::select! {
            biased;
            res = &mut pair_task => {
                match res {
                    Ok(Ok(())) => {
                        log::info!("gmessages: gaia pair_task ended OK before PairSuccess event drained");
                        // Biased select! catches pair_task completion before
                        // events.recv() can pull the PairSuccess message off
                        // the channel. Stamp Success status + clear emoji
                        // here so the dialog can transition out of
                        // "Pairing…" and auto-close.
                        crate::gm_qr_state::set_gaia_status(
                            crate::gm_qr_state::GaiaStatus::Success,
                        );
                        crate::gm_qr_state::set_gaia_emoji(None);
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        log::warn!("gmessages: gaia pair_task returned error: {e}");
                        crate::gm_qr_state::set_gaia_emoji(None);
                        crate::gm_qr_state::set_gaia_status(
                            crate::gm_qr_state::GaiaStatus::Failed(format!("{e}")),
                        );
                        anyhow::bail!("gaia pair: {e}");
                    }
                    Err(e) => {
                        log::warn!("gmessages: gaia pair_task panicked: {e}");
                        crate::gm_qr_state::set_gaia_emoji(None);
                        crate::gm_qr_state::set_gaia_status(
                            crate::gm_qr_state::GaiaStatus::Failed(format!(
                                "pair task panicked: {e}"
                            )),
                        );
                        anyhow::bail!("gaia pair task panicked: {e}");
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Some(e) => e,
                    None => {
                        log::warn!("gmessages: events channel closed during gaia pair");
                        anyhow::bail!("gaia pair: events channel closed");
                    }
                }
            }
        };
        match event {
            Event::AvailableGoogleAccounts { accounts } => {
                log::info!(
                    "gmessages: gaia pair: {} accounts available",
                    accounts.len()
                );
                crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::PickingAccount);
                crate::gm_qr_state::set_available_accounts(Some(accounts));
                // Poll for the user's choice.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5 * 60);
                let chosen = loop {
                    if let Some(n) = crate::gm_qr_state::take_chosen_authuser() {
                        break Some(n);
                    }
                    if std::time::Instant::now() > deadline {
                        break None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                };
                crate::gm_qr_state::set_available_accounts(None);
                match chosen {
                    Some(n) => {
                        log::info!("gmessages: gaia pair: user picked authuser={n}");
                        client.choose_google_account(n).await;
                        crate::gm_qr_state::set_gaia_status(
                            crate::gm_qr_state::GaiaStatus::ContactingGoogle,
                        );
                    }
                    None => {
                        log::warn!("gmessages: gaia pair: account choice timeout");
                        // Send 0 so the driver doesn't hang; will likely
                        // fail downstream but better than infinite wait.
                        client.choose_google_account(0).await;
                    }
                }
            }
            Event::PairingEmoji { emoji } => {
                log::info!("gmessages: gaia pair: emoji = {emoji}");
                crate::gm_qr_state::set_gaia_status(
                    crate::gm_qr_state::GaiaStatus::WaitingForEmoji,
                );
                crate::gm_qr_state::set_gaia_emoji(Some(emoji.clone()));

                // Poll for the user's confirmation. Timeout matches the
                // 5-minute window in pairing::gaia.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5 * 60);
                let confirmed = loop {
                    if let Some(ans) = crate::gm_qr_state::take_gaia_confirmation() {
                        break Some(ans);
                    }
                    if std::time::Instant::now() > deadline {
                        break None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                };
                // KEEP the emoji visible — the phone displays it AFTER we
                // send CLIENT_FINISH, so the user needs to keep referring
                // to the desktop emoji while they confirm on their phone.
                // We clear it on PairSuccess / PairFailed below.
                if matches!(confirmed, Some(true)) {
                    crate::gm_qr_state::set_gaia_status(
                        crate::gm_qr_state::GaiaStatus::AwaitingPhone,
                    );
                }
                match confirmed {
                    Some(b) => {
                        log::info!("gmessages: gaia pair: user said matches={b}");
                        client.confirm_pairing_emoji(b).await;
                    }
                    None => {
                        log::warn!("gmessages: gaia pair: user did not confirm in time");
                        client.confirm_pairing_emoji(false).await;
                    }
                }
            }
            Event::PairSuccess => {
                log::info!(
                    "gmessages: gaia paired; auth saved to {}",
                    auth_path.display()
                );
                // Set Success here too, since the escape-hatch path
                // through run_pair_flow skips the outer-loop branch
                // that would otherwise set it. Without this, the dialog
                // sits at the previous status forever and looks "stuck".
                crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Success);
                crate::gm_qr_state::set_gaia_emoji(None);
                break;
            }
            Event::PairFailed { reason } => {
                crate::gm_qr_state::set_gaia_emoji(None);
                crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Failed(
                    reason.clone(),
                ));
                anyhow::bail!("gaia pair failed: {reason}");
            }
            Event::AuthRevoked => {
                crate::gm_qr_state::set_gaia_emoji(None);
                crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Failed(
                    "auth revoked mid-flow".into(),
                ));
                anyhow::bail!("gaia pair: auth revoked mid-flow");
            }
            other => log::debug!("gmessages: gaia pair: ignoring {}", describe_event(&other)),
        }
    }

    // PairSuccess fired — wait for the task to finish cleanly.
    pair_task
        .await
        .context("gmessages: gaia pair task panicked")?
        .context("gmessages: start_gaia_pairing returned error")?;
    Ok(())
}

async fn load_auth(path: &Path) -> Result<AuthData> {
    if !path.exists() {
        return Ok(AuthData::default());
    }
    let bytes = tokio::fs::read(path).await?;
    let auth: AuthData =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(auth)
}

/// Pick the auth path. Prefer `<data_dir>/gmessages-auth.json` if it exists;
/// otherwise fall back to discovering the file in CWD or the workspace root
/// where `cargo run -p gmessages-rust --example pair` typically writes it.
fn resolve_auth_path(data_dir: &Path) -> PathBuf {
    let canonical = data_dir.join(AUTH_FILE);
    if canonical.exists() {
        return canonical;
    }
    // Look in CWD (in case the user launched from there).
    if let Ok(cwd) = std::env::current_dir() {
        let alt = cwd.join(AUTH_FILE);
        if alt.exists() {
            log::info!(
                "gmessages: using auth file from CWD ({}) — moving it to {} would be cleaner",
                alt.display(),
                canonical.display()
            );
            return alt;
        }
    }
    // Look in the parent (workspace root, if running from desktop/).
    if let Ok(cwd) = std::env::current_dir()
        && let Some(parent) = cwd.parent()
    {
        let alt = parent.join(AUTH_FILE);
        if alt.exists() {
            log::info!(
                "gmessages: using auth file from parent dir ({}) — moving it to {} would be cleaner",
                alt.display(),
                canonical.display()
            );
            return alt;
        }
    }
    // None of those — return the canonical path so the error message points there.
    canonical
}

/// Translate one [`Event`] into zero or more [`WaEvent`]s.
fn translate_event(event: Event) -> Vec<WaEvent> {
    match event {
        Event::Ready => {
            log::info!("gmessages: long-poll ready");
            Vec::new()
        }
        Event::Messages { messages, .. } => messages
            .iter()
            .flat_map(|m| {
                let mut out = Vec::with_capacity(2);
                // If this message has a `tmp_id` we recognize, that means
                // the server is confirming an outgoing send we made — fire
                // a MessageConfirmed so the optimistic bubble re-keys to
                // the server-assigned `message_id`.
                //
                // BOTH ids MUST carry the `gm:` prefix. The optimistic bubble
                // was already re-keyed to `gm:<tmp>` by the send-RPC confirm
                // path, and the echo MessageReceived (message_to_incoming) tags
                // its id `gm:<message_id>`. Emitting the raw un-prefixed ids
                // here made this confirm a no-op (lookup miss) and left the echo
                // to append a duplicate bubble.
                if !m.tmp_id.is_empty() {
                    let tag = |id: &str| -> String {
                        if id.starts_with(CHAT_PREFIX) {
                            id.to_string()
                        } else {
                            format!("{CHAT_PREFIX}{id}")
                        }
                    };
                    out.push(WaEvent::MessageConfirmed {
                        tmp_id: tag(&m.tmp_id),
                        real_id: tag(&m.message_id),
                        chat_id: format!("{CHAT_PREFIX}{}", m.conversation_id),
                    });
                }
                if let Some(im) = message_to_incoming(m) {
                    out.push(WaEvent::MessageReceived(Box::new(im)));
                }
                out
            })
            .collect(),
        Event::Typing {
            conversation_id,
            participant_id,
            typing,
        } => vec![WaEvent::TypingIndicator {
            chat_id: format!("{CHAT_PREFIX}{conversation_id}"),
            sender_name: participant_id,
            is_typing: typing,
        }],
        Event::ConversationUpdate { conversation } => {
            log::debug!(
                "gmessages: conversation updated {}",
                conversation.conversation_id
            );
            Vec::new()
        }
        Event::PhoneNotResponding => vec![WaEvent::ErrorToast(
            "Google Messages: phone not responding".into(),
        )],
        Event::PhoneRespondingAgain => Vec::new(),
        Event::AuthRevoked => vec![WaEvent::Disconnected(
            "Google Messages auth revoked — re-pair required".into(),
        )],
        Event::QrCode { url: _ }
        | Event::PairingEmoji { .. }
        | Event::AvailableGoogleAccounts { .. }
        | Event::PairSuccess
        | Event::PairFailed { .. } => Vec::new(),
    }
}

/// Pull pending downloads off a gmessages `Message`. Returns
/// `(media_id, decryption_key, dest_path)` triples. Caller should fetch +
/// decrypt each, then emit `WaEvent::MediaReady` when done.
/// The downloadable `(id, key, is_thumbnail)` for a MediaContent. Prefers
/// the full-size blob; falls back to the thumbnail when no full blob is
/// provided — RCS images frequently arrive thumbnail-only (the full
/// `media_id` stays empty and only `thumbnail_media_id` +
/// `thumbnail_decryption_key` are populated). `is_thumbnail` tells the
/// caller it should also request the full-size image.
fn media_blob_ref(mc: &MediaContent) -> Option<(&str, &[u8], bool)> {
    if !mc.media_id.is_empty() && !mc.decryption_key.is_empty() {
        Some((mc.media_id.as_str(), mc.decryption_key.as_slice(), false))
    } else if !mc.thumbnail_media_id.is_empty() && !mc.thumbnail_decryption_key.is_empty() {
        Some((
            mc.thumbnail_media_id.as_str(),
            mc.thumbnail_decryption_key.as_slice(),
            true,
        ))
    } else {
        None
    }
}

/// One downloadable attachment pulled off an incoming gm `Message`.
struct PendingMedia {
    blob_id: String,
    key: Vec<u8>,
    dest: PathBuf,
    kind: MediaType,
    /// True when only a thumbnail was available — the caller should also
    /// ask the phone for the full-size image.
    is_thumbnail: bool,
    /// `MessageInfo.action_message_id`, needed for the full-size request.
    action_message_id: String,
}

fn pending_downloads(m: &GmMessage, data_dir: &Path) -> Vec<PendingMedia> {
    let media_root = data_dir.join("gm_media");
    let _ = std::fs::create_dir_all(&media_root);
    let mut out = Vec::new();
    for info in &m.message_info {
        if let Some(message_info::Data::MediaContent(mc)) = &info.data
            && let Some((blob_id, blob_key, is_thumbnail)) = media_blob_ref(mc)
        {
            let dest = gm_media_dest(blob_id, &mc.mime_type, data_dir);
            let mime = mc.mime_type.as_str();
            let kind = if mime.starts_with("image/") {
                if mime == "image/gif" {
                    MediaType::Gif
                } else {
                    MediaType::Image
                }
            } else if mime.starts_with("video/") {
                MediaType::Video
            } else if mime.starts_with("audio/") {
                MediaType::Audio
            } else {
                MediaType::Document
            };
            out.push(PendingMedia {
                blob_id: blob_id.to_string(),
                key: blob_key.to_vec(),
                dest,
                kind,
                is_thumbnail,
                action_message_id: info.action_message_id.clone().unwrap_or_default(),
            });
        }
    }
    out
}

/// Best-effort phone+name extraction from a gmessages `Message`. Tries
/// the rich `sender_participant` first (which has full_name + SmallInfo
/// with a real phone number), falls back to `participant_id` (which for
/// SMS is often an internal numeric ID, not a phone).
fn sender_phone_and_name(m: &GmMessage) -> (String, String) {
    if let Some(sp) = &m.sender_participant {
        let phone = sp
            .id
            .as_ref()
            .map(|id| {
                if !id.number.is_empty() {
                    id.number.clone()
                } else {
                    id.participant_id.clone()
                }
            })
            .unwrap_or_default();
        let name = if !sp.full_name.is_empty() {
            sp.full_name.clone()
        } else if !sp.first_name.is_empty() {
            sp.first_name.clone()
        } else {
            phone.clone()
        };
        return (phone, name);
    }
    (m.participant_id.clone(), m.participant_id.clone())
}

fn gm_message_is_from_me(m: &GmMessage) -> bool {
    let status = m.message_status.as_ref().map(|s| s.status).unwrap_or(0);
    (1..=22).contains(&status)
        || m.sender_participant
            .as_ref()
            .is_some_and(|participant| participant.is_me)
}

/// Convert one gmessages `Message` to a desktop `IncomingMessage`.
/// Returns `None` for messages with no displayable content.
fn message_to_incoming(m: &GmMessage) -> Option<IncomingMessage> {
    let mut text: Option<String> = None;
    let mut media: Option<MediaType> = None;
    let mut media_filename: Option<String> = None;
    let mut media_local_path: Option<String> = None;

    for info in &m.message_info {
        match &info.data {
            Some(message_info::Data::MessageContent(c)) if !c.content.is_empty() => {
                text = Some(c.content.clone());
            }
            Some(message_info::Data::MediaContent(mc)) => {
                log::info!(
                    "gm media diag: msg={} type={} fmt={} media_id={:?} key_len={} \
                     mime={:?} name={:?} size={} inline_bytes={} thumb_id={:?} thumb_key_len={}",
                    m.message_id,
                    m.r#type,
                    mc.format,
                    mc.media_id,
                    mc.decryption_key.len(),
                    mc.mime_type,
                    mc.media_name,
                    mc.size,
                    mc.media_data.len(),
                    mc.thumbnail_media_id,
                    mc.thumbnail_decryption_key.len(),
                );
                let mime = mc.mime_type.as_str();
                media = Some(if mime.starts_with("image/") {
                    if mime == "image/gif" {
                        MediaType::Gif
                    } else {
                        MediaType::Image
                    }
                } else if mime.starts_with("video/") {
                    MediaType::Video
                } else if mime.starts_with("audio/") {
                    MediaType::Audio
                } else {
                    MediaType::Document
                });
                if !mc.media_name.is_empty() {
                    media_filename = Some(mc.media_name.clone());
                }
                // If this blob was already downloaded, point at it so the
                // image renders on restart / chat re-open without waiting
                // for a fresh MediaReady event. Uses the same id resolution
                // as the downloader (full blob, else thumbnail).
                if let Some((blob_id, _, _)) = media_blob_ref(mc)
                    && let Some(dir) = GM_DATA_DIR.get()
                {
                    let p = gm_media_dest(blob_id, mime, dir);
                    // HEIC/HEIF are transcoded to a sibling .jpg on download (GTK
                    // can't render HEIC). Prefer that .jpg so an iPhone photo
                    // survives a restart instead of pointing at the unrenderable
                    // .heic and going blank.
                    let jpg = p.with_extension("jpg");
                    if (mime == "image/heic" || mime == "image/heif") && jpg.exists() {
                        media_local_path = Some(jpg.to_string_lossy().into_owned());
                    } else if p.exists() {
                        media_local_path = Some(p.to_string_lossy().into_owned());
                    } else if jpg.exists() {
                        media_local_path = Some(jpg.to_string_lossy().into_owned());
                    }
                }
            }
            _ => {}
        }
    }
    if text.is_none() && media.is_none() {
        return None;
    }

    // Outgoing message statuses are 1-22; incoming start at 100. The
    // participant_id field is unreliable for this — server fills it with
    // the sender's number, which equals OUR number for outbound messages.
    let status = m.message_status.as_ref().map(|s| s.status).unwrap_or(0);
    // Outgoing message statuses are 1-22 (sent, sending, delivered, read,
    // etc); incoming start at 100. RCS messages from the user's phone
    // occasionally arrive with status outside 1-22 (e.g. 0 = uninitialized
    // for a still-sending RCS), so also fall back to checking the rich
    // sender_participant proto — if it's flagged `is_me`, treat as own.
    let sp_is_me = m
        .sender_participant
        .as_ref()
        .map(|sp| sp.is_me)
        .unwrap_or(false);
    let is_from_me = gm_message_is_from_me(m);
    let ts_s = gm_timestamp_to_unix_s(m.timestamp);
    log::debug!(
        "gmessages msg: id={} conv={} raw_ts={} → s={} ({}); status={status} sp_is_me={sp_is_me} from_me={is_from_me}",
        m.message_id,
        m.conversation_id,
        m.timestamp,
        ts_s,
        chrono::DateTime::from_timestamp(ts_s, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "<invalid>".into()),
    );
    let _ = MessageSource::GoogleMessages; // keep import meaningful in match arms below
    // Tag the message ID with `gm:` so the bubble renderer can identify the
    // origin protocol even after the chat_id gets rewritten to a WhatsApp
    // JID by the Phase 2 merge. (See `MessageSource::from_message`.)
    let tagged_id = if m.message_id.starts_with(CHAT_PREFIX) {
        m.message_id.clone()
    } else {
        format!("{CHAT_PREFIX}{}", m.message_id)
    };
    // Resolve sender name: prefer rich `sender_participant` data, then
    // the global contact directory (which holds names from BOTH WhatsApp
    // and gmessages contact lists), and only fall back to the raw
    // `participant_id` if nothing else has a name. Without this, the
    // chat list creates new rows with sender_name = participant_id, which
    // for some carriers/SMS shortcodes is a malformed-looking digit string
    // like `+137340286709870` even when the contact directory has the
    // person saved by name.
    let resolved_sender = {
        let from_sp = m.sender_participant.as_ref().and_then(|sp| {
            if !sp.full_name.is_empty() {
                Some(sp.full_name.clone())
            } else if !sp.first_name.is_empty() {
                Some(sp.first_name.clone())
            } else {
                None
            }
        });
        from_sp
            .or_else(|| {
                // Lookup by sender_participant.id.number first (richest),
                // then by participant_id.
                let candidate_keys = [
                    m.sender_participant
                        .as_ref()
                        .and_then(|sp| sp.id.as_ref())
                        .map(|id| id.number.clone())
                        .filter(|s| !s.is_empty()),
                    Some(m.participant_id.clone()).filter(|s| !s.is_empty()),
                ];
                let global = crate::contacts::global();
                candidate_keys
                    .into_iter()
                    .flatten()
                    .find_map(|k| global.lookup(&k))
            })
            .unwrap_or_else(|| m.participant_id.clone())
    };
    Some(IncomingMessage {
        media_download: None,
        id: tagged_id,
        chat_id: format!("{CHAT_PREFIX}{}", m.conversation_id),
        sender_id: m.participant_id.clone(),
        sender_name: resolved_sender,
        text,
        media_type: media,
        timestamp: ts_s,
        is_from_me,
        quoted_msg_id: None,
        quoted_text: None,
        quoted_sender: None,
        is_forwarded: false,
        forwarding_score: 0,
        reactions: m
            .reactions
            .iter()
            .filter_map(|r| {
                let participants = r.participant_i_ds.first().cloned().unwrap_or_default();
                let unicode = r
                    .data
                    .as_ref()
                    .map(|d| d.unicode.clone())
                    .unwrap_or_default();
                if unicode.is_empty() {
                    None
                } else {
                    Some((participants, unicode))
                }
            })
            .collect(),
        media_local_path,
        media_filename,
        media_caption: None,
        contact_name: None,
        contact_vcard: None,
        link_title: None,
        link_description: None,
        link_url: None,
        link_thumbnail_path: None,
        quoted_media_path: None,
        poll_question: None,
        poll_options: vec![],
        poll_selectable: 0,
        poll_secret: vec![],
        poll_votes: vec![],
        receipt_status: gm_status_to_receipt(
            m.message_status.as_ref().map(|s| s.status).unwrap_or(0),
        ),
        is_edited: false,
        is_system_message: false,
    })
}

/// Convert a gmessages timestamp to **seconds** since epoch.
///
/// The WhatsApp side of the desktop UI uses Unix-seconds for
/// `IncomingMessage.timestamp`; matching it here keeps the date-divider
/// and time-label formatters working correctly.
///
/// gmessages itself uses microseconds (per Go reference's
/// `time.UnixMicro(m.Timestamp)`), but the proto field is `int64` and we've
/// seen older messages occasionally come through in milliseconds, so we
/// detect by magnitude:
///   - <  1e10 → already seconds (or zero, in which case we use now())
///   - < 1e13 → milliseconds → / 1000
///   - >= 1e13 → microseconds → / 1_000_000
fn gm_timestamp_to_unix_s(raw: i64) -> i64 {
    if raw == 0 {
        return SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
    }
    if raw < 10_000_000_000 {
        raw
    } else if raw < 10_000_000_000_000 {
        raw / 1_000
    } else {
        raw / 1_000_000
    }
}

/// Map the gmessages `MessageStatusType` enum to our `ReceiptStatus`.
/// Numbers come from `conversations.proto`'s `MessageStatusType`.
fn gm_status_to_receipt(status: i32) -> ReceiptStatus {
    match status {
        // OUTGOING_*
        1 | 2 => ReceiptStatus::Pending,
        3 => ReceiptStatus::Sent,
        4 => ReceiptStatus::Delivered,
        5 | 6 => ReceiptStatus::Read,
        7..=10 => ReceiptStatus::Failed,
        _ => ReceiptStatus::Pending,
    }
}

/// Look up a contact name. First the per-runtime contact map (the legacy
/// scoped table), then falls back to the cross-protocol global directory
/// which has fuzzy phone-number matching baked in.
fn lookup_contact_name(
    contacts: &std::collections::HashMap<String, String>,
    key: &str,
) -> Option<String> {
    if let Some(n) = contacts.get(key) {
        return Some(n.clone());
    }
    let digits: String = key.chars().filter(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty()
        && let Some(n) = contacts.get(&digits)
    {
        return Some(n.clone());
    }
    if let Some(stripped) = key.strip_prefix('+')
        && let Some(n) = contacts.get(stripped)
    {
        return Some(n.clone());
    }
    if !key.starts_with('+') {
        let with_plus = format!("+{key}");
        if let Some(n) = contacts.get(&with_plus) {
            return Some(n.clone());
        }
    }
    // Last resort: ask the global directory (cross-protocol, fuzzy).
    crate::contacts::global().lookup(key)
}

/// Build a [`ChatSummary`] from a gmessages [`Conversation`]. `contacts`
/// is a phone→name map (empty allowed); used to resolve names when the
/// conversation's participant data is missing.
pub fn conversation_to_summary(
    c: &gmessages_rust::gmproto::conversations::Conversation,
    contacts: &std::collections::HashMap<String, String>,
) -> ChatSummary {
    let me = c.participants.iter().find(|p| p.is_me);
    let other = c
        .participants
        .iter()
        .find(|p| p.is_visible && !p.is_me)
        .or_else(|| c.participants.iter().find(|p| !p.is_me));

    let is_self_name = |candidate: &str| {
        let candidate = candidate.trim();
        !candidate.is_empty()
            && me.is_some_and(|p| {
                [&p.full_name, &p.first_name]
                    .into_iter()
                    .map(|value| value.trim())
                    .any(|value| !value.is_empty() && value.eq_ignore_ascii_case(candidate))
            })
    };

    // Best phone number we can extract for the OTHER participant. Tries:
    //   - the visible non-me Participant's SmallInfo.number
    //   - that Participant's SmallInfo.participant_id (often a phone)
    //   - the first entry in `other_participants` (Conversation field —
    //     populated even when `participants` is empty)
    let phone = other.and_then(participant_phone).or_else(|| {
        c.other_participants
            .iter()
            .find(|value| chat_name_quality(value) == 1)
            .cloned()
    });

    // One-to-one conversation-level/latest-message names can describe the
    // sender of the latest OUTGOING message (the desktop user), not the other
    // party. Trust the non-me participant/contact/phone first, and only use a
    // latest-message name when that latest message is incoming. Conversation
    // names remain authoritative for groups.
    log::debug!(
        "gmessages: resolving name for conv {} (c.name={:?}, participants={}, other_participants={:?}, phone={:?})",
        c.conversation_id,
        c.name,
        c.participants.len(),
        c.other_participants,
        phone,
    );
    let latest_name = c
        .latest_message
        .as_ref()
        .filter(|lm| lm.from_me == 0)
        .map(|lm| lm.display_name.trim().to_string())
        .filter(|name| chat_name_is_named(name) && !is_self_name(name));
    let participant_name = other
        .and_then(|participant| participant_display_name(participant, contacts))
        .filter(|name| !is_self_name(name));
    let phonebook_or_phone = phone.as_ref().map(|phone| {
        lookup_contact_name(contacts, phone)
            .filter(|name| chat_name_is_named(name) && !is_self_name(name))
            .unwrap_or_else(|| phone.clone())
    });

    let shortcode = (!c.is_group_chat)
        .then(|| conversation_shortcode(&c.conversation_id, &c.name))
        .flatten();
    let name = if c.is_group_chat && chat_name_is_named(&c.name) {
        c.name.clone()
    } else if let Some(name) = participant_name {
        name
    } else if let Some(name) = phonebook_or_phone {
        name
    } else if let Some(n) = latest_name {
        log::debug!(
            "gmessages: using latest_message.display_name {n:?} for conv {}",
            c.conversation_id
        );
        n
    } else if let Some(shortcode) = shortcode {
        shortcode.to_string()
    } else {
        log::warn!(
            "gmessages: no name source for conv {} (c.name={:?}, participants={}, other_participants={:?})",
            c.conversation_id,
            c.name,
            c.participants.len(),
            c.other_participants,
        );
        c.conversation_id.clone()
    };
    let last = c
        .latest_message
        .as_ref()
        .map(|lm| lm.display_content.clone())
        .unwrap_or_default();
    let timestamp = gm_timestamp_to_unix_s(c.last_message_timestamp);
    ChatSummary {
        id: format!("{CHAT_PREFIX}{}", c.conversation_id),
        name,
        last_message: last,
        timestamp,
        unread_count: if c.unread { 1 } else { 0 },
        is_group: c.is_group_chat,
        is_muted: false,
        is_pinned: c.pinned,
        is_archived: c.status == 2,
        is_favorite: false,
        label: None,
        pinned_msg_id: None,
        auto_mark_read: false,
    }
}

// Removed: spawn_firefox_to_refresh / wait_for_cookie_rotation. We now
// self-rotate via Set-Cookie capture (http.rs + longpoll.rs), so we
// never need to nudge FF at runtime. FF is only required for the
// initial pair (to get the cookies in the first place).

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, name: &str) -> ChatSummary {
        ChatSummary {
            id: id.into(),
            name: name.into(),
            last_message: "newest preview".into(),
            timestamp: 123,
            unread_count: 4,
            is_group: false,
            is_muted: false,
            is_pinned: false,
            is_archived: false,
            is_favorite: false,
            label: None,
            pinned_msg_id: None,
            auto_mark_read: false,
        }
    }

    #[test]
    fn conversation_metadata_resolves_numeric_name_without_regressing_row() {
        let existing = summary("gm:25", "25");
        let mut fresh = summary("gm:25", "Brandon Longstaff");
        fresh.last_message = "older server preview".into();
        fresh.timestamp = 100;
        fresh.unread_count = 0;
        fresh.is_pinned = true;
        let mut cache = vec![existing];

        let merged = merge_gm_conversation_metadata(&mut cache, &fresh);

        assert_eq!(merged.name, "Brandon Longstaff");
        assert_eq!(merged.last_message, "newest preview");
        assert_eq!(merged.timestamp, 123);
        assert_eq!(merged.unread_count, 4);
        assert!(merged.is_pinned);
    }

    #[test]
    fn conversation_metadata_does_not_replace_real_name_with_numeric_placeholder() {
        let existing = summary("gm:25", "Brandon Longstaff");
        let fresh = summary("gm:25", "25");
        let mut cache = vec![existing];

        let merged = merge_gm_conversation_metadata(&mut cache, &fresh);

        assert_eq!(merged.name, "Brandon Longstaff");
    }

    #[test]
    fn identity_quality_rejects_internal_ids_but_accepts_full_phone_numbers() {
        assert_eq!(chat_name_quality("25"), 0);
        assert_eq!(chat_name_quality("gm:6689"), 0);
        assert_eq!(chat_name_quality("6912"), 0);
        assert_eq!(chat_name_quality("79749019898100@lid"), 0);
        assert_eq!(chat_name_quality("+1 (416) 555-0123"), 1);
        assert_eq!(chat_name_quality("Brandon Longstaff"), 2);
    }

    #[test]
    fn conversation_summary_keeps_real_shortcode_but_rejects_own_internal_id() {
        use gmessages_rust::gmproto::conversations::Conversation;

        let shortcode = Conversation {
            conversation_id: "6650".into(),
            name: "87225".into(),
            ..Default::default()
        };
        assert_eq!(
            conversation_to_summary(&shortcode, &Default::default()).name,
            "87225"
        );

        let internal_id = Conversation {
            conversation_id: "25".into(),
            name: "25".into(),
            ..Default::default()
        };
        assert_eq!(
            conversation_to_summary(&internal_id, &Default::default()).name,
            "25"
        );
        assert_eq!(conversation_shortcode("25", "25"), None);
    }

    #[test]
    fn participant_protocol_id_never_replaces_a_real_phone_number() {
        use gmessages_rust::gmproto::conversations::Participant;

        let participant = Participant {
            full_name: "79749019898100@lid".into(),
            formatted_number: "+15145190335".into(),
            ..Default::default()
        };

        assert_eq!(
            participant_display_name(&participant, &Default::default()),
            Some("+15145190335".into())
        );
    }

    #[test]
    fn conversation_summary_prefers_participant_name_over_formatted_phone_placeholder() {
        use gmessages_rust::gmproto::conversations::{Conversation, Participant, SmallInfo};

        let conversation = Conversation {
            conversation_id: "25".into(),
            name: "+1 (416) 555-0123".into(),
            participants: vec![Participant {
                id: Some(SmallInfo {
                    number: "+14165550123".into(),
                    ..Default::default()
                }),
                full_name: "Brandon Longstaff".into(),
                is_visible: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let got = conversation_to_summary(&conversation, &Default::default());

        assert_eq!(got.name, "Brandon Longstaff");
    }

    #[test]
    fn direct_outgoing_conversation_never_uses_own_name() {
        use gmessages_rust::gmproto::conversations::{
            Conversation, LatestMessage, Participant, SmallInfo,
        };

        let conversation = Conversation {
            conversation_id: "6689".into(),
            // Google can put the latest outgoing sender here.
            name: "Jake".into(),
            latest_message: Some(LatestMessage {
                from_me: 1,
                display_name: "Jake Steinman".into(),
                ..Default::default()
            }),
            participants: vec![
                Participant {
                    full_name: "Jake Steinman".into(),
                    first_name: "Jake".into(),
                    is_me: true,
                    is_visible: true,
                    ..Default::default()
                },
                Participant {
                    id: Some(SmallInfo {
                        number: "+14165550123".into(),
                        ..Default::default()
                    }),
                    is_visible: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let got = conversation_to_summary(&conversation, &Default::default());

        assert_eq!(got.name, "+14165550123");
    }

    #[test]
    fn history_sender_marked_as_me_is_always_outgoing_even_with_unknown_status() {
        use gmessages_rust::gmproto::conversations::{Message, Participant};

        let message = Message {
            sender_participant: Some(Participant {
                full_name: "Jake Steinman".into(),
                is_me: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(gm_message_is_from_me(&message));
        assert_eq!(
            participant_display_name(
                message.sender_participant.as_ref().unwrap(),
                &Default::default()
            ),
            None
        );
    }
}
