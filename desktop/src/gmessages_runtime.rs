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
use gmessages_rust::gmproto::conversations::{Message as GmMessage, message_info};
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
pub fn spawn(data_dir: &Path, event_tx: Sender<WaEvent>) -> Option<TokioUnboundedSender<WaCommand>> {
    if std::env::var("GMESSAGES_ENABLE").as_deref() != Ok("1") {
        log::info!("gmessages: GMESSAGES_ENABLE not set; skipping (set GMESSAGES_ENABLE=1 to enable)");
        return None;
    }
    let data_dir = data_dir.to_path_buf();
    log::info!("gmessages: ENABLED — spawning runtime; data_dir={}", data_dir.display());
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run(data_dir, event_tx.clone(), cmd_rx).await {
            log::error!("gmessages runtime error: {e:#}");
            let _ = event_tx.send(WaEvent::ErrorToast(format!("gmessages: {e}"))).await;
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
fn redirect_chat_id(
    event: &mut WaEvent,
    merge_map: &std::collections::HashMap<String, String>,
) {
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
        WaEvent::MessageConfirmed { chat_id, .. }
        | WaEvent::MessageFailed { chat_id, .. } => map_id(chat_id),
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

async fn run(
    data_dir: PathBuf,
    event_tx: Sender<WaEvent>,
    mut cmd_rx: TokioUnboundedReceiver<WaCommand>,
) -> Result<()> {
    let auth_path = resolve_auth_path(&data_dir);
    log::info!("gmessages: looking for auth file at {}", auth_path.display());
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
    let contact_cache: ContactCache = std::sync::Arc::new(tokio::sync::Mutex::new(
        std::collections::HashMap::new(),
    ));

    // Per-message dedup ring, used to suppress server retransmissions of
    // batches we've already forwarded.
    let mut recent_msgs = RecentMsgRing::default();

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
    if let Ok(bytes) = std::fs::read(&gm_chats_cache_path)
        && let Ok(cached) = bincode::deserialize::<Vec<ChatSummary>>(&bytes)
    {
        log::info!("gmessages: hydrating {} chats from cache", cached.len());
        for summary in cached {
            let _ = event_tx.send(WaEvent::ChatAdded(summary)).await;
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
            let is_gaia = client
                .auth_snapshot()
                .await
                .gaia_authuser
                .is_some();
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
                client.connect().await.context("gmessages: connect after re-pair")?;
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
            let mut insert = |key: &str, name: &str, map: &mut std::collections::HashMap<String, String>| {
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
                            insert_fn: &mut dyn FnMut(&str, &str, &mut std::collections::HashMap<String, String>)| {
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
            let mut feed_to_global = |contacts: Vec<gmessages_rust::gmproto::conversations::Contact>,
                                      map: &mut std::collections::HashMap<String, String>,
                                      insert_fn: &mut dyn FnMut(&str, &str, &mut std::collections::HashMap<String, String>)| {
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
                    log::info!("gmessages: ListContacts returned {} contacts", resp.contacts.len());
                    if log::log_enabled!(log::Level::Debug) {
                        for c in resp.contacts.iter().take(3) {
                            log::debug!(
                                "gmessages: sample contact: name={:?} pid={:?} num={:?}",
                                c.name,
                                c.participant_id,
                                c.number.as_ref().map(|n| (n.number.as_str(), n.number2.as_str())),
                            );
                        }
                    }
                    feed_to_global(resp.contacts, &mut contact_map, &mut insert);
                }
                Err(e) => log::warn!("gmessages: list_contacts failed: {e}"),
            }
            match client.list_top_contacts(50).await {
                Ok(resp) => {
                    log::info!("gmessages: ListTopContacts returned {} contacts", resp.contacts.len());
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
            // Fetch a generous window. `list_conversations(50)` used to be the
            // limit and silently dropped every gm chat past the top 50 from
            // the cache, so anything you hadn't messaged recently
            // disappeared on every restart. 1000 covers any plausible
            // SMS history.
            match client.list_conversations(1000).await {
                Ok(resp) => {
                    log::info!("gmessages: got {} conversations", resp.conversations.len());
                    let summaries: Vec<ChatSummary> = resp
                        .conversations
                        .iter()
                        .map(|c| conversation_to_summary(c, &contact_map))
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
                            let phone = conv
                                .participants
                                .iter()
                                .find(|p| p.is_visible && !p.is_me)
                                .and_then(|p| p.id.as_ref())
                                .filter(|id| !id.number.is_empty())
                                .map(|id| id.number.clone())
                                .or_else(|| conv.other_participants.first().cloned());
                            // Record the gm chat_id in the global directory.
                            if let Some(p) = &phone {
                                global.record_chat_id(p, "gmessages", &summary.id);
                            }
                            let unified = unified_chat_id(&summary.id, phone.as_deref(), &phone_to_wa_chat);
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
                    if let Ok(old_bytes) = std::fs::read(&gm_chats_cache_path)
                        && let Ok(old_cached) =
                            bincode::deserialize::<Vec<ChatSummary>>(&old_bytes)
                    {
                        for s in old_cached {
                            if !mm_snap.contains_key(strip_prefix(&s.id)) {
                                merged_cache.insert(s.id.clone(), s);
                            }
                        }
                    }
                    // Overlay the fresh response (fresh wins).
                    for s in &summaries {
                        if mm_snap.contains_key(strip_prefix(&s.id)) {
                            // Merged into a WA row — drop it from the
                            // gm cache so it doesn't get hydrated again.
                            merged_cache.remove(&s.id);
                            continue;
                        }
                        merged_cache.insert(s.id.clone(), s.clone());
                    }
                    let mut to_persist: Vec<ChatSummary> =
                        merged_cache.into_values().collect();
                    // Sort by timestamp desc so the file is stable + easy
                    // to inspect.
                    to_persist
                        .sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                    log::info!(
                        "gmessages: persisting {} chats to cache (fresh: {}, merge_map: {})",
                        to_persist.len(),
                        summaries.len(),
                        mm_snap.len(),
                    );
                    if let Ok(bytes) = bincode::serialize(&to_persist) {
                        let _ = std::fs::write(&gm_chats_cache_path, &bytes);
                    }

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
                            .send(WaEvent::ChatDeleted {
                                chat_id: stale_id,
                            })
                            .await;
                    }
                    for summary in &summaries {
                        let conv_id = strip_prefix(&summary.id);
                        if mm_snap.contains_key(conv_id) {
                            // Don't add a duplicate row — the existing
                            // WhatsApp row will absorb this conversation's
                            // messages via the merge_map redirect.
                            continue;
                        }
                        log::debug!(
                            "gmessages → desktop: ChatAdded({} \"{}\")",
                            summary.id,
                            summary.name
                        );
                        if event_tx.send(WaEvent::ChatAdded(summary.clone())).await.is_err() {
                            return;
                        }
                        // Also push a ChatNameUpdated so any cached row (from a
                        // previous session) refreshes its title now that
                        // contacts are loaded.
                        let _ = event_tx
                            .send(WaEvent::ChatNameUpdated {
                                chat_id: summary.id.clone(),
                                name: summary.name.clone(),
                            })
                            .await;
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
                            let n = s.name.trim();
                            n.is_empty()
                                || n.starts_with('+')
                                || n.chars().all(|c| !c.is_alphabetic())
                        })
                        .map(|(s, _)| s.id.clone())
                        .collect();
                    log::info!(
                        "gmessages: eagerly enriching {} chats with unresolved names",
                        unresolved.len()
                    );
                    for chat_id in unresolved {
                        let client = client.clone();
                        let event_tx = event_tx.clone();
                        let conv_id = strip_prefix(&chat_id).to_string();
                        tokio::spawn(async move {
                            if let Ok(resp) = client.fetch_messages(&conv_id, 5).await {
                                for m in resp.messages {
                                    if let Some(sp) = &m.sender_participant {
                                        // Skip outgoing — `is_me` is set
                                        // when this participant is the
                                        // desktop user.
                                        if sp.is_me {
                                            continue;
                                        }
                                        let name = if !sp.full_name.is_empty() {
                                            Some(sp.full_name.clone())
                                        } else if !sp.first_name.is_empty() {
                                            Some(sp.first_name.clone())
                                        } else {
                                            None
                                        };
                                        if let Some(name) = name {
                                            let _ = event_tx
                                                .send(WaEvent::ChatNameUpdated {
                                                    chat_id,
                                                    name,
                                                })
                                                .await;
                                            return;
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => log::warn!("gmessages: list_conversations failed: {e}"),
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
                    crate::gm_qr_state::set_gaia_status(
                        crate::gm_qr_state::GaiaStatus::Finalizing,
                    );
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
                    crate::gm_qr_state::set_gaia_status(
                        crate::gm_qr_state::GaiaStatus::Failed(format!("{e}")),
                    );
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

                // For Messages events, also: (a) opportunistically learn the
                // sender's name via `sender_participant` data and emit a
                // `ChatNameUpdated` so chat rows that started life with an
                // internal numeric ID (like "16") can be retitled the moment
                // we see a message from them, and (b) kick off media
                // downloads in parallel.
                if let Event::Messages { messages, .. } = &event {
                    let cache = contact_cache.lock().await.clone();
                    let global = crate::contacts::global();
                    let mm = merge_map.lock().await.clone();
                    let _ = &mm; // used below in translate_event call after enrichment
                    for m in messages {
                        if let Some(sp) = &m.sender_participant {
                            // Skip OUTGOING messages: the proto's
                            // `Participant.is_me` flag is the canonical
                            // "this participant is the desktop user"
                            // signal. Status-based detection misses
                            // status=0 (Unknown) cases — using is_me is
                            // reliable for every code path.
                            if sp.is_me {
                                continue;
                            }
                            // Get the best name we can from the participant.
                            let name = if !sp.full_name.is_empty() {
                                Some(sp.full_name.clone())
                            } else if !sp.first_name.is_empty() {
                                Some(sp.first_name.clone())
                            } else {
                                sp.id.as_ref().and_then(|id| {
                                    if id.number.is_empty() {
                                        None
                                    } else {
                                        lookup_contact_name(&cache, &id.number)
                                            .or_else(|| Some(id.number.clone()))
                                    }
                                })
                            };
                            // Feed the global directory so other protocols
                            // can use this name later.
                            if let Some(name) = &name
                                && let Some(id) = &sp.id
                                && !id.number.is_empty()
                                && name.chars().any(|c| c.is_alphabetic())
                            {
                                global.insert(&id.number, name, "gmessages-msg");
                            }
                            if let Some(name) = name {
                                let chat_id = format!("{CHAT_PREFIX}{}", m.conversation_id);
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
                    for m in messages {
                        let downloads = pending_downloads(m, &data_dir);
                        for (media_id, key, dest, kind) in downloads {
                            let chat_id = format!("{CHAT_PREFIX}{}", m.conversation_id);
                            let msg_id = m.message_id.clone();
                            let client = client.clone();
                            let event_tx = event_tx.clone();
                            tokio::spawn(async move {
                                if dest.exists() {
                                    log::debug!(
                                        "gmessages: media already downloaded at {}",
                                        dest.display()
                                    );
                                    let _ = event_tx
                                        .send(WaEvent::MediaReady {
                                            msg_id,
                                            chat_id,
                                            path: dest.to_string_lossy().into_owned(),
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
                                        if let Err(e) = std::fs::write(&dest, &bytes) {
                                            log::warn!("gmessages: write media file: {e}");
                                            return;
                                        }
                                        let _ = event_tx
                                            .send(WaEvent::MediaReady {
                                                msg_id,
                                                chat_id,
                                                path: dest.to_string_lossy().into_owned(),
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
                        im.chat_id = VERIFICATION_CODES_CHAT_ID.into();
                    }
                    // STEP 1: persist using the (post-2FA-routing) chat_id.
                    if let WaEvent::MessageReceived(im) = &wa_event {
                        crate::ui::runtime::save_messages_append(&im.chat_id, im);
                    }
                    // STEP 2: rewrite chat_id for UI routing if merged.
                    redirect_chat_id(&mut wa_event, &mm);
                    // STEP 3: if the redirect targeted a WhatsApp chat row,
                    // update wa_chats.bin so the preview/timestamp survive
                    // a restart. Without this, on next launch the chat list
                    // shows the LAST WhatsApp message as the preview even
                    // though an SMS was the most recent thing — and the
                    // chat doesn't bump to the top.
                    if let WaEvent::MessageReceived(im) = &wa_event
                        && (im.chat_id.ends_with("@s.whatsapp.net") || im.chat_id.ends_with("@lid"))
                    {
                        let preview = im
                            .text
                            .clone()
                            .or_else(|| im.media_caption.clone())
                            .unwrap_or_else(|| match im.media_type {
                                Some(crate::bridge::MediaType::Image) => "📷 Photo".into(),
                                Some(crate::bridge::MediaType::Video) => "🎥 Video".into(),
                                Some(crate::bridge::MediaType::Audio) => "🎵 Audio".into(),
                                Some(crate::bridge::MediaType::Document) => "📄 Document".into(),
                                Some(crate::bridge::MediaType::Sticker) => "🎭 Sticker".into(),
                                Some(crate::bridge::MediaType::Gif) => "🎞 GIF".into(),
                                None => String::new(),
                            });
                        crate::ui::runtime::touch_wa_chat_preview(
                            &im.chat_id,
                            &preview,
                            im.timestamp,
                            im.is_from_me,
                        );
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
                tokio::spawn(async move {
                    if let Err(e) = handle_command(&client, &event_tx, cmd).await {
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

/// Handle a `WaCommand` whose `chat_id` belongs to a gmessages chat.
async fn handle_command(client: &Arc<Client>, event_tx: &Sender<WaEvent>, cmd: WaCommand) -> Result<()> {
    use crate::bridge::IncomingMessage;
    match cmd {
        WaCommand::SendText { chat_id, text, tmp_id, .. } => {
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
                    let _ = event_tx.send(WaEvent::MessageReceived(echo)).await;
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
        WaCommand::LoadChat { chat_id, chat_name } => {
            let conv = strip_prefix(&chat_id);
            log::info!("gmessages: LoadChat for {conv}");

            // Start from on-disk cache so locally-known history (including
            // anything we sent since the last server fetch) shows up.
            let mut merged: Vec<IncomingMessage> =
                crate::ui::runtime::load_messages(&chat_id);
            log::debug!("gmessages: LoadChat starting with {} cached messages", merged.len());
            let mut have: std::collections::HashSet<String> =
                merged.iter().map(|m| m.id.clone()).collect();

            match client.fetch_messages(conv, 100).await {
                Ok(resp) => {
                    // Mine sender_participant for live name updates —
                    // but ONLY from incoming messages. The user's own
                    // sender_participant on outgoing messages would
                    // rename the chat to the user's own name otherwise.
                    for m in &resp.messages {
                        let status = m
                            .message_status
                            .as_ref()
                            .map(|s| s.status)
                            .unwrap_or(0);
                        let is_from_me = (1..=22).contains(&status);
                        if is_from_me {
                            continue;
                        }
                        if let Some(sp) = &m.sender_participant {
                            let name = if !sp.full_name.is_empty() {
                                Some(sp.full_name.clone())
                            } else if !sp.first_name.is_empty() {
                                Some(sp.first_name.clone())
                            } else {
                                None
                            };
                            if let Some(name) = name {
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
                    for m in resp.messages.iter().rev() {
                        if have.contains(&m.message_id) {
                            continue;
                        }
                        if let Some(im) = message_to_incoming(m) {
                            have.insert(im.id.clone());
                            merged.push(im);
                        }
                    }
                    // Persist the merged set so future restarts see the same
                    // ordering.
                    crate::ui::runtime::save_messages(&chat_id, &merged);
                }
                Err(e) => log::warn!("gmessages: fetch_messages failed (using disk cache only): {e}"),
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
            let conv = strip_prefix(&chat_id);
            // mark_read needs the message_id; without it we can't dispatch
            // a useful API call. The UI invokes this on chat-open with no
            // specific message — for gmessages we'd need the latest message
            // id, which we don't track yet. Skip for now.
            log::debug!("gmessages: MarkRead for {conv} — not yet implemented");
        }
        WaCommand::SetTyping { chat_id, is_typing } => {
            let conv = strip_prefix(&chat_id);
            if let Err(e) = client.set_typing(conv, is_typing).await {
                log::warn!("gmessages: set_typing failed: {e}");
            }
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
            if im.is_from_me { "<self>" } else { im.sender_id.as_str() },
            im.text.as_deref().map(|s| if s.len() > 40 { &s[..40] } else { s }),
        ),
        WaEvent::TypingIndicator { chat_id, is_typing, .. } => {
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
        .arg("-u").arg("critical")
        .arg("-t").arg("0") // never auto-dismiss
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
            crate::gm_qr_state::set_gaia_status(
                crate::gm_qr_state::GaiaStatus::Starting,
            );
            return run_gaia_pair_flow(client, auth_path, events, event_tx).await;
        }
        let event = match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            events.recv(),
        )
        .await
        {
            Ok(Some(e)) => e,
            Ok(None) => break, // channel closed
            Err(_) => continue, // timer tick — re-check escape hatch
        };
        match event {
            Event::QrCode { url } => {
                // Publish to the in-app settings page.
                crate::gm_qr_state::set(Some(url.clone()));
                // Also render in the terminal for users not in settings.
                eprintln!("\n=== Google Messages pairing — scan with phone (or open Settings) ===\n");
                let code = QrCode::new(url.as_bytes())
                    .context("gmessages: failed to encode QR")?;
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
            other => log::debug!("gmessages: ignoring event during pair: {}", describe_event(&other)),
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
                crate::gm_qr_state::set_gaia_status(
                    crate::gm_qr_state::GaiaStatus::PickingAccount,
                );
                crate::gm_qr_state::set_available_accounts(Some(accounts));
                // Poll for the user's choice.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(5 * 60);
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
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(5 * 60);
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
                crate::gm_qr_state::set_gaia_status(
                    crate::gm_qr_state::GaiaStatus::Success,
                );
                crate::gm_qr_state::set_gaia_emoji(None);
                break;
            }
            Event::PairFailed { reason } => {
                crate::gm_qr_state::set_gaia_emoji(None);
                crate::gm_qr_state::set_gaia_status(
                    crate::gm_qr_state::GaiaStatus::Failed(reason.clone()),
                );
                anyhow::bail!("gaia pair failed: {reason}");
            }
            Event::AuthRevoked => {
                crate::gm_qr_state::set_gaia_emoji(None);
                crate::gm_qr_state::set_gaia_status(
                    crate::gm_qr_state::GaiaStatus::Failed(
                        "auth revoked mid-flow".into(),
                    ),
                );
                anyhow::bail!("gaia pair: auth revoked mid-flow");
            }
            other => log::debug!(
                "gmessages: gaia pair: ignoring {}",
                describe_event(&other)
            ),
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
    let auth: AuthData = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))?;
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
                if !m.tmp_id.is_empty() {
                    out.push(WaEvent::MessageConfirmed {
                        tmp_id: m.tmp_id.clone(),
                        real_id: m.message_id.clone(),
                        chat_id: format!("{CHAT_PREFIX}{}", m.conversation_id),
                    });
                }
                if let Some(im) = message_to_incoming(m) {
                    out.push(WaEvent::MessageReceived(im));
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
        Event::ConversationUpdate { conversation_id } => {
            log::debug!("gmessages: conversation updated {conversation_id}");
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
fn pending_downloads(m: &GmMessage, data_dir: &Path) -> Vec<(String, Vec<u8>, PathBuf, MediaType)> {
    let media_root = data_dir.join("gm_media");
    let _ = std::fs::create_dir_all(&media_root);
    let mut out = Vec::new();
    for info in &m.message_info {
        if let Some(message_info::Data::MediaContent(mc)) = &info.data
            && !mc.media_id.is_empty()
            && !mc.decryption_key.is_empty()
        {
            let ext = match mc.mime_type.split_once('/').map(|(_, sub)| sub) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => "bin".into(),
            };
            let filename = format!("{}.{}", mc.media_id, ext);
            let dest = media_root.join(filename);
            let mime = mc.mime_type.as_str();
            let kind = if mime.starts_with("image/") {
                if mime == "image/gif" { MediaType::Gif } else { MediaType::Image }
            } else if mime.starts_with("video/") {
                MediaType::Video
            } else if mime.starts_with("audio/") {
                MediaType::Audio
            } else {
                MediaType::Document
            };
            out.push((mc.media_id.clone(), mc.decryption_key.clone(), dest, kind));
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

/// Convert one gmessages `Message` to a desktop `IncomingMessage`.
/// Returns `None` for messages with no displayable content.
fn message_to_incoming(m: &GmMessage) -> Option<IncomingMessage> {
    let mut text: Option<String> = None;
    let mut media: Option<MediaType> = None;
    let mut media_filename: Option<String> = None;

    for info in &m.message_info {
        match &info.data {
            Some(message_info::Data::MessageContent(c)) if !c.content.is_empty() => {
                text = Some(c.content.clone());
            }
            Some(message_info::Data::MediaContent(mc)) => {
                let mime = mc.mime_type.as_str();
                media = Some(if mime.starts_with("image/") {
                    if mime == "image/gif" { MediaType::Gif } else { MediaType::Image }
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
    let is_from_me = (1..=22).contains(&status) || sp_is_me;
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
                let participants = r
                    .participant_i_ds
                    .first()
                    .cloned()
                    .unwrap_or_default();
                let unicode = r.data.as_ref().map(|d| d.unicode.clone()).unwrap_or_default();
                if unicode.is_empty() { None } else { Some((participants, unicode)) }
            })
            .collect(),
        media_local_path: None,
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
        receipt_status: gm_status_to_receipt(m.message_status.as_ref().map(|s| s.status).unwrap_or(0)),
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
    let other = c
        .participants
        .iter()
        .find(|p| p.is_visible && !p.is_me)
        .cloned();

    // Best phone number we can extract for the OTHER participant. Tries:
    //   - the visible non-me Participant's SmallInfo.number
    //   - that Participant's SmallInfo.participant_id (often a phone)
    //   - the first entry in `other_participants` (Conversation field —
    //     populated even when `participants` is empty)
    let phone = other
        .as_ref()
        .and_then(|p| p.id.as_ref())
        .map(|id| {
            if !id.number.is_empty() {
                id.number.clone()
            } else {
                id.participant_id.clone()
            }
        })
        .or_else(|| c.other_participants.first().cloned());

    // Name resolution priority:
    //   1. Conversation-level name with letters (set for group chats)
    //   2. Participant.full_name (saved contact pulled from phone)
    //   3. Participant.first_name
    //   4. Contact map lookup by phone (handles SMS where participants
    //      entry has no name but contacts list does)
    //   5. Phone number itself
    //   6. conversation_id as a last resort
    log::debug!(
        "gmessages: resolving name for conv {} (c.name={:?}, participants={}, other_participants={:?}, phone={:?})",
        c.conversation_id,
        c.name,
        c.participants.len(),
        c.other_participants,
        phone,
    );
    // `latest_message.display_name` is what gmessages itself shows in the
    // chat header — server-side resolved name including saved-contact
    // lookups. Use it as a high-priority fallback BEFORE we go to the
    // raw participant data.
    let latest_name = c
        .latest_message
        .as_ref()
        .map(|lm| lm.display_name.trim().to_string())
        .filter(|n| !n.is_empty() && n.chars().any(|ch| !ch.is_ascii_digit() && ch != '+' && ch != ' ' && ch != '(' && ch != ')' && ch != '-'));

    let name = if !c.name.is_empty() && c.name.chars().any(|ch| !ch.is_ascii_digit()) {
        c.name.clone()
    } else if let Some(p) = other.as_ref()
        && !p.full_name.is_empty()
    {
        p.full_name.clone()
    } else if let Some(p) = other.as_ref()
        && !p.first_name.is_empty()
    {
        p.first_name.clone()
    } else if let Some(n) = latest_name {
        log::debug!("gmessages: using latest_message.display_name {n:?} for conv {}", c.conversation_id);
        n
    } else if let Some(p) = phone.as_ref()
        && let Some(name) = lookup_contact_name(contacts, p)
    {
        log::debug!("gmessages: matched contact for {p} → {name}");
        name
    } else if let Some(p) = phone {
        log::debug!(
            "gmessages: name fallback to phone {p} for conv {} (no contact match)",
            c.conversation_id
        );
        p
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
