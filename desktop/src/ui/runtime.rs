//! Tokio-side runtime: connects to WhatsApp, processes events, handles commands.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_channel::Sender;
use base64::Engine as _;
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
/// Per-chat read watermark: chat_id → timestamp of the latest message the user
/// had seen when they last read the chat. Stored in its own file (a plain
/// `HashMap<String,i64>`) so it survives reboot WITHOUT touching the
/// `ChatSummary` bincode schema. Used by `upsert_chat` to stop a stale
/// reconnect reseed (history sync / list_conversations) from resurrecting the
/// unread badge of a chat the user already read. Missing file → empty map.
const READ_WM_FILE: &str = "wa_read_watermarks.bin";
/// Set of phone digit-strings already covered by the phone→LID prewarm sweep.
/// The sweep is capped per launch (the ContactInfoSpec usync times out under
/// large loads), so this lets each launch resume where the last stopped instead
/// of re-usyncing the same head of the phonebook forever. See
/// [`prewarm_contact_lids`]. Persisted as a bincode `HashSet<String>`.
const LID_SWEEP_DONE_FILE: &str = "wa_lid_sweep_done.bin";

// Magic header for versioned binary files: "WA01"
// Bumped from '01' to '02' after adding is_edited + is_system_message to IncomingMessage.
// Old bincode files with header '01' will fail to deserialize and be re-created.
const BIN_HEADER: [u8; 4] = [0x57, 0x41, b'0', b'2'];

// Legacy JSON filenames for auto-migration
const CHATS_FILE_JSON: &str = "wa_chats.json";
const CONTACTS_FILE_JSON: &str = "wa_contacts.json";
const LID_PHONE_FILE_JSON: &str = "wa_lid_phone.json";

const TENOR_RESULT_LIMIT: usize = 12;
const TENOR_SEARCH_RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
const TENOR_PAGE_RESPONSE_LIMIT: usize = 1024 * 1024;
const TENOR_GIF_SEND_MAX_BYTES: usize = 25 * 1024 * 1024;
const TENOR_STICKER_SEND_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Load an explicitly configured Tenor API key from env or a local file.
fn tenor_api_key() -> Option<String> {
    std::env::var("TENOR_API_KEY")
        .or_else(|_| std::fs::read_to_string("tenor_key.txt").map(|s| s.trim().to_string()))
        .or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::fs::read_to_string(
                std::path::Path::new(&home).join(".config/whatsapp-desktop/tenor_key"),
            )
            .map(|s| s.trim().to_string())
        })
        .ok()
        .filter(|key| !key.is_empty())
}

#[derive(Clone, Debug)]
struct TenorCredentials {
    api_key: String,
    client_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TenorSearchKind {
    Gif,
    Sticker,
}

impl TenorSearchKind {
    fn page_suffix(self) -> &'static str {
        match self {
            Self::Gif => "gifs",
            Self::Sticker => "stickers",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::Sticker => "sticker",
        }
    }
}

static TENOR_WEB_CREDENTIALS: std::sync::OnceLock<std::result::Result<TenorCredentials, String>> =
    std::sync::OnceLock::new();
static TENOR_SEARCH_SEMAPHORE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
static LATEST_GIF_SEARCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LATEST_STICKER_SEARCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn extract_script_contents<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let id_marker = format!("id=\"{id}\"");
    let id_pos = html.find(&id_marker)?;
    let script_start = html[..id_pos].rfind("<script")?;
    let body_start = script_start + html[script_start..].find('>')? + 1;
    let body_end = body_start + html[body_start..].find("</script>")?;
    Some(html[body_start..body_end].trim())
}

fn fetch_bounded_text(url: &str, limit: usize) -> Result<String> {
    use std::io::Read as _;

    let response = ureq::get(url)
        .set("User-Agent", "whatsapp-desktop/0.1 (Tenor picker)")
        .timeout(std::time::Duration::from_secs(12))
        .call()
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    let mut bytes = Vec::with_capacity(limit.min(256 * 1024));
    response
        .into_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        anyhow::bail!("response exceeded {limit} bytes");
    }
    String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("response was not UTF-8: {e}"))
}

fn fetch_bounded_bytes(url: &str, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read as _;

    if !url.starts_with("https://") {
        anyhow::bail!("refusing a non-HTTPS media URL");
    }
    let response = ureq::get(url)
        .set("User-Agent", "whatsapp-desktop/0.1 (Tenor picker)")
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if response
        .header("Content-Length")
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > limit)
    {
        anyhow::bail!("media exceeded the {limit}-byte download limit");
    }
    let mut bytes = Vec::with_capacity(limit.min(1024 * 1024));
    response
        .into_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        anyhow::bail!("media exceeded the {limit}-byte download limit");
    }
    Ok(bytes)
}

fn validate_mp4(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        anyhow::bail!("downloaded GIF payload was not an MP4 file");
    }
    Ok(())
}

fn validate_webp(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        anyhow::bail!("downloaded sticker payload was not a WebP file");
    }
    Ok(())
}

static TENOR_MEDIA_FILE_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

async fn save_tenor_media(prefix: &str, extension: &str, bytes: &[u8]) -> Result<String> {
    use std::sync::atomic::Ordering;

    let dir = std::env::current_dir()?.join(MEDIA_DIR);
    tokio::fs::create_dir_all(&dir).await?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = TENOR_MEDIA_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{prefix}_{timestamp}_{counter}.{extension}"));
    tokio::fs::write(&path, bytes).await?;
    Ok(tokio::fs::canonicalize(&path)
        .await
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned())
}

async fn report_tenor_send_failure(
    tx: &Sender<WaEvent>,
    tmp_id: &str,
    chat_id: &str,
    message: &str,
) {
    let _ = tx
        .send(WaEvent::MessageFailed {
            msg_id: tmp_id.to_string(),
            chat_id: chat_id.to_string(),
        })
        .await;
    let _ = tx.send(WaEvent::ErrorToast(message.to_string())).await;
}

fn discover_tenor_web_credentials() -> std::result::Result<TenorCredentials, String> {
    let html = fetch_bounded_text(
        "https://tenor.com/search/trending-gifs",
        TENOR_PAGE_RESPONSE_LIMIT,
    )
    .map_err(|e| e.to_string())?;
    let encoded = extract_script_contents(&html, "data")
        .ok_or_else(|| "Tenor page did not expose its public web configuration".to_string())?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("invalid Tenor web configuration: {e}"))?;
    let config: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|e| format!("invalid Tenor web configuration JSON: {e}"))?;
    let api_key = config
        .get("API_V2_KEY")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Tenor web configuration has no API key".to_string())?;
    let client_key = config
        .get("API_V2_CLIENT_KEY")
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("tenor_web");
    Ok(TenorCredentials {
        api_key: api_key.to_string(),
        client_key: client_key.to_string(),
    })
}

fn tenor_credentials() -> Result<TenorCredentials> {
    if let Some(api_key) = tenor_api_key() {
        return Ok(TenorCredentials {
            api_key,
            client_key: "whatsapp_desktop".to_string(),
        });
    }
    TENOR_WEB_CREDENTIALS
        .get_or_init(discover_tenor_web_credentials)
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Percent-encode one URL component without relying on ad-hoc replacements.
fn encode_url_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn media_url<'a>(media: &'a serde_json::Value, formats: &[&str]) -> Option<&'a str> {
    formats.iter().find_map(|format| {
        media
            .get(*format)
            .and_then(|value| value.get("url"))
            .and_then(serde_json::Value::as_str)
            .filter(|url| url.starts_with("https://"))
    })
}

fn parse_tenor_values(
    values: &[serde_json::Value],
    kind: TenorSearchKind,
) -> Vec<crate::bridge::GifResult> {
    values
        .iter()
        .filter_map(|result| {
            let media = result.get("media_formats")?;
            let (preview, send) = match kind {
                TenorSearchKind::Gif => (
                    media_url(media, &["gifpreview", "nanogif", "tinywebp", "tinygif"]),
                    media_url(media, &["mp4", "tinymp4"]),
                ),
                TenorSearchKind::Sticker => (
                    media_url(
                        media,
                        &[
                            "nanowebp_transparent",
                            "tinywebp_transparent",
                            "webp_transparent",
                            "tinywebp",
                            "webp",
                        ],
                    ),
                    // Never fall back to GIF here: SendSticker declares image/webp.
                    media_url(
                        media,
                        &[
                            "webp_transparent",
                            "tinywebp_transparent",
                            "nanowebp_transparent",
                            "webp",
                            "tinywebp",
                        ],
                    ),
                ),
            };
            let preview = preview?;
            let send = send?;
            let title = result
                .get("content_description")
                .and_then(serde_json::Value::as_str)
                .or_else(|| result.get("title").and_then(serde_json::Value::as_str))
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| kind.noun());
            Some(crate::bridge::GifResult {
                preview_url: preview.to_string(),
                mp4_url: send.to_string(),
                title: title.to_string(),
            })
        })
        .take(TENOR_RESULT_LIMIT)
        .collect()
}

fn parse_tenor_api_results(
    body: &str,
    kind: TenorSearchKind,
) -> Result<Vec<crate::bridge::GifResult>> {
    let response: serde_json::Value = serde_json::from_str(body)?;
    let values = response
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Tenor response did not contain results"))?;
    Ok(parse_tenor_values(values, kind))
}

fn parse_tenor_page_results(
    page: &str,
    kind: TenorSearchKind,
) -> Result<Vec<crate::bridge::GifResult>> {
    let cache = extract_script_contents(page, "store-cache")
        .ok_or_else(|| anyhow::anyhow!("Tenor page did not contain search results"))?;
    let state: serde_json::Value = serde_json::from_str(cache)?;
    let searches = state
        .pointer("/universal/search")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("Tenor page search state was missing"))?;
    let values = searches
        .values()
        .find_map(|search| search.get("results").and_then(serde_json::Value::as_array))
        .ok_or_else(|| anyhow::anyhow!("Tenor page did not contain a result list"))?;
    Ok(parse_tenor_values(values, kind))
}

fn search_tenor(query: &str, kind: TenorSearchKind) -> Result<Vec<crate::bridge::GifResult>> {
    let query = query.trim();
    let trending = query.is_empty() || query.eq_ignore_ascii_case("trending");
    let credentials = tenor_credentials();
    let api_result = credentials.and_then(|credentials| {
        let endpoint = if trending { "featured" } else { "search" };
        let mut url = format!(
            "https://tenor.googleapis.com/v2/{endpoint}?key={}&client_key={}&limit={TENOR_RESULT_LIMIT}&contentfilter=medium",
            encode_url_component(&credentials.api_key),
            encode_url_component(&credentials.client_key),
        );
        if !trending {
            url.push_str("&q=");
            url.push_str(&encode_url_component(query));
        }
        match kind {
            TenorSearchKind::Gif => {
                url.push_str("&media_filter=gifpreview,nanogif,tinywebp,tinygif,mp4,tinymp4");
            }
            TenorSearchKind::Sticker => {
                url.push_str("&searchfilter=sticker&media_filter=nanowebp_transparent,tinywebp_transparent,webp_transparent,tinywebp,webp");
            }
        }
        let body = fetch_bounded_text(&url, TENOR_SEARCH_RESPONSE_LIMIT)?;
        parse_tenor_api_results(&body, kind)
    });

    match api_result {
        Ok(results) => Ok(results),
        Err(api_error) => {
            // Tenor stopped accepting new API clients in 2026. Its public search
            // page still carries the same result objects, so retain a bounded
            // fallback instead of leaving fresh installs permanently broken.
            log::debug!("Tenor API search unavailable, trying public page: {api_error:#}");
            let page_query = if trending { "trending" } else { query };
            let page_url = format!(
                "https://tenor.com/search/{}-{}",
                encode_url_component(page_query),
                kind.page_suffix()
            );
            let page =
                fetch_bounded_text(&page_url, TENOR_PAGE_RESPONSE_LIMIT).map_err(|page_error| {
                    anyhow::anyhow!("API: {api_error:#}; page: {page_error:#}")
                })?;
            parse_tenor_page_results(&page, kind)
        }
    }
}

fn messages_dir() -> PathBuf {
    PathBuf::from(MESSAGES_DIR)
}

fn messages_file(chat_id: &str) -> PathBuf {
    let safe = chat_id.replace(['/', '\\', '@', ':'], "_");
    messages_dir().join(format!("{safe}.bin"))
}

static ATOMIC_WRITE_CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Atomically write `data` to `path`: write a sibling temp file, fsync it, then
/// rename over the target. A crash/power-loss mid-write leaves either the intact
/// old file or the complete new one — never a truncated/corrupt aggregate file
/// (which previously meant an empty sidebar, lost history, or read chats
/// reverting to unread). Rename within the same directory is atomic on Linux.
pub(crate) fn atomic_write(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let n = ATOMIC_WRITE_CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{n}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Preserve a present-but-undecodable bincode file (schema drift / corruption)
/// as `<path>.corrupt` before anything can overwrite it, so the raw bytes stay
/// recoverable by a future decoder. Only copies once (won't clobber an existing
/// `.corrupt`), so repeated failed loads don't churn.
pub(crate) fn backup_corrupt_once(path: &std::path::Path) {
    let bak = path.with_extension("corrupt");
    if bak.exists() {
        return;
    }
    if let Err(e) = std::fs::copy(path, &bak) {
        log::warn!("backup_corrupt_once({}): {e}", path.display());
    } else {
        log::warn!(
            "Preserved undecodable {} → {} (history not lost; needs a decoder to recover)",
            path.display(),
            bak.display()
        );
    }
}

/// Read a bincode file with version header. Returns None on any failure.
fn read_bin<T: serde::de::DeserializeOwned>(path: &str) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 4 || data[..4] != BIN_HEADER {
        return None;
    }
    bincode::deserialize(&data[4..]).ok()
}

/// Write a bincode file with version header (atomically).
fn write_bin<T: serde::Serialize>(path: &str, value: &T) {
    if let Ok(payload) = bincode::serialize(value) {
        let mut data = Vec::with_capacity(4 + payload.len());
        data.extend_from_slice(&BIN_HEADER);
        data.extend_from_slice(&payload);
        if let Err(e) = atomic_write(std::path::Path::new(path), &data) {
            log::warn!("write_bin({path}) failed: {e}");
        }
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
        if let Err(e) = atomic_write(path, &data) {
            log::warn!("write_bin_path({}) failed: {e}", path.display());
        }
    }
}

pub fn load_chats() -> Vec<ChatSummary> {
    if let Some(chats) = read_bin::<Vec<ChatSummary>>(CHATS_FILE) {
        return chats;
    }
    // Fallback: try the PREVIOUS schema version (without the auto_mark_read
    // field). Bincode doesn't respect #[serde(default)] when a field is
    // missing, so adding a new field to ChatSummary makes old wa_chats.bin
    // files fail to deserialize. This fallback reads the old format, copies
    // the data into the new struct with defaults, and re-saves so it loads
    // cleanly next time.
    #[derive(serde::Deserialize)]
    struct LegacyChatSummary {
        id: String,
        name: String,
        last_message: String,
        timestamp: i64,
        unread_count: u32,
        is_group: bool,
        is_muted: bool,
        is_pinned: bool,
        #[serde(default)]
        is_archived: bool,
        #[serde(default)]
        is_favorite: bool,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        pinned_msg_id: Option<String>,
    }
    if let Some(legacy) = read_bin::<Vec<LegacyChatSummary>>(CHATS_FILE) {
        log::info!(
            "Migrating {} chats from legacy ChatSummary format (no auto_mark_read)",
            legacy.len()
        );
        let migrated: Vec<ChatSummary> = legacy
            .into_iter()
            .map(|c| ChatSummary {
                id: c.id,
                name: c.name,
                last_message: c.last_message,
                timestamp: c.timestamp,
                unread_count: c.unread_count,
                is_group: c.is_group,
                is_muted: c.is_muted,
                is_pinned: c.is_pinned,
                is_archived: c.is_archived,
                is_favorite: c.is_favorite,
                label: c.label,
                pinned_msg_id: c.pinned_msg_id,
                auto_mark_read: false,
            })
            .collect();
        // Re-save in new format so subsequent loads use the fast path
        write_bin(CHATS_FILE, &migrated);
        return migrated;
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

fn load_read_watermarks() -> HashMap<String, i64> {
    read_bin::<HashMap<String, i64>>(READ_WM_FILE).unwrap_or_default()
}

fn save_read_watermarks(map: &HashMap<String, i64>) {
    write_bin(READ_WM_FILE, map);
}

pub fn load_contact_names() -> HashMap<String, String> {
    let map = if let Some(names) = read_bin::<HashMap<String, String>>(CONTACTS_FILE) {
        names
    } else if let Ok(data) = std::fs::read_to_string(CONTACTS_FILE_JSON) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let global = crate::contacts::global();
    // Project names into the cross-protocol global directory.
    global.extend("whatsapp", map.iter().map(|(k, v)| (k.clone(), v.clone())));
    // Feed the LID→phone-JID resolution map into the directory so a chat
    // labeled `137340286709870@lid` can resolve to its saved-contact name.
    // Two effects per (lid, phone_jid) pair:
    //   1. record the LID against the phone digits → lookup-by-LID works
    //   2. if WhatsApp's contact map has a name for the LID OR the phone,
    //      project it into the directory under the phone digits
    let lid_phone = load_lid_phone_map();
    for (lid, phone_jid) in &lid_phone {
        global.record_lid_jid(phone_jid, lid);
        if let Some(name) = map.get(lid).or_else(|| map.get(phone_jid)) {
            global.insert(phone_jid, name, "whatsapp-lid-phone");
        }
    }
    // Also feed every WhatsApp chat_id (JID) into the directory so the
    // Phase-2 merge-map lookup `other_chat_id(wa_jid, "gmessages")` can
    // walk back via the digits index.
    let global = crate::contacts::global();
    for chat in load_chats() {
        if chat.id.ends_with("@g.us") {
            continue;
        }
        global.record_chat_id(&chat.id, "whatsapp", &chat.id);
    }
    global.save_if_dirty();
    map
}

fn save_contact_names(names: &HashMap<String, String>) {
    write_bin(CONTACTS_FILE, names);
}

/// Check if a proposed "name" is a valid display name (not a raw JID
/// or a phone-format fallback).
///
/// We reject phone-format fallbacks like "+14168235004" because they are
/// what `display_name_from_jid` produces when no real contact name is
/// known. Storing those in `contact_names` poisons the cache: a real
/// push_name arriving later won't replace the entry, and the chat shows
/// as a phone number until app restart (which loads the contact name
/// from a fresher source).
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
    // Reject phone-formatted fallbacks: "+1234567890" or pure digits.
    if let Some(rest) = name.strip_prefix('+') {
        if rest.chars().all(|c| c.is_ascii_digit()) && rest.len() >= 6 {
            return false;
        }
    }
    if name.chars().all(|c| c.is_ascii_digit()) && name.len() > 6 {
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

    // One-time poison purge (gated by marker — runs once per user only).
    // v1: removed entries where name == JID
    // v2: also remove phone-format fallback entries ("+1234567890") that
    //     were leaking in because is_valid_contact_name used to accept
    //     them, then preventing real push_names from replacing them.
    let purge_marker_v1 = std::path::PathBuf::from(".contact_purge_v1");
    let purge_marker_v2 = std::path::PathBuf::from(".contact_purge_v2");
    let mut purged = 0;
    if !purge_marker_v1.exists() {
        let before = names.len();
        names.retain(|jid, name| name != jid);
        purged = before - names.len();
        if purged > 0 {
            log::info!("One-time purge v1: removed {purged} entries where name == JID");
        }
        let _ = std::fs::write(&purge_marker_v1, "done");
    }
    if !purge_marker_v2.exists() {
        let before = names.len();
        names.retain(|_jid, name| {
            // Reject "+12345..." phone format
            if let Some(rest) = name.strip_prefix('+') {
                if rest.chars().all(|c| c.is_ascii_digit()) && rest.len() >= 6 {
                    return false;
                }
            }
            // Reject pure-digit fallbacks too
            if name.chars().all(|c| c.is_ascii_digit()) && name.len() > 6 {
                return false;
            }
            true
        });
        let removed_v2 = before - names.len();
        if removed_v2 > 0 {
            log::info!("One-time purge v2: removed {removed_v2} phone-format poison entries");
            purged += removed_v2;
        }
        let _ = std::fs::write(&purge_marker_v2, "done");
    }

    // The full message-history scan is expensive (1000+ files, bincode
    // deserialize each). Gate behind a marker so it runs ONCE — every
    // startup after that uses the cached contact_names directly.
    // Push_names from new messages are still captured in the live message
    // handler, so we don't lose anything by not re-scanning.
    let scan_marker = std::path::PathBuf::from(".contact_scan_v1");
    if scan_marker.exists() {
        log::debug!("Skipping contact name history scan (already done)");
        // Save in case the purge above changed anything.
        if purged > 0 {
            save_contact_names(&names);
        }
        return names;
    }

    let dir = messages_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        save_contact_names(&names);
        let _ = std::fs::write(&scan_marker, "done");
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
            if m.sender_name.is_empty() || m.sender_name == m.sender_id {
                continue;
            }
            if !names.contains_key(&m.sender_id) {
                names.insert(m.sender_id.clone(), m.sender_name.clone());
                learned += 1;
            }
        }
    }
    let _ = std::fs::write(&scan_marker, "done");
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

/// Load the set of phone digit-strings already swept for phone→LID mapping.
/// Missing/undecodable file → empty set (the sweep simply starts fresh).
fn load_lid_sweep_done() -> std::collections::HashSet<String> {
    read_bin::<std::collections::HashSet<String>>(LID_SWEEP_DONE_FILE).unwrap_or_default()
}

fn save_lid_sweep_done(done: &std::collections::HashSet<String>) {
    write_bin(LID_SWEEP_DONE_FILE, done);
}

/// Old bincode header (pre-is_system_message). Used for fallback deserialization.
const BIN_HEADER_V1: [u8; 4] = [0x57, 0x41, b'0', b'1'];

pub fn load_messages(chat_id: &str) -> Vec<IncomingMessage> {
    let bin_path = messages_file(chat_id);

    // Try current version first
    if let Some(msgs) = read_bin_path::<Vec<IncomingMessage>>(&bin_path) {
        return msgs;
    }

    // Present with the CURRENT header but undecodable by the current struct =
    // schema drift (a field added without a legacy decoder). Preserve the raw
    // bytes as `.corrupt` before returning empty, otherwise the very next
    // incoming message would load []→append→save and permanently overwrite the
    // old history. The `.corrupt` copy keeps it recoverable.
    if let Ok(data) = std::fs::read(&bin_path) {
        if data.len() >= 4 && data[..4] == BIN_HEADER {
            backup_corrupt_once(&bin_path);
            log::warn!(
                "load_messages({chat_id}): current-format decode failed; preserved as .corrupt, showing empty"
            );
            return vec![];
        }
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

/// Global serialization for message-file writes. [`save_messages_scoped`]
/// is a read-modify-write (it reads the *other* protocol's messages off
/// disk, splices, and writes back). Without this lock the WhatsApp
/// disk-writer thread and a gmessages task could interleave and lose each
/// other's update.
static MSG_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Chats whose on-disk message file was updated by the **gmessages** runtime
/// — a separate thread that can't reach this runtime's in-memory `s.history`.
/// `load_chat`'s fast path serves the cached last-50 on a hit and never
/// re-reads disk, so without this an SMS/MMS the gm runtime appended to disk
/// stays invisible in the open conversation until an app restart forces a
/// cache miss. The gm runtime flags the chat here on every write; `LoadChat`
/// reconciles from disk on the next open for any flagged chat, then clears it.
static GM_DIRTY_CHATS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Flag a chat as having gm-written, cache-unseen messages on disk. Called by
/// the gmessages runtime after it appends an incoming SMS/MMS to disk.
pub fn mark_gm_dirty(chat_id: &str) {
    GM_DIRTY_CHATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(chat_id.to_string());
}

/// Clear and return whether `chat_id` was flagged dirty by the gm runtime.
fn take_gm_dirty(chat_id: &str) -> bool {
    GM_DIRTY_CHATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(chat_id)
}

/// Plain overwrite of a chat's message file. Used **only** for in-place
/// format migration (legacy → "WA02"), where `messages` already IS the
/// file's full content for every protocol. Every real save goes through
/// [`save_messages_scoped`], which protects the other protocol's history.
pub fn save_messages(chat_id: &str, messages: &[IncomingMessage]) {
    let dir = messages_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let _guard = MSG_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    write_bin_path(&messages_file(chat_id), &messages.to_vec());
}

/// Persist a chat's messages **without ever touching the other protocol's
/// history**.
///
/// A single `wa_messages/<id>.bin` file can hold both WhatsApp messages
/// *and* Google Messages (SMS/MMS/RCS) messages once a contact's WhatsApp
/// chat and SMS thread are merged into one row. The catch: each runtime
/// only has a complete, reliable view of its OWN protocol. The WhatsApp
/// runtime's `s.history` may carry a handful of SMS messages incidentally
/// (the ones that happened to arrive as live events while a chat was
/// open) — but never all of them. So a WhatsApp save must NOT be treated
/// as authoritative for SMS, and vice versa. A blind overwrite — or even
/// a "merge by which protocols are present" — silently drops the SMS
/// messages (incl. MMS images) the saver didn't happen to have in memory.
///
/// `owned` is the protocol the caller actually owns. This writes:
///   * every on-disk message of OTHER protocols, verbatim — that
///     protocol's own runtime is the canonical source and keeps the file
///     current; plus
///   * the `owned`-protocol messages taken from `messages` (authoritative).
///
/// Non-`owned` entries in `messages` are ignored (the on-disk copy wins).
/// The result is timestamp-sorted so the two streams interleave. The whole
/// read-modify-write runs under [`MSG_FILE_LOCK`].
pub fn save_messages_scoped(
    chat_id: &str,
    owned: crate::bridge::MessageSource,
    messages: &[IncomingMessage],
) {
    use crate::bridge::MessageSource;
    // A scoped save with nothing to save is a no-op. Writing an empty
    // `owned` set would delete that protocol's entire history (e.g. if an
    // in-memory cache momentarily glitched empty) — never lose data so.
    if messages.is_empty() {
        return;
    }
    let dir = messages_dir();
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    let path = messages_file(chat_id);

    let _guard = MSG_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Existing on-disk content in the current ("WA02") format. Use
    // `read_bin_path`, NOT `load_messages` — the latter re-saves legacy
    // formats via `save_messages` and would recurse.
    let existing = read_bin_path::<Vec<IncomingMessage>>(&path).unwrap_or_default();

    let mut result: Vec<IncomingMessage> = Vec::with_capacity(existing.len() + messages.len());
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // 1. Other protocols' messages — verbatim from disk (canonical).
    for m in &existing {
        if MessageSource::from_message(&m.chat_id, &m.id) != owned && seen.insert(m.id.as_str()) {
            result.push(m.clone());
        }
    }
    // 2. The owned protocol's messages — from the caller (authoritative).
    for m in messages {
        if MessageSource::from_message(&m.chat_id, &m.id) == owned && seen.insert(m.id.as_str()) {
            result.push(m.clone());
        }
    }
    // Stable sort interleaves the two streams by time while preserving
    // each stream's internal order for equal timestamps.
    result.sort_by_key(|m| m.timestamp);

    write_bin_path(&path, &result);
}

/// Append a single message to an existing chat's message file.
/// Loads existing messages, appends (deduplicating by ID), and saves.
///
/// **Safeguard against schema-mismatch data loss**: if loading returned no
/// messages but the on-disk file is non-trivially sized (>64 bytes), we
/// REFUSE to save — overwriting that file with one message would destroy
/// whatever's on disk. This guarded against a real incident where adding
/// a new field to `IncomingMessage` made bincode silently fail to read,
/// then the next message wiped the chat history.
pub fn save_messages_append(chat_id: &str, msg: &IncomingMessage) {
    let mut messages = load_messages(chat_id);
    if messages.is_empty() {
        let path = messages_file(chat_id);
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > 64
        {
            log::error!(
                "save_messages_append: refusing to save — load returned empty but {} is {} bytes (suspected schema mismatch). Skipping write to avoid destroying history.",
                path.display(),
                meta.len()
            );
            return;
        }
    }
    if !messages.iter().any(|m| m.id == msg.id) {
        messages.push(msg.clone());
        // gmessages is the only caller of this path — save scoped to SMS
        // so a concurrent WhatsApp save can't drop these messages.
        save_messages_scoped(
            chat_id,
            crate::bridge::MessageSource::GoogleMessages,
            &messages,
        );
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
/// Map a file extension to a proper MIME type. Used when sending
/// documents — WhatsApp receivers store files with `application/octet-stream`
/// as `.bin`, losing the original extension. Returning the correct MIME
/// preserves the file's true type (pdf stays pdf, xlsx stays xlsx, etc.).
fn mime_from_extension(lower_path: &str) -> String {
    let ext = lower_path.rsplit('.').next().unwrap_or("");
    let mime = match ext {
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "rtf" => "application/rtf",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "apk" => "application/vnd.android.package-archive",
        "exe" | "msi" => "application/x-msdownload",
        "dmg" => "application/x-apple-diskimage",
        "deb" => "application/vnd.debian.binary-package",
        "rpm" => "application/x-rpm",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// Resolve a LID JID (`12345@lid`) or any "speculative" JID to the
/// CANONICAL WhatsApp phone JID for that contact (`12345@s.whatsapp.net`),
/// if one is known. Returns `None` if there's no mapping — caller should
/// fall back to whatever JID they had.
///
/// Used by "Reply privately" / "Message user" actions in group chats:
/// without this, we'd open a new DM under the participant's LID, which
/// becomes a SECOND chat row distinct from any existing chat the user
/// has with that contact under their phone JID. Server fanout of our
/// outgoing message lands in the phone-JID chat anyway, so the LID row
/// just collects orphan copies.
pub fn lid_to_canonical_phone_jid(maybe_lid: &str) -> Option<String> {
    // 1. Global cross-protocol directory: stores phone digits with all
    //    known JID variants on the same entry.
    if let Some(entry) = crate::contacts::global().lookup_full(maybe_lid)
        && let Some(jid) = entry.chat_ids.get("whatsapp")
    {
        return Some(jid.clone());
    }
    // 2. On-disk lid_to_phone map (built from previous sessions).
    if maybe_lid.ends_with("@lid") {
        let map = load_lid_phone_map();
        if let Some(phone) = map.get(maybe_lid) {
            return Some(phone.clone());
        }
    }
    None
}

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
        let digits: String = user.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 5 {
            return format!("+{digits}");
        }
    }
    jid.to_string()
}

/// Persist a live/local chat upsert. `from_reseed == false` — the read-watermark
/// clobber in [`RuntimeState::upsert_chat`] does NOT apply, so a genuine unread
/// bump is never zeroed by a same-second local mark-read watermark.
fn persist_chat(state: &Arc<Mutex<RuntimeState>>, summary: ChatSummary) {
    state.lock().unwrap().upsert_chat(summary, false, false);
}

/// Persist a history-sync / server-reseed chat upsert whose `summary.unread_count`
/// is the SERVER's authoritative value (history sync provided `conv.unread_count`
/// explicitly). This lets a phone-side read (`unread_count == 0`) clear the
/// desktop badge instead of being overridden by the stale-preserve heuristic.
/// `from_reseed == true` so the read-watermark defense rejects a stale server
/// unread the user already cleared locally.
fn persist_chat_authoritative(state: &Arc<Mutex<RuntimeState>>, summary: ChatSummary) {
    state.lock().unwrap().upsert_chat(summary, true, true);
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
    // 6. Cross-protocol global directory: fuzzy phone matching (full digits,
    //    last 10, last 7). This is where SMS/gmessages contacts merge in,
    //    and also covers WhatsApp numbers stored under a slightly-different
    //    JID format than what we have in `contact_names` (the most common
    //    cause of "quoted message shows raw phone but timeline shows real
    //    name" — they take different code paths).
    if let Some(name) = crate::contacts::global().lookup(sender_jid) {
        // Sanity check: the global may sometimes hand back a placeholder
        // that still looks like a raw JID. Treat that as a miss.
        if !name.contains('@') && !name.is_empty() {
            return name;
        }
    }
    // 7. For LID JIDs with no resolution, try to at least show the phone number
    if sender_jid.ends_with("@lid") {
        if let Some(phone) = s.lid_to_phone.get(sender_jid) {
            return display_name_from_jid(phone);
        }
    }
    // 8. Formatted phone number fallback
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

/// Given a raw @mention token (leading '@' already stripped), produce the
/// best display form for it. Returns `Some(display_without_leading_at)` when
/// the token is a resolvable/formattable JID mention, `None` when it should be
/// left exactly as-is (not a numeric JID mention).
///
/// Resolution order:
///   1. A real contact/push name (passes `is_valid_contact_name`) → that name.
///   2. Otherwise map @lid → canonical phone JID and format as "+phone".
///   3. Otherwise strip the "@server" suffix and show the bare number as "+num".
fn resolve_mention_token(jid_part: &str, s: &RuntimeState) -> Option<String> {
    // Only numeric JID mentions (e.g. "12345@lid", "12345@s.whatsapp.net",
    // or a bare "12345"). A leading non-digit means it's already a name.
    if !jid_part
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        return None;
    }

    // Candidate JID formats to try for a real name.
    let candidates = if jid_part.contains('@') {
        vec![jid_part.to_string()]
    } else {
        vec![
            format!("{jid_part}@lid"), // LID format (most common in mentions)
            format!("{jid_part}@s.whatsapp.net"), // Phone format
        ]
    };

    // 1. Prefer a genuine resolved name (rejects '+digits'/pure-digit/'@' via
    //    is_valid_contact_name, so a phone-format fallback never wins here).
    for full_jid in &candidates {
        let name = resolve_sender_name(s, full_jid);
        if is_valid_contact_name(&name, full_jid) {
            return Some(name);
        }
    }

    // 2. No real name yet — never leak a raw "@12345@lid"/"@...@s.whatsapp.net"
    //    with its server suffix. Map @lid → phone first, then format "+phone".
    for full_jid in &candidates {
        if let Some(phone_jid) = lid_to_canonical_phone_jid(full_jid) {
            let formatted = display_name_from_jid(&phone_jid);
            if !formatted.contains('@') {
                return Some(formatted);
            }
        }
    }

    // 3. Last resort: format whatever number we have (strips "@server").
    let formatted = display_name_from_jid(&candidates[0]);
    if !formatted.contains('@') {
        return Some(formatted);
    }
    // Even display_name couldn't format it — strip the server suffix manually
    // so the user never sees the raw "@lid"/"@s.whatsapp.net" tail.
    let bare = jid_part.split('@').next().unwrap_or(jid_part);
    Some(format!("+{bare}"))
}

/// Replace @JID mentions in message text with @DisplayName.
///
/// Walks the text token-by-token (preserving original whitespace) instead of
/// doing a global substring `replace`, so one mention token can never mangle
/// another when one is a substring of the other.
fn resolve_mentions(text: &str, s: &RuntimeState) -> String {
    let mut result = String::with_capacity(text.len());
    // Split on whitespace boundaries while keeping the separators intact.
    let mut rest = text;
    while !rest.is_empty() {
        // Emit any leading whitespace verbatim.
        let ws_end = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if ws_end > 0 {
            result.push_str(&rest[..ws_end]);
            rest = &rest[ws_end..];
            if rest.is_empty() {
                break;
            }
        }
        // Grab the next whitespace-delimited token.
        let tok_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let word = &rest[..tok_end];
        rest = &rest[tok_end..];

        if word.starts_with('@') && word.len() >= 4 {
            let jid_part = &word[1..]; // strip leading @
            if let Some(display) = resolve_mention_token(jid_part, s) {
                result.push('@');
                result.push_str(&display);
                continue;
            }
        }
        result.push_str(word);
    }
    result
}

// ── Shared runtime state ──────────────────────────────────────────────────────

/// Provenance/priority of a stored contact name. A higher-priority source may
/// overwrite a lower-priority one, but never the reverse — so a sender's
/// self-chosen WhatsApp push_name can't clobber the user's phonebook name.
///
/// Ordering matters: `Phonebook` > `PushName` > `History`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NameSource {
    /// Name inferred from history-sync/typing/other low-confidence paths.
    History,
    /// Sender's self-chosen WhatsApp display name (`push_name`).
    PushName,
    /// The user's own phonebook / app-state ContactUpdate name — always wins.
    Phonebook,
}

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
    /// Last INCOMING (not-from-me) message id per chat — the correct anchor for
    /// a read receipt. `last_msg_id` includes our own sends, so acking it tells
    /// the phone nothing and the chat stays unread there.
    last_incoming_msg_id: HashMap<String, String>,
    /// Chat display names (kept in sync with chats[].name)
    chat_names: HashMap<String, String>,
    /// LID JID → phone JID mapping for resolving LID-addressed group messages
    lid_to_phone: HashMap<String, String>,
    /// Reverse map: phone JID → LID JID (for O(1) reverse lookups in name resolution)
    phone_to_lid: HashMap<String, String>,
    /// Names from app-state ContactUpdate (phonebook sync) — highest-priority name source.
    /// Separate from chat_names so we never mistake a saved phone-number for a real name.
    contact_names: HashMap<String, String>,
    /// Provenance of each `contact_names` entry, so a lower-priority source
    /// (e.g. a sender's push_name) can never overwrite a higher-priority one
    /// (e.g. the user's phonebook name). Entries loaded from disk have unknown
    /// provenance and are treated as `History` (lowest) so any live update may
    /// correct them. Not persisted — rebuilt from live events each session.
    contact_name_sources: HashMap<String, NameSource>,
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
    /// True once the proactive phone→LID prewarm sweep has run this session.
    did_phone_lid_sweep: bool,
    /// Per-chat read watermark (chat_id → last-read message timestamp).
    /// Persisted to [`READ_WM_FILE`]; consulted by [`RuntimeState::upsert_chat`]
    /// to defend a locally-read chat against a stale reconnect reseed.
    read_watermarks: HashMap<String, i64>,
    /// Watermarks derived ONLY from self-read receipts (a chat read on another
    /// device). Kept SEPARATE from `read_watermarks` so gating the live unread
    /// bump on them does not reintroduce the same-second suppression that local
    /// reads / auto-mark-read would cause. In-memory only — its job is to win the
    /// offline-flush race where a self-read receipt arrives before the messages it
    /// covers, so those messages are not bumped to unread when they finally decode.
    receipt_watermarks: HashMap<String, i64>,
    /// The chat currently open in the UI (via `WaCommand::SetActiveChat`).
    /// An incoming message for this chat is not counted as unread (the user is
    /// looking at it), so the persisted `unread_count` stays a true source of
    /// truth that survives restart.
    active_chat: Option<String>,
    /// Projection tap: the shared UI event channel. Set once in `run_inner`
    /// right after state construction. `emit_row` uses it to push an
    /// authoritative `ChatRowChanged` whenever a WA-owned summary mutates —
    /// the sidebar is a pure projection of this persisted state. `None` only
    /// during the brief window before `run_inner` wires it up.
    ui_tx: Option<Sender<WaEvent>>,
}

impl RuntimeState {
    fn new(
        save_tx: std::sync::mpsc::Sender<Vec<ChatSummary>>,
        msg_save_tx: std::sync::mpsc::Sender<(String, Vec<IncomingMessage>)>,
    ) -> Self {
        let chats = load_chats();
        // Read watermarks defend already-read chats from reconnect reseeds.
        // First run after upgrade: the file won't exist, so seed it from every
        // chat that is currently read (unread == 0) using its last-message
        // timestamp. This makes the defense effective on the very first
        // post-upgrade reconnect, before the user re-opens anything. A chat
        // with genuinely newer server activity still has a higher reseed
        // timestamp, so it correctly stays unread.
        let mut read_watermarks = load_read_watermarks();
        if read_watermarks.is_empty() {
            for c in &chats {
                if c.unread_count == 0 && c.timestamp > 0 {
                    read_watermarks.insert(c.id.clone(), c.timestamp);
                }
            }
            if !read_watermarks.is_empty() {
                save_read_watermarks(&read_watermarks);
                log::info!(
                    "Seeded {} read watermarks from existing chats",
                    read_watermarks.len()
                );
            }
        }
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
            last_incoming_msg_id: HashMap::new(),
            chat_names,
            lid_to_phone,
            phone_to_lid,
            contact_names,
            contact_name_sources: HashMap::new(),
            save_tx,
            msg_save_tx,
            own_phone: String::new(),
            own_lid: String::new(),
            connect_count: 0,
            did_phone_lid_sweep: false,
            read_watermarks,
            receipt_watermarks: HashMap::new(),
            active_chat: None,
            ui_tx: None,
        }
    }

    /// Push an authoritative sidebar-row refresh for one chat. Renders the
    /// chat's current persisted summary VERBATIM on the UI side — no guards,
    /// no UI clock. `gm:` ids are owned by the gmessages runtime, never the WA
    /// runtime, so we hard-skip them here (they are not even in `self.chats`).
    /// `try_send` is sync-safe (this runs under the state lock on tokio
    /// workers) and the channel is unbounded so it never blocks or drops.
    fn emit_row(&self, chat_id: &str) {
        if chat_id.starts_with("gm:") {
            return; // ownership rule: gm rows are emitted only by gmessages_runtime
        }
        if let (Some(tx), Some(c)) = (&self.ui_tx, self.chats.iter().find(|c| c.id == chat_id)) {
            let _ = tx.try_send(WaEvent::ChatRowChanged(c.clone()));
        }
    }

    /// Emit an EPHEMERAL row refresh: render `preview` now, but DO NOT persist
    /// it (state is untouched, nothing hits disk). Used for reaction previews
    /// ("Reacted 👍") — the sidebar shows the reaction live, but a restart
    /// intentionally falls back to the underlying message text. No-op for `gm:`
    /// ids and for chats not in `self.chats`.
    fn emit_row_ephemeral(&self, chat_id: &str, preview: &str) {
        if chat_id.starts_with("gm:") {
            return;
        }
        if let (Some(tx), Some(c)) = (&self.ui_tx, self.chats.iter().find(|c| c.id == chat_id)) {
            let mut clone = c.clone();
            clone.last_message = preview.to_string();
            let _ = tx.try_send(WaEvent::ChatRowChanged(clone));
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
    /// Keep the working set small. Full histories contain owned strings and
    /// media metadata, so retaining thousands of chats can consume hundreds
    /// of megabytes. Evicted chats remain on disk and are loaded on demand.
    fn evict_old_histories(&mut self) {
        const MAX_CACHED: usize = 32;
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
    /// Normalizes both JIDs by stripping the `:device` suffix so the cache
    /// stays consistent across reconnects (WhatsApp's server stamps device
    /// suffixes that can rotate). Also keeps the original-form variants
    /// for cheap lookup compatibility.
    fn insert_lid_phone(&mut self, lid: String, phone: String) {
        let strip_dev = |j: &str| -> String {
            match (j.find(':'), j.find('@')) {
                (Some(c), Some(a)) if c < a => format!("{}{}", &j[..c], &j[a..]),
                _ => j.to_string(),
            }
        };
        let lid_base = strip_dev(&lid);
        let phone_base = strip_dev(&phone);

        // Always store the bare→bare mapping (canonical, device-agnostic).
        self.phone_to_lid
            .insert(phone_base.clone(), lid_base.clone());
        self.lid_to_phone
            .insert(lid_base.clone(), phone_base.clone());

        // Also store the originals if they differ from the bare forms — so
        // callers passing device-suffixed JIDs still hit on direct lookup.
        if lid != lid_base {
            self.lid_to_phone.insert(lid.clone(), phone_base.clone());
        }
        if phone != phone_base {
            self.phone_to_lid.insert(phone.clone(), lid_base.clone());
        }
    }

    fn record_contact_name(
        &mut self,
        jid: &str,
        name: &str,
        also_jid: Option<&str>,
        source: NameSource,
    ) -> bool {
        if !is_valid_contact_name(name, jid) {
            return false;
        }
        // Refuse to overwrite an entry recorded from a strictly higher-priority
        // source (e.g. a push_name must not clobber a phonebook name). Entries
        // with no tracked provenance (loaded from disk) default to History
        // (lowest) so any live source may correct them.
        let may_write = |sources: &HashMap<String, NameSource>, key: &str| -> bool {
            let existing = sources.get(key).copied().unwrap_or(NameSource::History);
            source >= existing
        };
        let mut changed = false;
        if may_write(&self.contact_name_sources, jid) {
            if self.contact_names.get(jid).map(|n| n.as_str()) != Some(name) {
                self.contact_names.insert(jid.to_string(), name.to_string());
                changed = true;
            }
            // Upgrade the tracked provenance even when the name text is
            // unchanged, so a later lower-priority source can't overwrite it.
            self.contact_name_sources.insert(jid.to_string(), source);
        }
        if let Some(alt) = also_jid {
            if !alt.is_empty()
                && is_valid_contact_name(name, alt)
                && may_write(&self.contact_name_sources, alt)
            {
                if self.contact_names.get(alt).map(|n| n.as_str()) != Some(name) {
                    self.contact_names.insert(alt.to_string(), name.to_string());
                    changed = true;
                }
                self.contact_name_sources.insert(alt.to_string(), source);
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
    /// Mark a chat read locally: zero its unread badge AND stamp a read
    /// watermark at its latest-seen message timestamp. The watermark is what
    /// lets [`RuntimeState::upsert_chat`] reject a later stale reconnect reseed
    /// that would otherwise resurrect the badge. Works for WhatsApp, SMS/gm,
    /// and merged chats — any chat present in `self.chats`. Persists both the
    /// chat list and the watermark map only when something actually changed.
    fn mark_chat_read_local(&mut self, chat_id: &str) {
        let mut changed = false;
        let mut watermark = 0i64;
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            if c.unread_count != 0 {
                c.unread_count = 0;
                changed = true;
            }
            watermark = c.timestamp;
        }
        if watermark > 0 {
            let prev = self.read_watermarks.get(chat_id).copied().unwrap_or(0);
            if watermark > prev {
                self.read_watermarks.insert(chat_id.to_string(), watermark);
                // Offload the bincode-serialize + std::fs::write to a background
                // thread instead of doing it synchronously under the state lock
                // (this runs on a tokio worker and, for auto-mark-read chats,
                // fires on nearly every incoming message). The watermark map is
                // small, so the snapshot clone is cheap — matching how
                // save_lid_phone_map / save_contact_names are already offloaded.
                let snapshot = self.read_watermarks.clone();
                std::thread::spawn(move || save_read_watermarks(&snapshot));
                changed = true;
            }
        }
        if changed {
            let _ = self.save_tx.send(self.chats.clone());
            // The badge (and possibly the watermark) moved — refresh the row so
            // the sidebar clears the unread pill without a second UI-side write.
            self.emit_row(chat_id);
        }
    }

    /// Set a chat's preview text WITHOUT touching its timestamp (so the row
    /// does not reorder) or its unread count. This is the single choke point
    /// for the "latest message's TEXT changed but not its position" cases:
    /// an edit of the latest message, a revoke/delete of the latest message,
    /// a re-resolved group-sender prefix, and ClearChat (preview → ""). It
    /// persists and emits an authoritative row refresh. No-op if the chat is
    /// absent or the text is already what's stored.
    fn set_chat_preview(&mut self, chat_id: &str, preview: &str) {
        let mut changed = false;
        if let Some(c) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            if c.last_message != preview {
                c.last_message = preview.to_string();
                changed = true;
            }
        }
        if changed {
            let _ = self.save_tx.send(self.chats.clone());
            self.emit_row(chat_id);
        }
    }

    /// `from_reseed` marks history-sync / server-reseed / app-state paths, where
    /// the incoming `unread_count` is the SERVER's (possibly stale) value. Only
    /// those paths are subject to the read-watermark clobber below. The live
    /// message path (`persist_new_message`) and local actions pass `false` so a
    /// genuine unread bump is never zeroed by a same-second watermark.
    fn upsert_chat(
        &mut self,
        mut summary: ChatSummary,
        authoritative_unread: bool,
        from_reseed: bool,
    ) {
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
        // Snapshot the incoming activity + our read watermark BEFORE `summary`
        // is moved, so the reseed-defense below can decide whether this update
        // carries genuinely newer activity than what the user has already read.
        let incoming_ts = summary.timestamp;
        let incoming_unread = summary.unread_count;
        let read_watermark = self.read_watermarks.get(&summary.id).copied();
        // Captured before `summary` is moved into the row, so the post-sort
        // `emit_row` can find the entry by id. `row_changed` gates the emit.
        let emit_id = summary.id.clone();
        let row_changed;
        if let Some(existing) = self.chats.iter_mut().find(|c| c.id == summary.id) {
            // Snapshot the pre-mutation row so we can decide, AFTER the merge
            // below, whether anything the sidebar renders actually changed —
            // and skip the emit if not (prevents an event flood during
            // history-sync / reconnect reseed storms; ChatsLoaded covers bulk).
            let snap_before = (
                existing.last_message.clone(),
                existing.timestamp,
                existing.unread_count,
                existing.is_pinned,
                existing.is_muted,
                existing.is_archived,
                existing.is_favorite,
                existing.label.clone(),
                existing.auto_mark_read,
            );

            // Producer-side monotonicity hardening (replaces the deleted UI
            // guard): keep the old preview when the existing row is
            // strictly-newer-and-non-empty (as before) OR when the incoming
            // preview is empty but the existing one is not — an empty preview
            // must never clobber real text (CreateGroup / StartNewChat upsert
            // ts=now + empty preview and would otherwise blank a live row).
            let keep_old_preview = (existing.timestamp > summary.timestamp
                && !existing.last_message.is_empty())
                || (summary.last_message.is_empty() && !existing.last_message.is_empty());
            let old_msg = existing.last_message.clone();
            let old_ts = existing.timestamp;
            // Never move the row's sort clock backward: if the incoming
            // timestamp is older than what we already have, keep the old one
            // (a stale reseed / a ts=now placeholder must not sink the row).
            let keep_old_timestamp = summary.timestamp < existing.timestamp;

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
            let old_auto_mark_read = existing.auto_mark_read;

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
            existing.auto_mark_read = old_auto_mark_read;
            if old_label.is_some() {
                existing.label = old_label;
            }
            if old_pinned_msg.is_some() {
                existing.pinned_msg_id = old_pinned_msg;
            }

            // But restore the preview if the old one was newer / the incoming
            // one was an empty clobber.
            if keep_old_preview {
                existing.last_message = old_msg;
            }
            // Restore the old (newer) timestamp independently — an empty-preview
            // guard and a backward-timestamp guard are separate concerns.
            if keep_old_timestamp {
                existing.timestamp = old_ts;
            }
            // Don't reset unread to 0 if the update doesn't carry unread info.
            // BUT when the count is authoritative (history sync sent an explicit
            // conv.unread_count), trust it — a phone-side read arrives as an
            // authoritative 0 and must be allowed to clear the desktop badge.
            if !authoritative_unread && existing.unread_count == 0 && old_unread > 0 {
                existing.unread_count = old_unread;
            }
            // Reseed-defense: a history-sync / list_conversations summary carries
            // the SERVER's unread count, which is stale for a chat the user read
            // on THIS device (the local read may never have propagated to the
            // server). If our read watermark already covers the incoming
            // summary's latest activity, the count is stale — keep the chat read.
            // A genuinely newer message (timestamp past the watermark) is still
            // allowed to mark unread. This is what stops read chats from
            // reverting to unread on every reboot/suspend reconnect.
            //
            // Only applies to reseed paths. The live-message path passes
            // `from_reseed == false` so a real unread bump that lands in the same
            // second as a local mark-read (watermarks are second-resolution) or
            // arrives from a slightly-behind sender clock is NOT zeroed. The
            // comparison stays `>=` here because a chat read at exactly the
            // last-message timestamp reseeds with that same second, and that
            // stale server unread must still be clobbered — the `from_reseed`
            // gate (not a stricter comparison) is what protects live bumps.
            if from_reseed && incoming_unread > 0 && existing.unread_count > 0 {
                if let Some(wm) = read_watermark {
                    if wm >= incoming_ts {
                        existing.unread_count = 0;
                    }
                }
            }
            // Change-detection for the row emit: compare the sidebar-visible
            // fields against the pre-mutation snapshot. A no-op upsert (common
            // during reseed storms — every conversation re-arrives unchanged)
            // emits nothing, so the UI channel isn't flooded.
            let snap_after = (
                existing.last_message.clone(),
                existing.timestamp,
                existing.unread_count,
                existing.is_pinned,
                existing.is_muted,
                existing.is_archived,
                existing.is_favorite,
                existing.label.clone(),
                existing.auto_mark_read,
            );
            row_changed = snap_after != snap_before;
        } else {
            self.chats.push(summary);
            row_changed = true; // a brand-new row always needs an emit
        }
        self.chats.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let _ = self.save_tx.send(self.chats.clone());
        // Authoritative sidebar refresh — only when something actually changed.
        if row_changed {
            self.emit_row(&emit_id);
        }
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

                // Strip :device suffix to a base form. Chats may be keyed by
                // any device-suffixed variant (e.g. "12345:5@s.whatsapp.net")
                // while contact_names keys land on the non-AD form. Without
                // this fallback, `Francis Lau Hedgefund` stored under the
                // base JID never matches a chat keyed by a device variant.
                let strip_dev = |j: &str| -> String {
                    match (j.find(':'), j.find('@')) {
                        (Some(c), Some(a)) if c < a => format!("{}{}", &j[..c], &j[a..]),
                        _ => j.to_string(),
                    }
                };
                let base_id = strip_dev(&c.id);

                // Priority: contact_names by chat_id → device-stripped → LID
                // reverse lookup → device-stripped LID — then fall through.
                let name = self
                    .contact_names
                    .get(&c.id)
                    .cloned()
                    .or_else(|| {
                        if base_id != c.id {
                            self.contact_names.get(&base_id).cloned()
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        // If chat_id is phone JID, use reverse map to find LID
                        if c.id.ends_with("@s.whatsapp.net") {
                            if let Some(lid) = self
                                .phone_to_lid
                                .get(&c.id)
                                .or_else(|| self.phone_to_lid.get(&base_id))
                            {
                                let lid_base = strip_dev(lid);
                                if let Some(n) = self
                                    .contact_names
                                    .get(lid)
                                    .or_else(|| self.contact_names.get(&lid_base))
                                {
                                    return Some(n.clone());
                                }
                            }
                        }
                        // If chat_id is LID, resolve to phone and look up
                        if c.id.ends_with("@lid") {
                            if let Some(phone) = self
                                .lid_to_phone
                                .get(&c.id)
                                .or_else(|| self.lid_to_phone.get(&base_id))
                            {
                                let phone_base = strip_dev(phone);
                                if let Some(n) = self
                                    .contact_names
                                    .get(phone)
                                    .or_else(|| self.contact_names.get(&phone_base))
                                {
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

pub async fn run_wa_runtime(event_tx: Sender<WaEvent>, cmd_rx: UnboundedReceiver<WaCommand>) {
    // Fork the command stream: anything addressed to a `gm:` chat goes to
    // the gmessages runtime; everything else flows to the WhatsApp runtime.
    // If gmessages is disabled, we just forward everything to WhatsApp (the
    // `gm_cmd_tx.is_none()` path below). Created BEFORE the gm spawn so the gm
    // runtime can send WaCommand::TouchChatSummary back to us for merged-chat
    // SMS (it owns no non-gm summaries — the WA runtime is the sole writer).
    let (wa_cmd_tx, mut wa_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WaCommand>();

    // Sibling: Google Messages integration. No-op unless GMESSAGES_ENABLE=1.
    let gm_data_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let gm_cmd_tx =
        crate::gmessages_runtime::spawn(&gm_data_dir, event_tx.clone(), wa_cmd_tx.clone());

    {
        let mut cmd_rx = cmd_rx;
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                // MarkRead must reach BOTH runtimes. The WhatsApp runtime owns
                // the shared RuntimeState, where we stamp a local read watermark
                // for EVERY chat (WhatsApp, SMS/gm, merged) so a reconnect reseed
                // can't resurrect the badge; the gmessages runtime tells Google's
                // server. Without the WA leg a pure `gm:N` read is never
                // watermarked and reverts on the next list_conversations; without
                // the gm leg a merged SMS is re-flagged unread on restart.
                if let WaCommand::MarkRead { chat_id } = cmd {
                    if let Some(tx) = &gm_cmd_tx {
                        let _ = tx.send(WaCommand::MarkRead {
                            chat_id: chat_id.clone(),
                        });
                    }
                    if wa_cmd_tx.send(WaCommand::MarkRead { chat_id }).is_err() {
                        break;
                    }
                    continue;
                }
                // MarkUnread persists in the shared RuntimeState (owned by the WA
                // runtime); route it there for ANY chat (incl. gm:) so a gm chat's
                // mark-unread also persists rather than being dropped by gm.
                //
                // For a gm/SMS chat ALSO forward it to the gmessages runtime so it
                // can roll its own read watermark back. Without the gm leg the gm
                // watermark still says "read" and re-clamps the chat to read on the
                // next list_conversations reseed (mirror of the MarkRead fan-out).
                if let WaCommand::MarkUnread { chat_id } = cmd {
                    if crate::gmessages_runtime::is_gm_chat(&chat_id) {
                        if let Some(tx) = &gm_cmd_tx {
                            let _ = tx.send(WaCommand::MarkUnread {
                                chat_id: chat_id.clone(),
                            });
                        }
                    }
                    if wa_cmd_tx.send(WaCommand::MarkUnread { chat_id }).is_err() {
                        break;
                    }
                    continue;
                }
                // SetActiveChat must reach BOTH runtimes: the WA runtime owns
                // unread suppression for WA/merged chats; the gm runtime owns it
                // for gm: (SMS) chats (G2's live increment). Fan it out so a
                // merged SMS to the actively-viewed chat isn't counted twice or
                // missed. gm applies it INLINE in its select loop (its
                // handle_command is spawned — spawning would race rapid switches).
                if let WaCommand::SetActiveChat { chat_id } = cmd {
                    if let Some(tx) = &gm_cmd_tx {
                        let _ = tx.send(WaCommand::SetActiveChat {
                            chat_id: chat_id.clone(),
                        });
                    }
                    if wa_cmd_tx
                        .send(WaCommand::SetActiveChat { chat_id })
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                // GmessagesRepair is gm-routed regardless of chat_id (it
                // has none).
                let goes_to_gm = matches!(cmd, WaCommand::GmessagesRepair)
                    || command_chat_id(&cmd)
                        .map(crate::gmessages_runtime::is_gm_chat)
                        .unwrap_or(false);
                if goes_to_gm {
                    if let Some(tx) = &gm_cmd_tx {
                        let _ = tx.send(cmd);
                    } else {
                        log::warn!(
                            "received gm-routed command but gmessages runtime is disabled — dropping"
                        );
                    }
                } else if wa_cmd_tx.send(cmd).is_err() {
                    break;
                }
            }
        });
    }

    if let Err(e) = run_inner(event_tx.clone(), &mut wa_cmd_rx).await {
        log::error!("WhatsApp runtime error: {e:#}");
        let _ = event_tx.send(WaEvent::Disconnected(e.to_string())).await;
    }
}

/// Extract the `chat_id` field from any [`WaCommand`] variant that has one,
/// so the router can decide which runtime should handle it.
fn command_chat_id(cmd: &WaCommand) -> Option<&str> {
    match cmd {
        WaCommand::SendText { chat_id, .. }
        | WaCommand::SendReply { chat_id, .. }
        | WaCommand::ResendMessage { chat_id, .. }
        | WaCommand::DeleteForEveryone { chat_id, .. }
        | WaCommand::LoadChat { chat_id, .. }
        | WaCommand::SetTyping { chat_id, .. }
        | WaCommand::MarkRead { chat_id }
        | WaCommand::SetAutoMarkRead { chat_id, .. }
        | WaCommand::ArchiveChat { chat_id, .. }
        | WaCommand::MuteChat { chat_id, .. }
        | WaCommand::PinChat { chat_id, .. }
        | WaCommand::LabelChat { chat_id, .. }
        | WaCommand::MarkUnread { chat_id }
        | WaCommand::FavoriteChat { chat_id, .. }
        | WaCommand::BlockContact { chat_id, .. }
        | WaCommand::SendReaction { chat_id, .. }
        | WaCommand::SendImage { chat_id, .. }
        | WaCommand::SendGif { chat_id, .. }
        | WaCommand::SendSticker { chat_id, .. }
        | WaCommand::SendPoll { chat_id, .. }
        | WaCommand::SendAudio { chat_id, .. }
        | WaCommand::RequestMediaDownload { chat_id, .. } => Some(chat_id),
        WaCommand::SendContact { to_chat_id, .. }
        | WaCommand::ForwardMessage { to_chat_id, .. } => Some(to_chat_id),
        _ => None,
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
            log::info!(
                "Reset app_state_versions for regular collections — next connect will pull all contact names from phone"
            );
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
                // Coalesce bursts before writing. Each queued item is the
                // FULL current in-memory history for a chat, so for any one
                // chat only the LAST snapshot matters — earlier ones are
                // strict supersets-in-time of the same file. `queue_save`
                // fires on every append (which also re-sorts), so a flurry
                // of inbound messages can stack up many saves of the same
                // chat. Since `save_messages_scoped` is a full-file
                // read-modify-write (deserialize → clone → sort → serialize),
                // collapsing N queued saves of one chat into 1 skips N-1
                // whole-file rewrites. Different chats are each kept (keyed
                // by id) and written once. Net on-disk state is identical to
                // processing every save sequentially — last-writer-wins.
                let mut latest: std::collections::HashMap<String, Vec<IncomingMessage>> =
                    std::collections::HashMap::new();
                latest.insert(chat_id, messages);
                while let Ok((cid, msgs)) = msg_save_rx.try_recv() {
                    latest.insert(cid, msgs);
                }
                for (cid, msgs) in latest {
                    // The disk-writer serves the WhatsApp runtime. Save scoped
                    // so SMS history in a merged chat is never clobbered.
                    save_messages_scoped(&cid, crate::bridge::MessageSource::WhatsApp, &msgs);
                }
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
    let _ = tokio::task::spawn_blocking(rebuild_contact_names_from_history).await;

    let state = Arc::new(Mutex::new(RuntimeState::new(save_tx, msg_save_tx)));
    // Wire the projection tap: every WA-owned summary mutation now emits an
    // authoritative ChatRowChanged onto the shared UI channel (see `emit_row`).
    state.lock().unwrap().ui_tx = Some(event_tx.clone());

    // ── Centralized LID resolver ──
    // Any handler that encounters an unresolved @lid JID sends it here.
    // The resolver batches requests, deduplicates, and runs usync in bulk.
    let (lid_resolve_tx, mut lid_resolve_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

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
                handle_wa_event(&tx, &state, &client, &lid_tx, (*event).clone()).await;
            }
        })
        .build()
        .await?;

    let client = bot.client();

    // Seed own phone/lid identity BEFORE the event loop starts. A group self-read
    // receipt can be processed during the offline flush (the is_own_read check
    // consults own_lid/own_phone) before the Connected handler — which normally
    // sets these — has run. Seeding here removes that race; Connected still
    // refreshes them.
    {
        let device = client.persistence_manager().get_device_snapshot().await;
        let strip = |raw: String| -> String {
            if let (Some(c), Some(a)) = (raw.find(':'), raw.find('@')) {
                if c < a {
                    return format!("{}{}", &raw[..c], &raw[a..]);
                }
            }
            raw
        };
        let mut s = state.lock().unwrap();
        if let Some(pn) = device.pn.as_ref() {
            s.own_phone = strip(pn.to_string());
        }
        if let Some(lid) = device.lid.as_ref() {
            s.own_lid = strip(lid.to_string());
        }
        log::info!(
            "Seeded own identity pre-run: phone={} lid={}",
            s.own_phone,
            s.own_lid
        );
    }

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

    // One process-lifetime safety poll for read-state sync. This used to be
    // spawned from every Connected event, so reconnects accumulated permanent
    // pollers (and client/state Arcs). A weak reference also lets the client
    // drop when this runtime exits.
    {
        let weak_client = Arc::downgrade(&client);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                let Some(client) = weak_client.upgrade() else {
                    break;
                };
                if let Err(e) = client
                    .resync_app_state(wacore::appstate::patch_decode::WAPatchName::RegularLow)
                    .await
                {
                    log::debug!("RegularLow safety poll error: {e:#}");
                }
                // Return unused glibc arena pages after GTK has released large
                // batches of chat widgets/textures.
                unsafe { libc::malloc_trim(0) };
            }
        });
    }

    // NOTE: A wall-clock suspend/resume detector used to live here (a 5s sleep
    // that fired a wake→force_reconnect when it overshot by >15s). It has been
    // removed because the client keepalive loop already has an identical
    // wall-clock suspend detector (src/keepalive.rs: `wall_elapsed_ms >
    // expected_ms + 15_000` → reconnect_immediately()). Running both meant a
    // single wake tore the connection down twice in quick succession — the
    // keepalive path is now the single source of truth for suspend detection.

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                // SetActiveChat is a lock-only, no-await state mutation. Applying
                // it inline (instead of tokio::spawn) preserves ordering: rapid
                // chat switches used to race each other as independent tasks, so
                // a stale `Some(A)` could land after `Some(B)` and mis-suppress
                // B's unread bumps. Everything else is spawned as before.
                if let WaCommand::SetActiveChat { chat_id } = cmd {
                    state.lock().unwrap().active_chat = chat_id;
                    continue;
                }
                let c = client.clone();
                let tx = event_tx.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_command(&c, &tx, &state, cmd).await {
                        log::warn!("Command error: {e:#}");
                    }
                });
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

fn receipt_status_for_type(receipt_type: &ReceiptType) -> Option<ReceiptStatus> {
    match receipt_type {
        ReceiptType::Read
        | ReceiptType::ReadSelf
        | ReceiptType::Played
        | ReceiptType::PlayedSelf => Some(ReceiptStatus::Read),
        ReceiptType::Delivered | ReceiptType::Sender | ReceiptType::PeerMsg => {
            Some(ReceiptStatus::Delivered)
        }
        ReceiptType::ServerError => Some(ReceiptStatus::Failed),
        ReceiptType::Retry
        | ReceiptType::EncRekeyRetry
        | ReceiptType::Inactive
        | ReceiptType::HistorySync
        | ReceiptType::Other(_) => None,
    }
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
            log::info!(
                "LID resolver: resolved {resolved}/{} JIDs",
                unresolved.len()
            );
            if resolved == 0 {
                log::warn!(
                    "LID resolver: 0 resolved — usync device query cannot map lid→pn; unresolved: {unresolved:?}"
                );
            }
            if resolved > 0 {
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
                // Proactively resolve saved contacts' LIDs (phone→LID usync) BEFORE
                // the merge, so freshly-learned mappings collapse existing phantoms
                // this same launch and future LID messages resolve on arrival.
                prewarm_contact_lids(&client_clone, &state_clone).await;
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

            // Seed the PEER's LID→phone mapping from an own-echo DM's recipient_alt
            // (the `peer_recipient_pn` attr). Without this, the chat-key resolution
            // below misses every branch and mints a phantom "+<lid digits>" chat for
            // a contact you already have. Seeded BEFORE resolution so `cached` hits.
            if info.source.is_from_me
                && raw_chat_id.ends_with("@lid")
                && let Some(ralt) = &info.source.recipient_alt
            {
                let ralt_str = ralt.to_string();
                if ralt_str.ends_with("@s.whatsapp.net") {
                    state
                        .lock()
                        .unwrap()
                        .insert_lid_phone(raw_chat_id.clone(), ralt_str);
                }
            }

            // Resolve LID chat_id to phone JID: check sender_alt first (for 1:1 DMs),
            // then cached lid_to_phone mapping. IMPORTANT: only trust the mapping
            // if the resolved phone JID actually has an existing chat — otherwise
            // stale/corrupt LID→phone mappings create phantom chats with wrong
            // phone numbers (bug where self-sent messages from phone land in a
            // completely different chat).
            let (chat_id, needs_lid_resolve) = if raw_chat_id.ends_with("@lid") {
                // For an own-echo DM the peer's phone is in recipient_alt (sender_alt
                // is None); recipient_alt is None on all inbound-DM and group paths,
                // so the `.or` is a safe no-op there.
                let alt_phone = info
                    .source
                    .sender_alt
                    .as_ref()
                    .or(info.source.recipient_alt.as_ref())
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
                    match cached
                        .as_ref()
                        .or(cached_base.as_ref())
                        .or(alt_phone.as_ref())
                    {
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
                log::info!("Queued LID resolution for self-message phantom chat: {chat_id}");
            }

            // Handle message revoke (delete for everyone) — edit_attribute tells us
            use whatsapp_rust::types::message::EditAttribute;
            let is_revoke = matches!(
                info.edit,
                EditAttribute::SenderRevoke | EditAttribute::AdminRevoke
            );
            if is_revoke {
                // Resolve the ORIGINAL message id from ProtocolMessage{type=Revoke}.key.id.
                // meta_info.target_id is never populated in the receive parser, so the
                // old fallback to msg_id targeted the revoke stanza's OWN fresh id — the
                // delete removed nothing and the bubble never updated. `base` reaches a
                // from-me echo wrapped in edited_message/DeviceSentMessage.
                let base = msg.get_base_message();
                let revoke_pm = msg
                    .protocol_message
                    .as_deref()
                    .or(base.protocol_message.as_deref());
                let target_id = info
                    .meta_info
                    .target_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .or_else(|| {
                        revoke_pm
                            .and_then(|pm| pm.key.as_ref())
                            .and_then(|k| k.id.clone())
                    })
                    .unwrap_or_else(|| msg_id.clone());
                log::info!("Revoke message received: {target_id} in {chat_id}");
                // Remove from cache (load from disk first if the chat isn't in memory,
                // so the removal persists and was_latest is correct for chats never
                // opened this session).
                let new_preview = {
                    let mut s = state.lock().unwrap();
                    if !s.history.contains_key(&chat_id) {
                        let disk_msgs = load_messages(&chat_id);
                        if !disk_msgs.is_empty() {
                            s.history.insert(chat_id.clone(), disk_msgs);
                        }
                    }
                    let mut was_latest = false;
                    if let Some(msgs) = s.history.get_mut(&chat_id) {
                        was_latest = msgs
                            .iter()
                            .max_by_key(|m| m.timestamp)
                            .map(|m| m.id == target_id)
                            .unwrap_or(false);
                        msgs.retain(|m| m.id != target_id);
                        s.queue_save_messages(&chat_id);
                    }
                    // Only touch the preview if the DELETED message was the latest;
                    // otherwise the sidebar keeps showing the true latest message.
                    if was_latest {
                        // set_chat_preview keeps the timestamp (no reorder),
                        // persists, and emits the authoritative row refresh.
                        s.set_chat_preview(&chat_id, "🚫 Message deleted");
                        Some("🚫 Message deleted".to_string())
                    } else {
                        None
                    }
                };
                let _ = tx
                    .send(WaEvent::MessageDeletedLocal {
                        chat_id,
                        msg_id: target_id,
                        new_preview,
                    })
                    .await;
                return;
            }

            // Handle message edits — update text in cache and notify UI.
            // Detect edits either via the edit attribute OR by checking
            // for protocol_message.edited_message (self-edits from phone
            // sometimes arrive without the edit attribute set).
            // Inbound edits arrive wrapped one level deeper than the top-level
            // protocol_message: Message.edited_message (FutureProofMessage) →
            // .message.protocol_message{type=MESSAGE_EDIT, key.id=ORIGINAL id,
            // edited_message=replacement}. Resolve the ProtocolMessage from BOTH
            // possible levels — reading only the top level made every peer edit
            // parse as empty text with a missing target and dropped from_me echoes.
            let edit_pm = msg.protocol_message.as_deref().or_else(|| {
                msg.edited_message
                    .as_ref()
                    .and_then(|fp| fp.message.as_deref())
                    .and_then(|m| m.protocol_message.as_deref())
            });
            let has_edited_msg = edit_pm.is_some_and(|pm| pm.edited_message.is_some());
            if matches!(info.edit, EditAttribute::MessageEdit) || has_edited_msg {
                // Target ID priority: meta_info.target_id → protocol_message.key.id
                // (the ORIGINAL message id) → fall back to msg_id (last resort).
                let target_id = info
                    .meta_info
                    .target_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .or_else(|| {
                        edit_pm
                            .and_then(|pm| pm.key.as_ref())
                            .and_then(|k| k.id.clone())
                    })
                    .unwrap_or_else(|| msg_id.clone());
                // The replacement text lives in the wrapped edited_message
                // (conversation OR extendedTextMessage.text — text_content covers both).
                let new_text = edit_pm
                    .and_then(|pm| pm.edited_message.as_deref())
                    .and_then(|em| em.text_content().map(|s| s.to_string()))
                    .or_else(|| msg.text_content().map(|s| s.to_string()))
                    .unwrap_or_default();
                log::info!(
                    "Message edit received: {target_id} in {chat_id} new_text={:?}",
                    new_text
                );
                // Never blank a bubble: an empty extraction (a media-body edit we
                // don't render, or an unparsed shape) must not overwrite the stored
                // text or emit an update.
                if new_text.is_empty() {
                    log::warn!(
                        "Message edit for {target_id} in {chat_id} had empty text — skipping (not overwriting existing message)"
                    );
                    return;
                }
                let is_latest = {
                    let mut s = state.lock().unwrap();
                    // Load history from disk if this chat isn't in memory — otherwise the
                    // edit wouldn't persist and is_latest would be a false negative for a
                    // chat not opened this session.
                    if !s.history.contains_key(&chat_id) {
                        let disk_msgs = load_messages(&chat_id);
                        if !disk_msgs.is_empty() {
                            s.history.insert(chat_id.clone(), disk_msgs);
                        }
                    }
                    let mut is_latest = false;
                    if let Some(msgs) = s.history.get_mut(&chat_id) {
                        if let Some(m) = msgs.iter_mut().find(|m| m.id == target_id) {
                            m.text = Some(new_text.clone());
                            m.is_edited = true;
                        }
                        is_latest = msgs
                            .iter()
                            .max_by_key(|m| m.timestamp)
                            .map(|m| m.id == target_id)
                            .unwrap_or(false);
                        s.queue_save_messages(&chat_id);
                    }
                    // If the edited message is the chat's latest, refresh the persisted
                    // preview too (no timestamp/unread change — edits don't reorder).
                    if is_latest {
                        s.set_chat_preview(&chat_id, &new_text);
                    }
                    is_latest
                };
                let _ = tx
                    .send(WaEvent::MessageEdited {
                        chat_id,
                        msg_id: target_id,
                        new_text,
                        is_latest,
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
                        // Persist reaction to message cache + capture the full
                        // updated reactions so the UI can rebuild (dedup/remove).
                        let updated: Option<(Vec<(String, String)>, bool)> = {
                            let mut s = state.lock().unwrap();
                            let mut out = None;
                            if let Some(msgs) = s.history.get_mut(&chat_id) {
                                let is_latest = msgs
                                    .iter()
                                    .max_by_key(|m| m.timestamp)
                                    .map(|m| m.id == target_id)
                                    .unwrap_or(false);
                                if let Some(m) = msgs.iter_mut().find(|m| m.id == target_id) {
                                    // Replace-or-remove this sender's reaction.
                                    m.reactions.retain(|(s, _)| *s != sender);
                                    if !emoji.is_empty() {
                                        m.reactions.push((sender, emoji.clone()));
                                    }
                                    out = Some((m.reactions.clone(), is_latest));
                                }
                                s.queue_save_messages(&chat_id);
                            }
                            out
                        };
                        // Emit ALWAYS (including removals — empty vec clears the row).
                        if let Some((reactions, is_latest)) = updated {
                            // Ephemeral sidebar override for a reaction on the
                            // LATEST message (only when adding, not clearing).
                            // Rendered live but never persisted — restart shows
                            // the underlying message text again (A6).
                            if is_latest && !emoji.is_empty() {
                                state
                                    .lock()
                                    .unwrap()
                                    .emit_row_ephemeral(&chat_id, &format!("Reacted {emoji}"));
                            }
                            let _ = tx
                                .send(WaEvent::ReactionUpdated {
                                    chat_id: chat_id.clone(),
                                    msg_id: target_id,
                                    reactions,
                                    is_latest,
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

            let mapped = map_message((*msg).clone(), (*info).clone());
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
                            } else if qs.starts_with('+') || qs.chars().all(|c| c.is_ascii_digit())
                            {
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
                        if !m.is_from_me {
                            s.last_incoming_msg_id
                                .insert(m.chat_id.clone(), m.id.clone());
                        }
                        if !m.sender_id.is_empty() && !m.is_from_me {
                            s.last_msg_sender
                                .insert(m.chat_id.clone(), m.sender_id.clone());
                        }
                    }
                    // Persist message and update chat summary
                    let (name_update, new_chat) = persist_new_message(&m, state);
                    if let Some(s) = new_chat {
                        // A brand-new group starts with a placeholder name (the
                        // creator/sender, since we have no subject yet). Fetch the
                        // real subject now so the header corrects within a second
                        // instead of showing the creator's name until restart.
                        let new_group_id = s.id.ends_with("@g.us").then(|| s.id.clone());
                        let _ = tx.send(WaEvent::ChatAdded(s)).await;
                        if let Some(gid) = new_group_id {
                            let c = client.clone();
                            let st = state.clone();
                            let t = tx.clone();
                            tokio::spawn(async move {
                                fetch_group_subject(&c, &st, &t, &gid).await;
                            });
                        }
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
                    if m.sender_name.contains("@lid")
                        && m.sender_id.ends_with("@lid")
                        && !m.is_from_me
                    {
                        queue_lid_resolve(&lid_resolver_tx, &m.sender_id);
                    }

                    WaEvent::MessageReceived(Box::new(m))
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
                r.source.sender
            );
            // Detect "we read on another device":
            // - ReadSelf type (DMs)
            // - Read type where sender is OUR OWN LID/phone (groups)
            let sender_raw = r.source.sender.to_string();
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
            let status = receipt_status_for_type(&r.r#type);
            if matches!(r.r#type, ReceiptType::ServerError) {
                log::warn!(
                    "WhatsApp rejected outgoing message receipt(s): chat={} ids={:?}",
                    r.source.chat,
                    r.message_ids
                );
            }
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

            if !is_read_self && status.is_some() {
                let status = status.clone().expect("checked above");
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
                // Stamp the read watermark at the RECEIPT's timestamp (when the
                // phone actually read), not the chat's stale last-message time.
                // During the offline flush a self-read receipt can arrive before
                // the messages it covers; stamping a receipt-time watermark lets
                // persist_new_message suppress those messages' unread bump when
                // they finally decode, regardless of arrival order.
                let receipt_ts = r.timestamp.timestamp();
                for cid in &actual_chats {
                    log::info!("ReadSelf: clearing unread for chat={cid} (t={receipt_ts})");
                    {
                        let mut s = state.lock().unwrap();
                        s.mark_chat_read_local(cid);
                        if receipt_ts > 0 {
                            let prev = s.receipt_watermarks.get(cid).copied().unwrap_or(0);
                            if receipt_ts > prev {
                                s.receipt_watermarks.insert(cid.clone(), receipt_ts);
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
            if let Some(status) = status {
                for msg_id in r.message_ids {
                    let _ = tx
                        .send(WaEvent::ReceiptUpdate {
                            msg_id,
                            status: status.clone(),
                        })
                        .await;
                }
            }
            return;
        }

        // Log ALL notifications for diagnostics
        Event::Notification(node) => {
            let mut attrs = node.attrs();
            if let Some(type_attr) = attrs.optional_string("type") {
                log::info!("Notification: type={} tag={}", type_attr, node.tag());
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
            // Ignore ALL full-sync mark-read/unread mutations. The app-state
            // log only records the last time the user *explicitly* read or
            // unread a chat — it is stale the moment new messages arrive.
            // Replaying it on every startup zeroed the unread badge of every
            // chat the user had ever read, including ones with fresh unread
            // messages ("0 unread on every reopen"). The history sync's
            // `conv.unread_count` is the authoritative current read state —
            // let it win. Live (non-full-sync) MarkChatAsRead updates and
            // ReadSelf receipts still clear badges in real time.
            if update.from_full_sync {
                return;
            }
            if is_read {
                {
                    let mut s = state.lock().unwrap();
                    s.mark_chat_read_local(&chat_id);
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

        Event::HistorySync(lazy_sync) => {
            let Some(history_sync) = lazy_sync.get() else {
                return;
            };
            for conv in &history_sync.conversations {
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
                    {
                        let mut s = state.lock().unwrap();
                        s.insert_lid_phone(chat_id.clone(), pn_jid.clone());
                        // Also store contact name under phone JID if we have one for the LID
                        if let Some(lid_name) = s.contact_names.get(&chat_id).cloned() {
                            if !s.contact_names.contains_key(&pn_jid) {
                                s.contact_names.insert(pn_jid.clone(), lid_name);
                            }
                        }
                    }
                    // The (lid, pn) pair here is real — feed it to the CORE LID-PN
                    // cache too so the client's own resolver learns it (not just
                    // the desktop maps). Non-fatal, spawned off the event loop.
                    let bare_user = |jid: &str| -> String {
                        jid.split('@')
                            .next()
                            .unwrap_or(jid)
                            .split(':')
                            .next()
                            .unwrap_or(jid)
                            .trim_start_matches('+')
                            .to_string()
                    };
                    let lid_user = bare_user(&chat_id);
                    let phone_user = bare_user(&pn_jid);
                    if !lid_user.is_empty() && !phone_user.is_empty() {
                        let client = client.clone();
                        tokio::spawn(async move {
                            client.learn_lid_pn(&lid_user, &phone_user).await;
                        });
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
                        save_messages_scoped(
                            &chat_id,
                            crate::bridge::MessageSource::WhatsApp,
                            &disk_msgs,
                        );
                        (preview, ts, new_msgs)
                    }
                };

                // Push new sync messages to the UI as live messages
                // (so self-messages from other devices and missed messages appear immediately)
                for m in &new_messages {
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(m.clone()))).await;
                }

                // Seed the read-receipt anchor from history sync, mirroring the
                // live handler (~2935). Without this, MarkRead has no
                // last_incoming_msg_id to ack after a restart, so re-opening a
                // synced chat can't send a read receipt / clear it on the phone.
                // Use the newest incoming (non-from-me) message in this batch, but
                // only fill an EMPTY slot — never clobber a fresher value already
                // stored by a concurrent live MessageReceived for this chat.
                if let Some(anchor) = sync_messages
                    .iter()
                    .filter(|m| !m.is_from_me)
                    .max_by_key(|m| m.timestamp)
                {
                    let mut s = state.lock().unwrap();
                    if !s.last_incoming_msg_id.contains_key(&chat_id) {
                        s.last_incoming_msg_id
                            .insert(chat_id.clone(), anchor.id.clone());
                        if !anchor.sender_id.is_empty() {
                            s.last_msg_sender
                                .insert(chat_id.clone(), anchor.sender_id.clone());
                        }
                    }
                }

                // Respect the server's unread count when present.
                // When absent (None), preserve the existing count if the chat
                // is already known, otherwise count incoming non-from-me messages
                // so fresh syncs don't incorrectly mark everything as read.
                // Whether the server explicitly told us the unread count. If so,
                // it's authoritative (a phone-side read arrives as Some(0)) and
                // upsert must trust it over the local stale-preserve heuristic.
                let unread_authoritative = conv.unread_count.is_some();
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
                    auto_mark_read: false,
                };

                if unread_authoritative {
                    persist_chat_authoritative(state, summary);
                } else {
                    // History-sync reseed with no explicit server count: still a
                    // reseed path, so the read-watermark defense must apply
                    // (authoritative_unread == false keeps the stale-preserve
                    // heuristic, from_reseed == true enables the clobber).
                    state.lock().unwrap().upsert_chat(summary, false, true);
                }

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

                // If a history-synced group ended up with a raw JID or a
                // "Alice, Bob, …" participant-name placeholder (no subject in
                // the proto), fetch the real subject now — mirroring the
                // live-new-group path — so it doesn't stay on the placeholder
                // until the chat is reopened. fetch_group_subject only renames
                // when the server returns a non-empty subject, so this is a
                // safe no-op for correctly-named groups.
                if let Some(s) = &persisted {
                    if s.id.ends_with("@g.us") {
                        let n = &s.name;
                        let looks_raw = n.contains('@')
                            || (n.chars().all(|c| c.is_ascii_digit() || c == '+') && n.len() > 4);
                        // group_name_from_history placeholders end with "…"
                        // ("Karim Valji, Lorne, …"); treat those as unresolved.
                        let looks_placeholder = n.ends_with('\u{2026}');
                        if looks_raw || looks_placeholder {
                            let c = client.clone();
                            let st = state.clone();
                            let t = tx.clone();
                            let gid = s.id.clone();
                            tokio::spawn(async move {
                                fetch_group_subject(&c, &st, &t, &gid).await;
                            });
                        }
                    }
                }

                if let Some(s) = persisted {
                    let _ = tx.send(WaEvent::ChatAdded(s)).await;
                }
            }
            return;
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
                // and rename any already-loaded chat. push_name is a LOWER
                // priority source than the user's phonebook name — record_contact_name
                // refuses the write (returns false) when a phonebook entry already
                // exists, and in that case we must NOT emit ChatNameUpdated (which
                // routes through the authoritative path and would clobber the UI).
                let (wrote, names_snapshot) = {
                    let mut s = state.lock().unwrap();
                    let wrote = s.record_contact_name(&chat_id, &name, None, NameSource::PushName);
                    (wrote, s.contact_names.clone())
                };
                if wrote {
                    tokio::task::spawn_blocking(move || save_contact_names(&names_snapshot));
                    WaEvent::ChatNameUpdated { chat_id, name }
                } else {
                    return;
                }
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
                // Phonebook is the highest-priority source (see NameSource) —
                // it always wins over push_names captured from messages.
                // 1. Primary LID
                s.record_contact_name(&lid_jid, &name, None, NameSource::Phonebook);
                // 2. Explicit phone JID from ContactAction
                let mut phone = phone_jid.clone();
                if !phone.is_empty() {
                    s.record_contact_name(&phone, &name, None, NameSource::Phonebook);
                    if lid_jid.ends_with("@lid") {
                        s.insert_lid_phone(lid_jid.clone(), phone.clone());
                    }
                }
                // 3. Phone derived from lid_to_phone mapping (for LidContactAction
                //    which has no pn_jid field — previously we stored only under
                //    LID, leaving phone-JID chats unresolved)
                if phone.is_empty() && lid_jid.ends_with("@lid") {
                    if let Some(mapped) = s.lid_to_phone.get(&lid_jid).cloned() {
                        s.record_contact_name(&mapped, &name, None, NameSource::Phonebook);
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
                    s.record_contact_name(&lid_base, &name, None, NameSource::Phonebook);
                }
                if !phone.is_empty() {
                    let phone_base = strip_dev(&phone);
                    if phone_base != phone {
                        s.record_contact_name(&phone_base, &name, None, NameSource::Phonebook);
                    }
                }
                (s.contact_names.clone(), phone)
            };
            tokio::task::spawn_blocking(move || save_contact_names(&names_snapshot));

            // Feed the (lid, pn) pair the app-state carries into the CORE LID-PN
            // cache too — previously we only wrote the desktop-side maps, so the
            // client's own resolver never learned it. `resolved_phone` is the
            // phone JID (explicit pn_jid or derived from lid_to_phone). Extract
            // the bare user parts; skip if either is missing. Non-fatal: spawn
            // it so a persist error can't wedge the event loop.
            if lid_jid.ends_with("@lid") && !resolved_phone.is_empty() {
                let bare_user = |jid: &str| -> String {
                    jid.split('@')
                        .next()
                        .unwrap_or(jid)
                        .split(':')
                        .next()
                        .unwrap_or(jid)
                        .trim_start_matches('+')
                        .to_string()
                };
                let lid_user = bare_user(&lid_jid);
                let phone_user = bare_user(&resolved_phone);
                if !lid_user.is_empty() && !phone_user.is_empty() {
                    let client = client.clone();
                    tokio::spawn(async move {
                        client.learn_lid_pn(&lid_user, &phone_user).await;
                    });
                }
            }

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
                // Show the user's own account as "You" instead of their number.
                let digits =
                    |j: &str| -> String { j.chars().filter(|c| c.is_ascii_digit()).collect() };
                let rd = digits(&resolved);
                let raw_d = digits(&raw);
                if !rd.is_empty()
                    && (rd == digits(&s.own_phone)
                        || rd == digits(&s.own_lid)
                        || raw_d == digits(&s.own_lid))
                {
                    return "You".to_string();
                }
                resolve_sender_name(&s, &resolved)
            };

            // Resolve the actor (admin/user who triggered the change) to a name.
            // `participant` may be @lid — resolve() already runs it through
            // lid_to_phone + resolve_sender_name and maps self→"You".
            let actor_name: Option<String> = update.participant.as_ref().map(|p| resolve(p));
            // Digits of the actor JID (both raw and phone-mapped) so we can tell
            // a self-leave ("Bob left") from an admin-kick ("Alice removed Bob").
            let actor_digits: Option<String> = update.participant.as_ref().map(|p| {
                p.to_string()
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect()
            });
            let same_person = |info: &wacore::stanza::groups::GroupParticipantInfo| -> bool {
                let Some(ad) = actor_digits.as_ref() else {
                    return false;
                };
                if ad.is_empty() {
                    return false;
                }
                let jd: String = info
                    .jid
                    .to_string()
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect();
                let pd: String = info
                    .phone_number
                    .as_ref()
                    .map(|p| {
                        p.to_string()
                            .chars()
                            .filter(|c| c.is_ascii_digit())
                            .collect()
                    })
                    .unwrap_or_default();
                &jd == ad || (!pd.is_empty() && &pd == ad)
            };

            let text = match &update.action {
                GroupNotificationAction::Add { participants, .. } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    match &actor_name {
                        Some(actor) => format!("{actor} added {}", names.join(", ")),
                        None => format!("Added {}", names.join(", ")),
                    }
                }
                GroupNotificationAction::Remove { participants, .. } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    // A member removing *themselves* is a voluntary leave; anyone
                    // else removing them is an admin kick ("Alice removed Bob").
                    let self_leave = participants.len() == 1
                        && participants
                            .first()
                            .map(|p| same_person(p))
                            .unwrap_or(false);
                    if self_leave {
                        format!("{} left", names.join(", "))
                    } else if let Some(actor) = &actor_name {
                        format!("{actor} removed {}", names.join(", "))
                    } else {
                        format!("{} was removed", names.join(", "))
                    }
                }
                GroupNotificationAction::Promote { participants } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    match &actor_name {
                        Some(actor) => format!("{actor} made {} an admin", names.join(", ")),
                        None => format!("{} is now an admin", names.join(", ")),
                    }
                }
                GroupNotificationAction::Demote { participants } => {
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    match &actor_name {
                        Some(actor) => format!("{actor} removed {} as admin", names.join(", ")),
                        None => format!("{} is no longer an admin", names.join(", ")),
                    }
                }
                GroupNotificationAction::Modify { participants } => {
                    // wacore documents <modify> as "Member changed phone number".
                    let names: Vec<String> = participants.iter().map(|p| resolve(&p.jid)).collect();
                    if names.is_empty() {
                        "A member changed their phone number".to_string()
                    } else {
                        format!("{} changed their phone number", names.join(", "))
                    }
                }
                GroupNotificationAction::Subject { subject, .. } => {
                    // Falling through (not returning early) lets the member-refresh
                    // spawn below also rename the chat live via get_metadata.
                    format!("changed the group name to \u{201c}{subject}\u{201d}")
                }
                _ => {
                    log::debug!("Unhandled group notification action");
                    return;
                }
            };

            log::info!("GroupUpdate: {text} in {chat_id}");

            let now = update.timestamp.timestamp();
            let sys_msg = IncomingMessage {
                media_download: None,
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

            // Persist through the message choke point so the chat summary
            // (preview + timestamp) updates and emits a ChatRowChanged — a bare
            // history push froze the row on "You added X" events. Unread stays
            // suppressed (persist_new_message skips the bump for is_system_message).
            persist_new_message(&sys_msg, state);
            let _ = tx.send(WaEvent::MessageReceived(Box::new(sys_msg))).await;

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
                    // Update the group NAME from the fresh subject. A newly
                    // created group (or one we were just added to) initially
                    // shows a placeholder derived from the creator/sender —
                    // group_name_from_history picks the only person who has
                    // spoken. Previously that stuck until the next restart's
                    // group-name refresh; set it now that we have the real
                    // subject in hand.
                    if !meta.subject.is_empty() {
                        s.lock().unwrap().rename_chat(&cid, &meta.subject);
                        let _ = t
                            .send(WaEvent::ChatNameUpdated {
                                chat_id: cid.clone(),
                                name: meta.subject.clone(),
                            })
                            .await;
                    }
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
                                is_admin: p.is_admin(),
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
                                if let Some(phone_jid) = c.resolve_lid_to_phone_jid(lid_str).await {
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
                                    if m.name.contains("@lid") || m.name.contains("@s.whatsapp.net")
                                    {
                                        let st = s.lock().unwrap();
                                        m.name = resolve_sender_name(&st, &m.jid);
                                    }
                                }
                                // Live-refresh any open bubbles from these senders
                                // so a participant that showed a raw number now
                                // shows their real name without reopening the chat.
                                for m in &members {
                                    if !m.name.contains('@') {
                                        let _ = t
                                            .send(WaEvent::SenderNameResolved {
                                                chat_id: cid.clone(),
                                                sender_id: m.jid.clone(),
                                                name: m.name.clone(),
                                            })
                                            .await;
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

    // Check cache miss WITHOUT disk I/O (cheap lock).
    let needs_disk_load;
    let was_new;
    {
        let s = state.lock().unwrap();
        was_new = !s.chats.iter().any(|c| c.id == chat_id);
        needs_disk_load = !s.history.contains_key(&chat_id);
    }

    // Do the disk read OUTSIDE the lock — this was blocking every UI event.
    // With 1000+ chats, every incoming message blocked the global state Mutex
    // for the duration of a bincode deserialize, causing chat-switch lag.
    let disk_msgs = if needs_disk_load {
        Some(load_messages(&chat_id))
    } else {
        None
    };

    // Update in-memory cache (fast) and queue async disk write (non-blocking).
    let (
        existing_name,
        existing_unread,
        existing_is_muted,
        existing_is_pinned,
        existing_is_archived,
        existing_is_favorite,
        existing_label,
        msg_is_new,
    ) = {
        let mut s = state.lock().unwrap();

        // Insert any disk-loaded messages now (still fast — just a HashMap insert).
        if let Some(loaded) = disk_msgs {
            // Re-check; another concurrent path may have populated meanwhile.
            if !s.history.contains_key(&chat_id) {
                s.history.insert(chat_id.clone(), loaded);
            }
        }
        s.touch_history(&chat_id);
        s.evict_old_histories();

        let history = s.history.entry(chat_id.clone()).or_default();
        let msg_is_new = !history.iter().any(|x| x.id == m.id);
        if msg_is_new {
            // Insert in sorted position instead of push+full-resort. History is
            // already sorted by timestamp; the common case (a message newer than
            // everything present) is an O(1) push, and an out-of-order message
            // (older, e.g. from history backfill) is a single binary-search
            // insert — avoiding the O(n log n) re-sort of the whole Vec on
            // every append in a large, active group.
            if history
                .last()
                .map(|last| m.timestamp >= last.timestamp)
                .unwrap_or(true)
            {
                history.push(m.clone());
            } else {
                let idx = history.partition_point(|x| x.timestamp <= m.timestamp);
                history.insert(idx, m.clone());
            }
        }

        // Retransmitted messages do not change the cache, so avoid cloning and
        // queueing the full history again for those common duplicate events.
        if msg_is_new {
            s.queue_save_messages(&chat_id);
        }

        // Read chat info AND compute the unread count to PERSIST. The runtime now
        // owns the count so it survives restart (it used to be carried unchanged,
        // leaving the persisted value stale while only the UI badge incremented).
        // Bump for a genuinely-new incoming message unless the chat is the one
        // being viewed or is auto-mark-read (both get cleared to 0 anyway).
        let active = s.active_chat.clone();
        // A self-read receipt (read on the phone) may have already arrived for this
        // chat, ahead of this message in the offline backlog. If so, this message
        // is at-or-before the read point and must NOT bump unread.
        let receipt_wm = s.receipt_watermarks.get(&chat_id).copied().unwrap_or(0);
        if let Some(ex) = s.chats.iter().find(|c| c.id == chat_id) {
            let should_bump = msg_is_new
                && !m.is_from_me
                && active.as_deref() != Some(chat_id.as_str())
                && !ex.auto_mark_read
                && !m.is_system_message
                && m.timestamp > receipt_wm;
            let unread = if should_bump {
                ex.unread_count.saturating_add(1)
            } else {
                ex.unread_count
            };
            (
                Some(ex.name.clone()),
                unread,
                ex.is_muted,
                ex.is_pinned,
                ex.is_archived,
                ex.is_favorite,
                ex.label.clone(),
                msg_is_new,
            )
        } else {
            // Brand-new chat (not yet in self.chats). A first message from a new
            // contact while the user is away must persist unread=1, or the chat
            // shows as already-read after restart. Count it unless it's ours or
            // the chat is the one actively being viewed.
            let unread = (!m.is_from_me
                && active.as_deref() != Some(chat_id.as_str())
                && m.timestamp > receipt_wm) as u32;
            (None, unread, false, false, false, false, None, msg_is_new)
        }
    };

    // Resolve display name
    //
    // A name is "JID-like" / phone-fallback if it's NOT a real human-friendly
    // name. This includes:
    //   - raw JID strings ("14168235004@s.whatsapp.net")
    //   - bare digit strings ("14168235004")
    //   - the +country-code phone format produced by `display_name_from_jid`
    //     ("+14168235004") — this was the missed case that left contacts
    //     showing as phone numbers until app restart, because push_names
    //     arriving via MessageReceived never replaced the existing phone
    //     fallback.
    let name_looks_like_phone_fallback = |s: &str| -> bool {
        if s.contains('@') {
            return true;
        }
        // "+1234567890" — starts with +, rest digits, length > 6 total
        if let Some(rest) = s.strip_prefix('+') {
            if rest.chars().all(|c| c.is_ascii_digit()) && rest.len() >= 6 {
                return true;
            }
        }
        // "1234567890" — pure digits
        if s.chars().all(|c| c.is_ascii_digit()) && s.len() > 6 {
            return true;
        }
        false
    };
    let (resolved_name, name_is_new) = if let Some(ref existing_name) = existing_name {
        let name_looks_like_jid = name_looks_like_phone_fallback(existing_name);
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

    // Producer-side preview: group prefixes + @mention resolution baked in,
    // so the persisted preview matches what the sidebar renders (A7).
    let preview = row_preview(m, is_group);
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
        auto_mark_read: false,
    };
    persist_chat(state, summary);

    // WhatsApp semantics: a message the user sent (including the echo of a message
    // sent from their phone during an offline window) marks that chat read. Do this
    // AFTER persist_chat so mark_chat_read_local runs POST-upsert — otherwise the
    // upsert stale-preserve heuristic (see upsert_chat) would restore the old
    // nonzero unread. mark_chat_read_local also stamps the read watermark at the
    // now-updated chat timestamp, so a later stale reseed carrying this message's
    // timestamp cannot resurrect the badge. System messages are excluded.
    if m.is_from_me && msg_is_new && !m.is_system_message {
        state.lock().unwrap().mark_chat_read_local(&chat_id);
    }

    if name_is_new && !is_group {
        // resolved_name here is a sender push_name derived from the message —
        // lower priority than the phonebook, so it must not clobber a saved
        // ContactUpdate name.
        let names_snapshot = {
            let mut s = state.lock().unwrap();
            s.record_contact_name(&chat_id, &resolved_name, None, NameSource::PushName);
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

/// Build the sidebar-row preview for a message, producer-side. This is the
/// authoritative preview text — the UI renders it VERBATIM. Moves the group
/// "You: " / "{sender}: " prefixing and the `@<jid-digits>` → `@Name` mention
/// resolution that used to live in chat_list.rs's now-deleted
/// `update_last_message` onto the runtime, so restart-loaded previews finally
/// match live-rendered ones (wa_chats.bin previews now carry prefixes).
///
/// `crate::contacts::global()` is a cross-thread global directory, safe to call
/// from any runtime thread.
pub fn row_preview(m: &IncomingMessage, is_group: bool) -> String {
    let content = media_preview(m);
    // Resolve raw `@<jid-digits>` mentions to `@Name`. Message bodies carry
    // mentions in the protocol form; the bubble renderer resolves them and the
    // preview must too, else "@agnes" shows as a bare digit blob.
    let clean = if content.contains('@') {
        let mut c = content.clone();
        for word in content.split_whitespace() {
            if word.starts_with('@')
                && word.len() > 4
                && word[1..]
                    .chars()
                    .next()
                    .map(|ch| ch.is_ascii_digit())
                    .unwrap_or(false)
            {
                let jid_part = &word[1..];
                let replacement = crate::contacts::global()
                    .lookup(jid_part)
                    .filter(|n| !n.is_empty() && !n.contains('@'))
                    .map(|n| format!("@{n}"))
                    .unwrap_or_else(|| "@user".to_string());
                c = c.replace(word, &replacement);
            }
        }
        c
    } else {
        content
    };
    if is_group && !m.is_from_me && !m.sender_name.is_empty() {
        format!("{}: {clean}", m.sender_name)
    } else if is_group && m.is_from_me {
        format!("You: {clean}")
    } else {
        clean
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
        WaCommand::RequestMediaDownload { chat_id, msg_id } => {
            // On-demand download: look up the stored message's download keys,
            // media type, and filename, then run the same download path used by
            // the auto-download trigger. Lets the user fetch attachments that were
            // never auto-downloaded (history-synced, failed, or skipped media).
            let found = {
                let s = state.lock().unwrap();
                s.history.get(&chat_id).and_then(|msgs| {
                    msgs.iter().find(|m| m.id == msg_id).map(|m| {
                        (
                            m.media_download.clone(),
                            m.media_type.clone(),
                            m.media_filename.clone(),
                        )
                    })
                })
            };
            let Some((keys, media_type, filename)) = found else {
                log::warn!("RequestMediaDownload: msg {msg_id} not found in chat {chat_id}");
                return Ok(());
            };
            let Some(keys) = keys else {
                log::warn!(
                    "RequestMediaDownload: msg {msg_id} has no download keys — re-send required"
                );
                return Ok(());
            };
            let Some(dl) = pending_from_keys(&keys, media_type.as_ref(), filename) else {
                log::warn!("RequestMediaDownload: msg {msg_id} has keys but no usable media_type");
                return Ok(());
            };
            log::info!("On-demand media download requested: msg={msg_id} chat={chat_id}");
            execute_media_download(client.clone(), tx.clone(), state, msg_id, chat_id, dl).await;
        }
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
                    let real_id = real_id.message_id;
                    // Persist sent message to cache so it survives chat switches
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let sent_msg = IncomingMessage {
                        media_download: None,
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
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sent_msg))).await;
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
                    let real_id = real_id.message_id;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let sent_msg = IncomingMessage {
                        media_download: None,
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
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sent_msg))).await;
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
                    let new_id = new_id.message_id;
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
            let new_preview = {
                let mut s = state.lock().unwrap();
                let mut was_latest = false;
                if let Some(history) = s.history.get_mut(&chat_id) {
                    was_latest = history
                        .iter()
                        .max_by_key(|m| m.timestamp)
                        .map(|m| m.id == msg_id)
                        .unwrap_or(false);
                    history.retain(|m| m.id != msg_id);
                    s.queue_save_messages(&chat_id);
                }
                if was_latest {
                    // Preview → deleted stamp; timestamp/sort untouched (A5).
                    s.set_chat_preview(&chat_id, "🚫 Message deleted");
                    Some("🚫 Message deleted".to_string())
                } else {
                    None
                }
            };
            let _ = tx
                .send(WaEvent::MessageDeletedLocal {
                    chat_id: chat_id.clone(),
                    msg_id: msg_id.clone(),
                    new_preview,
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
            // Fire presence subscribe in the background — chat switch must NOT
            // wait for a network round-trip. Typing notifications start working
            // ~50ms later, which is fine.
            {
                if let Ok(jid) = chat_id.parse::<Jid>() {
                    let c = client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = c.presence().subscribe(&jid).await {
                            log::debug!("Presence subscribe failed: {e:#}");
                        }
                    });
                }
            }

            // RECONCILE gm-written disk messages into the in-memory cache.
            // The gmessages runtime appends incoming SMS/MMS straight to this
            // chat's disk file but can't update our `s.history` cache (separate
            // thread). The fast path below serves the cached last-50 without
            // touching disk, so those messages would be invisible until an app
            // restart. For any chat the gm runtime flagged dirty, merge the
            // disk file into the cache now — union by id, never dropping
            // anything already in memory (e.g. a just-arrived WhatsApp message
            // not yet flushed). Only fires for flagged chats, so the common
            // chat-switch stays disk-free.
            if take_gm_dirty(&chat_id) {
                let cid = chat_id.clone();
                let disk_msgs = tokio::task::spawn_blocking(move || load_messages(&cid))
                    .await
                    .unwrap_or_default();
                if !disk_msgs.is_empty() {
                    let mut s = state.lock().unwrap();
                    let hist = s.history.entry(chat_id.clone()).or_default();
                    let have: std::collections::HashSet<String> =
                        hist.iter().map(|m| m.id.clone()).collect();
                    let mut added = 0usize;
                    for m in disk_msgs {
                        if !have.contains(&m.id) {
                            hist.push(m);
                            added += 1;
                        }
                    }
                    if added > 0 {
                        hist.sort_by_key(|m| m.timestamp);
                        log::info!(
                            "LoadChat {chat_id}: reconciled {added} disk-only gmessages message(s) into history cache"
                        );
                    }
                }
            }

            // FAST PATH: only clone the last 50 messages from cache, not the
            // entire history. For a 5000-msg chat the previous code allocated
            // and copied a 5MB Vec on every chat switch.
            let cached_last_50 = {
                let s = state.lock().unwrap();
                s.history.get(&chat_id).map(|h| {
                    let start = h.len().saturating_sub(50);
                    h[start..].to_vec()
                })
            };

            let mut all_messages = if let Some(msgs) = cached_last_50 {
                state.lock().unwrap().touch_history(&chat_id);
                msgs
            } else {
                // Cache miss — load from disk OFF the async thread
                let cid = chat_id.clone();
                let disk_msgs = tokio::task::spawn_blocking(move || load_messages(&cid))
                    .await
                    .unwrap_or_default();
                // Populate cache (full history kept) + take last 50 for display
                let last_50 = {
                    let mut s = state.lock().unwrap();
                    let start = disk_msgs.len().saturating_sub(50);
                    let last_50 = disk_msgs[start..].to_vec();
                    s.history.insert(chat_id.clone(), disk_msgs);
                    s.touch_history(&chat_id);
                    s.evict_old_histories();
                    last_50
                };
                last_50
            };

            // Push_name learning — REMOVED from hot path. The startup scan in
            // rebuild_contact_names_from_history already covers the entire
            // message corpus once. Live messages add new names via the message
            // handler. Re-scanning every chat switch was wasted work.

            // Phase 2 merge: if this WhatsApp chat has a paired gmessages
            // conversation, pull in the SMS messages from gm_<conv>.bin so
            // they interleave with WhatsApp messages.
            if let Some(gm_chat_id) = crate::contacts::global().other_chat_id(&chat_id, "gmessages")
            {
                let gm_msgs = {
                    let cid = gm_chat_id.clone();
                    tokio::task::spawn_blocking(move || load_messages(&cid))
                        .await
                        .unwrap_or_default()
                };
                if !gm_msgs.is_empty() {
                    log::debug!(
                        "LoadChat {chat_id}: merging {} gm messages from {gm_chat_id}",
                        gm_msgs.len()
                    );
                    let have: std::collections::HashSet<&str> =
                        all_messages.iter().map(|m| m.id.as_str()).collect();
                    let new: Vec<_> = gm_msgs
                        .into_iter()
                        .filter(|m| !have.contains(m.id.as_str()))
                        .collect();
                    all_messages.extend(new);
                }
            }

            // Filter for display + sort
            all_messages.retain(|m| {
                m.text.is_some() || m.media_type.is_some() || m.media_caption.is_some()
            });
            all_messages.sort_by_key(|m| m.timestamp);
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
                        // Rebuild the preview with the now-resolved sender name
                        // (and @mention resolution) via the shared producer.
                        // set_chat_preview persists + emits ChatRowChanged; the
                        // old ChatPreviewUpdated emission is retired.
                        let preview = row_preview(last, true);
                        state.lock().unwrap().set_chat_preview(&chat_id, &preview);
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
                let _ = tx
                    .send(WaEvent::MessagePinned {
                        chat_id: chat_id.clone(),
                        msg_id,
                    })
                    .await;
            }

            // Async LID→phone resolution for unresolved group participant senders.
            // Uses usync (device-list query) which does a network round-trip and
            // persists mappings. The resolved names land in the cache so
            // they appear correctly the next time this chat is loaded.
            //
            // We deliberately do NOT re-send a HistoryMessages event for
            // the resolved names: the UI's append_bubble_to_inner_at dedups
            // by msg.id and would skip the re-render anyway, but the
            // redundant load_history call was tickling a GTK4 GL renderer
            // paint bug that left group-chat bubbles invisible until the
            // user hovered over the message pane.
            if !unresolved_lids.is_empty() && chat_id.ends_with("@g.us") {
                log::info!(
                    "LoadChat {chat_id}: {} unresolved LID senders, triggering usync resolution",
                    unresolved_lids.len()
                );
                let client_c = client.clone();
                let state_c = state.clone();
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
                                    if let Some(name) = s.contact_names.get(lid_str).cloned() {
                                        if !s.contact_names.contains_key(&phone_jid) {
                                            s.contact_names.insert(phone_jid.clone(), name);
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
                                // Apply resolved names to cached messages so
                                // the next load of this chat shows them
                                // correctly. Do NOT re-fire HistoryMessages.
                                let resolutions: HashMap<String, String> = {
                                    let s = state_c.lock().unwrap();
                                    unresolved_lids
                                        .iter()
                                        .map(|lid| {
                                            let name = resolve_sender_name(&s, lid);
                                            (lid.clone(), name)
                                        })
                                        .collect()
                                };
                                let mut s = state_c.lock().unwrap();
                                if let Some(msgs) = s.history.get_mut(&chat_id_c) {
                                    for m in msgs.iter_mut() {
                                        if let Some(resolved) = resolutions.get(&m.sender_id) {
                                            if !resolved.contains("@lid")
                                                && *resolved != m.sender_name
                                            {
                                                m.sender_name = resolved.clone();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("LoadChat {chat_id_c}: usync LID resolution failed: {e:#}");
                        }
                    }
                });
            }
        }

        WaCommand::SetActiveChat { chat_id } => {
            state.lock().unwrap().active_chat = chat_id;
        }

        // INTERNAL (gm→WA): an SMS/MMS landed on / was sent from / was reacted-to
        // on a chat merged into a WhatsApp row. The WA runtime is the sole owner
        // of non-gm summaries, so the gm thread routes the update here instead of
        // writing wa_chats.bin behind our back (which raced the save_tx flusher).
        WaCommand::TouchChatSummary {
            chat_id,
            preview,
            timestamp,
            is_from_me,
            ephemeral,
        } => {
            // Ephemeral (reaction preview): render-only, NO state mutation, NO
            // save — restart intentionally shows the underlying message again.
            if ephemeral {
                state.lock().unwrap().emit_row_ephemeral(&chat_id, &preview);
            } else {
                let mut s = state.lock().unwrap();
                // Apply only to an EXISTING WA row (a merged chat already has one);
                // never create a row from a gm touch. Monotonic guards mirror
                // upsert_chat: timestamp never moves backward, an empty preview
                // never clobbers a non-empty one. Same-second updates still
                // refresh the text (parity with the retired touch_wa_chat_preview,
                // but not dropping a merged send echo that shares a second).
                let mut applied = false;
                let active = s.active_chat.clone();
                let wm = s.read_watermarks.get(&chat_id).copied().unwrap_or(0);
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    if timestamp > c.timestamp {
                        c.timestamp = timestamp;
                        if !preview.is_empty() {
                            c.last_message = preview;
                        }
                        applied = true;
                    } else if timestamp == c.timestamp
                        && !preview.is_empty()
                        && c.last_message != preview
                    {
                        c.last_message = preview;
                        applied = true;
                    }
                    // Unread accounting for an incoming (not-from-me) merged SMS
                    // to a background chat past its read watermark. A sent one
                    // (is_from_me) clears the badge via mark_chat_read_local below.
                    if applied
                        && !is_from_me
                        && active.as_deref() != Some(chat_id.as_str())
                        && timestamp > wm
                    {
                        c.unread_count = c.unread_count.saturating_add(1);
                    }
                }
                if applied {
                    s.chats.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                    let _ = s.save_tx.send(s.chats.clone());
                    // is_from_me: mark read (zeros badge + stamps watermark).
                    // That emit is conditional on a badge change, so ALSO emit
                    // the row unconditionally here — the preview/timestamp moved
                    // even when the badge was already 0.
                    if is_from_me {
                        s.mark_chat_read_local(&chat_id);
                    }
                    s.emit_row(&chat_id);
                }
            }
        }

        WaCommand::MarkRead { chat_id } => {
            // Persist the LOCAL read state for ANY chat (WhatsApp, SMS/gm, or
            // merged) up front, before any early return. This zeros the badge
            // and stamps a read watermark so a later reconnect reseed cannot
            // resurrect it — the core of the "chats revert to unread after
            // reboot/suspend" fix. gm chat_ids (`gm:N`) stop after this; their
            // server-side read is handled by the gmessages runtime's MarkRead.
            // Capture the previous watermark BEFORE mark_chat_read_local bumps it,
            // so the read-receipt collection below knows which incoming messages
            // were still unread (everything newer-or-equal to it).
            let prev_wm = {
                let mut s = state.lock().unwrap();
                let prev = s.read_watermarks.get(&chat_id).copied().unwrap_or(0);
                s.mark_chat_read_local(&chat_id);
                prev
            };
            let _ = tx
                .send(WaEvent::ChatReadOnOtherDevice {
                    chat_id: chat_id.clone(),
                })
                .await;

            // The rest is WhatsApp-protocol read sync, valid only for real WA
            // JIDs. TWO mechanisms needed for cross-device read sync:
            //   1. <receipt type="read"> — blue tick to sender
            //   2. markChatAsRead app state mutation — syncs to our other devices
            let Ok(jid) = chat_id.parse::<Jid>() else {
                return Ok(());
            };

            // Mechanism 2: App state sync (durable, works across device restarts)
            if let Err(e) = client
                .chat_actions()
                .mark_chat_as_read(&jid, true, None)
                .await
            {
                log::debug!("mark_chat_as_read (app state) failed: {e:#}");
            }

            // Mechanism 1: Read receipts to the sender(s). Ack EVERY unread
            // incoming message id since the previous watermark — not just the
            // newest. The phone dismisses its system notification per message id,
            // so a single-id receipt left the other notified messages stuck in the
            // Android notification tray. Official clients ack all unread ids in one
            // <receipt ...><list><item id=.../></list></receipt>.
            //
            // `>=` prev_wm (not `>`) is intentional: the watermark is
            // second-granularity and re-acking an already-read id is idempotent.
            let mut unread: Vec<(String, String)> = {
                let s = state.lock().unwrap();
                s.history
                    .get(&chat_id)
                    .map(|msgs| {
                        msgs.iter()
                            .filter(|m| !m.is_from_me && m.timestamp >= prev_wm)
                            .map(|m| (m.sender_id.clone(), m.id.clone()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            };
            // History LRU may have evicted this chat — fall back to disk (outside
            // the state lock).
            if unread.is_empty() {
                unread = load_messages(&chat_id)
                    .iter()
                    .filter(|m| !m.is_from_me && m.timestamp >= prev_wm)
                    .map(|m| (m.sender_id.clone(), m.id.clone()))
                    .collect();
            }
            // Ultimate fallback: the single last-incoming anchor.
            if unread.is_empty() {
                let (last_id, last_sender) = {
                    let s = state.lock().unwrap();
                    (
                        s.last_incoming_msg_id.get(&chat_id).cloned(),
                        s.last_msg_sender.get(&chat_id).cloned(),
                    )
                };
                if let Some(msg_id) = last_id {
                    unread.push((last_sender.unwrap_or_default(), msg_id));
                }
            }
            if unread.is_empty() {
                return Ok(());
            }
            // Dedup by id (preserving chronological order) and cap to bound the
            // stanza size — a chat unread for weeks could otherwise ack thousands.
            {
                let mut seen = std::collections::HashSet::new();
                unread.retain(|(_, id)| seen.insert(id.clone()));
                const MAX_RECEIPT_IDS: usize = 100;
                if unread.len() > MAX_RECEIPT_IDS {
                    // Keep the NEWEST ids (history is chronological ascending).
                    let drop_to = unread.len() - MAX_RECEIPT_IDS;
                    unread.drain(0..drop_to);
                }
            }

            if chat_id.ends_with("@g.us") {
                // Groups: one receipt per sender, with that sender as the
                // `participant` (whatsmeow semantics — a receipt's participant
                // covers only that sender's ids). Keep the LID→phone retry ladder.
                let mut by_sender: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for (sender, id) in unread {
                    by_sender.entry(sender).or_default().push(id);
                }
                for (sender_str, ids) in by_sender {
                    let sender_jid = sender_str.parse::<Jid>().ok();
                    log::info!(
                        "MarkRead: chat={chat_id} sender={sender_str} ids={}",
                        ids.len()
                    );
                    let result = client
                        .mark_as_read(&jid, sender_jid.as_ref(), ids.clone())
                        .await;
                    if let Err(e) = &result {
                        log::warn!("MarkRead group attempt 1 failed: {e:#}");
                        let _ = client
                            .mark_as_read(&jid, None, ids.clone())
                            .await
                            .map_err(|e2| {
                                log::warn!("MarkRead group attempt 2 (no sender): {e2:#}")
                            });
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
                                    .mark_as_read(&jid, Some(&phone_jid), ids)
                                    .await
                                    .map_err(|e3| {
                                        log::warn!("MarkRead group attempt 3 (phone): {e3:#}")
                                    });
                            }
                        }
                    } else {
                        log::info!("MarkRead succeeded for {chat_id} (sender {sender_str})");
                    }
                }
            } else {
                // DM: one receipt, no participant.
                let ids: Vec<String> = unread.into_iter().map(|(_, id)| id).collect();
                log::info!("MarkRead: chat={chat_id} ids={}", ids.len());
                if let Err(e) = client.mark_as_read(&jid, None, ids).await {
                    log::warn!("MarkRead failed: {e:#}");
                } else {
                    log::info!("MarkRead succeeded for {chat_id}");
                }
            }
        }

        WaCommand::Logout => {
            client.disconnect().await;
        }

        WaCommand::GmessagesRepair => {
            // Routed to the gmessages runtime by the dispatcher; if it
            // reaches here, gmessages is disabled. No-op.
            log::debug!("WhatsApp runtime received GmessagesRepair (gm runtime disabled)");
        }

        WaCommand::SetProfilePicture { path } => match std::fs::read(&path) {
            Ok(data) => match client.profile().set_profile_picture(data).await {
                Ok(_) => log::info!("Profile picture updated from {path}"),
                Err(e) => log::warn!("Failed to set profile picture: {e:#}"),
            },
            Err(e) => log::warn!("Failed to read profile image file {path}: {e}"),
        },

        WaCommand::SetAutoMarkRead { chat_id, enabled } => {
            let save_tx;
            let chats_snapshot;
            {
                let mut s = state.lock().unwrap();
                if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    c.auto_mark_read = enabled;
                }
                save_tx = s.save_tx.clone();
                chats_snapshot = s.chats.clone();
            }
            let _ = save_tx.send(chats_snapshot);
            // If enabling, mark now so the chat transitions immediately.
            if enabled {
                let jid: Jid = chat_id.parse()?;
                let _ = client
                    .chat_actions()
                    .mark_chat_as_read(&jid, true, None)
                    .await;
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
            // Persist locally first (works for ANY chat incl gm/verification):
            // set unread=1 and roll the read watermark back below the last
            // message so a reseed / restart doesn't clamp it back to read.
            {
                let mut s = state.lock().unwrap();
                let ts = if let Some(c) = s.chats.iter_mut().find(|c| c.id == chat_id) {
                    if c.unread_count == 0 {
                        c.unread_count = 1;
                    }
                    Some(c.timestamp)
                } else {
                    None
                };
                if let Some(ts) = ts {
                    s.read_watermarks
                        .insert(chat_id.clone(), ts.saturating_sub(1));
                    save_read_watermarks(&s.read_watermarks);
                    let chats = s.chats.clone();
                    let _ = s.save_tx.send(chats);
                    // Authoritative badge refresh (replaces ChatMarkedUnread).
                    // No-op for gm: ids (not in self.chats) — the gm runtime's
                    // MarkUnread leg emits their ChatRowChanged instead.
                    s.emit_row(&chat_id);
                }
            }
            // WhatsApp-server mark-unread only applies to real JIDs.
            if let Ok(jid) = chat_id.parse::<Jid>() {
                let _ = client
                    .chat_actions()
                    .mark_chat_as_read(&jid, false, None)
                    .await;
            }
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
            match client.blocking().block(&jid).await {
                Ok(_) => {
                    let _ = tx.send(WaEvent::InfoToast("Contact blocked".into())).await;
                }
                Err(e) => {
                    log::warn!("Block {chat_id} failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to block contact: {e}")))
                        .await;
                }
            }
        }

        WaCommand::ClearChat { chat_id } => {
            {
                let mut s = state.lock().unwrap();
                s.history.remove(&chat_id);
                // Blank the preview but KEEP the timestamp (so the row holds
                // its position and renders a sane date, not the 1970 epoch the
                // old clear path produced). Persists + emits the row refresh.
                s.set_chat_preview(&chat_id, "");
            }
            let _ = tokio::task::spawn_blocking({
                let chat_id = chat_id.clone();
                move || {
                    let _ = std::fs::remove_file(messages_file(&chat_id));
                }
            })
            .await;
            // Keep ChatCleared for chat_view (clears the open conversation).
            let _ = tx.send(WaEvent::ChatCleared { chat_id }).await;
        }

        WaCommand::DeleteChat { chat_id } => {
            // WhatsApp-server delete only applies to real WA JIDs. A gm:/SMS
            // chat_id fails to parse — previously the `?` bailed here so the row
            // was never removed and the user got no feedback (they'd confirmed a
            // dialog). Now the local delete + feedback always run; the WA server
            // call is attempted only for a valid JID.
            if let Ok(jid) = chat_id.parse::<Jid>() {
                if let Err(e) = client.chat_actions().delete_chat(&jid, false, None).await {
                    log::warn!("Delete chat {chat_id} failed: {e:#}");
                }
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
                let mut updated_reactions: Option<(Vec<(String, String)>, bool)> = None;
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
                        let is_latest = msgs
                            .iter()
                            .max_by_key(|m| m.timestamp)
                            .map(|m| m.id == msg_id)
                            .unwrap_or(false);
                        if let Some(m) = msgs.iter_mut().find(|m| m.id == msg_id) {
                            m.reactions.retain(|(s, _)| *s != own_jid);
                            // Empty emoji = clear our own reaction (toggle off).
                            if !emoji.is_empty() {
                                m.reactions.push((own_jid, emoji.clone()));
                            }
                            updated_reactions = Some((m.reactions.clone(), is_latest));
                            s.queue_save_messages(&chat_id);
                        }
                    }
                }
                if let Some((reactions, is_latest)) = updated_reactions {
                    // Ephemeral "Reacted 👍" sidebar override on the latest
                    // message (adds only). Rendered live, never persisted (A6).
                    if is_latest && !emoji.is_empty() {
                        state
                            .lock()
                            .unwrap()
                            .emit_row_ephemeral(&chat_id, &format!("Reacted {emoji}"));
                    }
                    let _ = tx
                        .send(WaEvent::ReactionUpdated {
                            chat_id,
                            msg_id,
                            reactions,
                            is_latest,
                        })
                        .await;
                }
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
            let new_preview = {
                let mut s = state.lock().unwrap();
                let mut was_latest = false;
                if let Some(history) = s.history.get_mut(&chat_id) {
                    was_latest = history
                        .iter()
                        .max_by_key(|m| m.timestamp)
                        .map(|m| m.id == msg_id)
                        .unwrap_or(false);
                    history.retain(|m| m.id != msg_id);
                    s.queue_save_messages(&chat_id);
                }
                // Delete-for-me removes nothing for the other party, so the preview
                // should fall back to the new latest REMAINING message, not a
                // "deleted" stamp.
                if was_latest {
                    let is_group = chat_id.ends_with("@g.us");
                    let preview = s
                        .history
                        .get(&chat_id)
                        .and_then(|h| h.iter().max_by_key(|m| m.timestamp))
                        .map(|m| row_preview(m, is_group))
                        .unwrap_or_default();
                    // Timestamp preserved (no reorder); persists + emits row.
                    s.set_chat_preview(&chat_id, &preview);
                    Some(preview)
                } else {
                    None
                }
            };
            let _ = tx
                .send(WaEvent::MessageDeletedLocal {
                    chat_id,
                    msg_id,
                    new_preview,
                })
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
                        if let Ok(upload) =
                            client.upload(data, upload_type, Default::default()).await
                        {
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
                                        media_key: Some(upload.media_key.to_vec()),
                                        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                        file_sha256: Some(upload.file_sha256.to_vec()),
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
                                        media_key: Some(upload.media_key.to_vec()),
                                        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                        file_sha256: Some(upload.file_sha256.to_vec()),
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
                                        media_key: Some(upload.media_key.to_vec()),
                                        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                        file_sha256: Some(upload.file_sha256.to_vec()),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                                Some(crate::bridge::MediaType::Document) => {
                                    // Derive mime type from the original filename
                                    // (preferred — preserves the user-visible
                                    // extension) or fall back to the local path.
                                    // Without this, recipients see all forwarded
                                    // docs as `.bin` because the server defaults
                                    // unknown mimetypes to application/octet-stream.
                                    let mime_source = orig
                                        .media_filename
                                        .as_deref()
                                        .unwrap_or(local_path)
                                        .to_lowercase();
                                    let mime = mime_from_extension(&mime_source);
                                    wa::Message {
                                        document_message: Some(Box::new(
                                            wa::message::DocumentMessage {
                                                mimetype: Some(mime),
                                                file_name: orig.media_filename.clone(),
                                                url: Some(upload.url),
                                                direct_path: Some(upload.direct_path),
                                                media_key: Some(upload.media_key.to_vec()),
                                                file_enc_sha256: Some(
                                                    upload.file_enc_sha256.to_vec(),
                                                ),
                                                file_sha256: Some(upload.file_sha256.to_vec()),
                                                file_length: Some(upload.file_length),
                                                context_info: Some(fwd_ctx),
                                                ..Default::default()
                                            },
                                        )),
                                        ..Default::default()
                                    }
                                }
                                Some(crate::bridge::MediaType::Audio) => wa::Message {
                                    audio_message: Some(Box::new(wa::message::AudioMessage {
                                        mimetype: Some("audio/ogg".into()),
                                        url: Some(upload.url),
                                        direct_path: Some(upload.direct_path),
                                        media_key: Some(upload.media_key.to_vec()),
                                        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                        file_sha256: Some(upload.file_sha256.to_vec()),
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
                                        media_key: Some(upload.media_key.to_vec()),
                                        file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                        file_sha256: Some(upload.file_sha256.to_vec()),
                                        file_length: Some(upload.file_length),
                                        context_info: Some(fwd_ctx),
                                        ..Default::default()
                                    })),
                                    ..Default::default()
                                },
                            };
                            if let Ok(real_id) = client.send_message(to_jid.clone(), msg).await {
                                let real_id = real_id.message_id;
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
                                let _ = tx.send(WaEvent::MessageReceived(Box::new(fwd_msg))).await;
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
                        let real_id = real_id.message_id;
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
                        let _ = tx.send(WaEvent::MessageReceived(Box::new(fwd_msg))).await;
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
                            let real_id = real_id.message_id;
                            count += 1;
                            // Create optimistic local message so it appears immediately
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let local_msg = IncomingMessage {
                                media_download: None,
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
                            let _ = tx.send(WaEvent::MessageReceived(Box::new(local_msg))).await;
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
                    let chat_id = result.metadata.id.to_string();
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
                        auto_mark_read: false,
                    };
                    persist_chat(state, summary);
                    // Emit the POST-upsert summary: upsert_chat's hardening
                    // guards may have preserved a real preview/timestamp over
                    // this ts=now + empty-preview payload, so the pre-upsert
                    // value would be stale (correction 4). Read it back.
                    let stored = state
                        .lock()
                        .unwrap()
                        .chats
                        .iter()
                        .find(|c| c.id == chat_id)
                        .cloned();
                    if let Some(stored) = stored {
                        let _ = tx.send(WaEvent::ChatAdded(stored)).await;
                    }
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
                                is_admin: p.is_admin(),
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
                    let msg_id = msg_id.message_id;
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
                        media_download: None,
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
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(poll_msg))).await;
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
                                auto_mark_read: false,
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
                                is_admin: p.is_admin(),
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
                        if !p.is_admin() {
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
            match client
                .contacts()
                .is_on_whatsapp(&[Jid::pn(normalized)])
                .await
            {
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
                    auto_mark_read: false,
                };
                persist_chat(state, summary);
                // Emit the POST-upsert summary (correction 4) — upsert_chat may
                // have preserved a pre-existing preview/timestamp for this JID
                // over the ts=now + empty payload.
                let stored = state
                    .lock()
                    .unwrap()
                    .chats
                    .iter()
                    .find(|c| c.id == resolved_jid)
                    .cloned();
                if let Some(stored) = stored {
                    let _ = tx.send(WaEvent::ChatAdded(stored)).await;
                }
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

            let (upload_type, mime_owned) = if is_image {
                (wacore::download::MediaType::Image, "image/jpeg".to_string())
            } else if is_video {
                (wacore::download::MediaType::Video, "video/mp4".to_string())
            } else {
                // Detect MIME from extension — sending application/octet-stream
                // for everything made receivers see all files as .bin.
                (
                    wacore::download::MediaType::Document,
                    mime_from_extension(&lower_path),
                )
            };
            let mime = mime_owned.as_str();

            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("file")
                .to_string();

            match client
                .upload(file_data, upload_type, Default::default())
                .await
            {
                Ok(upload) => {
                    let file_len = upload.file_length;
                    let msg = if is_image {
                        wa::Message {
                            image_message: Some(Box::new(wa::message::ImageMessage {
                                mimetype: Some(mime.to_string()),
                                caption: caption.clone(),
                                url: Some(upload.url),
                                direct_path: Some(upload.direct_path),
                                media_key: Some(upload.media_key.to_vec()),
                                file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                file_sha256: Some(upload.file_sha256.to_vec()),
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
                                media_key: Some(upload.media_key.to_vec()),
                                file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                file_sha256: Some(upload.file_sha256.to_vec()),
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
                                media_key: Some(upload.media_key.to_vec()),
                                file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                                file_sha256: Some(upload.file_sha256.to_vec()),
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
                            let real_id = real_id.message_id;
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let sent_msg = IncomingMessage {
                                media_download: None,
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
                                // Persist out of /tmp so the sent image survives a reboot.
                                media_local_path: Some(persist_outgoing_media(&path)),
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
                            let _ = tx.send(WaEvent::MessageReceived(Box::new(sent_msg))).await;
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
            let data = match tokio::task::spawn_blocking(move || {
                let bytes = fetch_bounded_bytes(&mp4_url, TENOR_GIF_SEND_MAX_BYTES)?;
                validate_mp4(&bytes)?;
                Ok::<_, anyhow::Error>(bytes)
            })
            .await
            {
                Ok(Ok(data)) => data,
                Ok(Err(e)) => {
                    log::warn!("GIF download failed: {e:#}");
                    report_tenor_send_failure(
                        tx,
                        &tmp_id,
                        &chat_id,
                        "Couldn’t download that GIF. Please try another result.",
                    )
                    .await;
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("GIF download task failed: {e:#}");
                    report_tenor_send_failure(
                        tx,
                        &tmp_id,
                        &chat_id,
                        "GIF download stopped unexpectedly. Please try again.",
                    )
                    .await;
                    return Ok(());
                }
            };

            // Save locally so the GIF shows in our app
            let local_path = match save_tenor_media("gif", "mp4", &data).await {
                Ok(path) => Some(path),
                Err(e) => {
                    // Sending can still succeed if the local cache is unwritable.
                    log::warn!("Could not cache downloaded GIF: {e:#}");
                    None
                }
            };

            // Upload as video
            match client
                .upload(data, wacore::download::MediaType::Video, Default::default())
                .await
            {
                Ok(upload) => {
                    let msg = wa::Message {
                        video_message: Some(Box::new(wa::message::VideoMessage {
                            mimetype: Some("video/mp4".to_string()),
                            url: Some(upload.url),
                            direct_path: Some(upload.direct_path),
                            media_key: Some(upload.media_key.to_vec()),
                            file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                            file_sha256: Some(upload.file_sha256.to_vec()),
                            file_length: Some(upload.file_length),
                            gif_playback: Some(true),
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    match client.send_message(jid, msg).await {
                        Ok(real_id) => {
                            let real_id = real_id.message_id;
                            log::info!("GIF sent: {real_id}");
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let sent = IncomingMessage {
                                media_download: None,
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
                                media_local_path: local_path,
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
                            let _ = tx.send(WaEvent::MessageReceived(Box::new(sent))).await;
                            let _ = tx
                                .send(WaEvent::MessageConfirmed {
                                    tmp_id,
                                    real_id,
                                    chat_id,
                                })
                                .await;
                        }
                        Err(e) => {
                            log::warn!("GIF send failed: {e:#}");
                            report_tenor_send_failure(
                                tx,
                                &tmp_id,
                                &chat_id,
                                "Couldn’t send that GIF. Please try again.",
                            )
                            .await;
                        }
                    }
                }
                Err(e) => {
                    log::warn!("GIF upload failed: {e:#}");
                    report_tenor_send_failure(
                        tx,
                        &tmp_id,
                        &chat_id,
                        "Couldn’t upload that GIF. Please try again.",
                    )
                    .await;
                }
            }
        }

        WaCommand::SearchGifs { request_id, query } => {
            use std::sync::atomic::Ordering;
            LATEST_GIF_SEARCH.fetch_max(request_id, Ordering::AcqRel);
            let permit = match TENOR_SEARCH_SEMAPHORE.acquire().await {
                Ok(permit) => permit,
                Err(e) => {
                    log::warn!("GIF search limiter unavailable: {e}");
                    let _ = tx
                        .send(WaEvent::GifResults {
                            request_id,
                            query,
                            gifs: Vec::new(),
                            error: Some("GIF search is unavailable. Please try again.".to_string()),
                        })
                        .await;
                    return Ok(());
                }
            };
            if LATEST_GIF_SEARCH.load(Ordering::Acquire) != request_id {
                return Ok(());
            }
            let search_query = query.clone();
            let result = tokio::task::spawn_blocking(move || {
                search_tenor(&search_query, TenorSearchKind::Gif)
            })
            .await;
            drop(permit);
            if LATEST_GIF_SEARCH.load(Ordering::Acquire) != request_id {
                return Ok(());
            }
            let (gifs, error) = match result {
                Ok(Ok(gifs)) => {
                    log::info!("GIF search '{}': {} results", query, gifs.len());
                    (gifs, None)
                }
                Ok(Err(e)) => {
                    log::warn!("GIF search failed: {e:#}");
                    (
                        Vec::new(),
                        Some(
                            "Tenor search is unavailable. Check your connection and try again."
                                .to_string(),
                        ),
                    )
                }
                Err(e) => {
                    log::warn!("GIF search task failed: {e:#}");
                    (
                        Vec::new(),
                        Some("GIF search stopped unexpectedly. Please try again.".to_string()),
                    )
                }
            };
            let _ = tx
                .send(WaEvent::GifResults {
                    request_id,
                    query,
                    gifs,
                    error,
                })
                .await;
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
                    let real_id = real_id.message_id;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let sent = IncomingMessage {
                        media_download: None,
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
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sent))).await;
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

        WaCommand::SearchStickers { request_id, query } => {
            use std::sync::atomic::Ordering;
            LATEST_STICKER_SEARCH.fetch_max(request_id, Ordering::AcqRel);
            let permit = match TENOR_SEARCH_SEMAPHORE.acquire().await {
                Ok(permit) => permit,
                Err(e) => {
                    log::warn!("Sticker search limiter unavailable: {e}");
                    let _ = tx
                        .send(WaEvent::StickerResults {
                            request_id,
                            query,
                            stickers: Vec::new(),
                            error: Some(
                                "Sticker search is unavailable. Please try again.".to_string(),
                            ),
                        })
                        .await;
                    return Ok(());
                }
            };
            if LATEST_STICKER_SEARCH.load(Ordering::Acquire) != request_id {
                return Ok(());
            }
            let search_query = query.clone();
            let result = tokio::task::spawn_blocking(move || {
                search_tenor(&search_query, TenorSearchKind::Sticker)
            })
            .await;
            drop(permit);
            if LATEST_STICKER_SEARCH.load(Ordering::Acquire) != request_id {
                return Ok(());
            }
            let (stickers, error) = match result {
                Ok(Ok(stickers)) => {
                    log::info!("Sticker search '{}': {} results", query, stickers.len());
                    (stickers, None)
                }
                Ok(Err(e)) => {
                    log::warn!("Sticker search failed: {e:#}");
                    (
                        Vec::new(),
                        Some("Tenor sticker search is unavailable. Check your connection and try again.".to_string()),
                    )
                }
                Err(e) => {
                    log::warn!("Sticker search task failed: {e:#}");
                    (
                        Vec::new(),
                        Some("Sticker search stopped unexpectedly. Please try again.".to_string()),
                    )
                }
            };
            let _ = tx
                .send(WaEvent::StickerResults {
                    request_id,
                    query,
                    stickers,
                    error,
                })
                .await;
        }

        WaCommand::SendSticker {
            chat_id,
            webp_url,
            tmp_id,
        } => {
            let jid: Jid = chat_id.parse()?;
            let data = match tokio::task::spawn_blocking(move || {
                let bytes = fetch_bounded_bytes(&webp_url, TENOR_STICKER_SEND_MAX_BYTES)?;
                validate_webp(&bytes)?;
                Ok::<_, anyhow::Error>(bytes)
            })
            .await
            {
                Ok(Ok(data)) => data,
                Ok(Err(e)) => {
                    log::warn!("Sticker download failed: {e:#}");
                    report_tenor_send_failure(
                        tx,
                        &tmp_id,
                        &chat_id,
                        "Couldn’t download that sticker. Please try another result.",
                    )
                    .await;
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("Sticker download task failed: {e:#}");
                    report_tenor_send_failure(
                        tx,
                        &tmp_id,
                        &chat_id,
                        "Sticker download stopped unexpectedly. Please try again.",
                    )
                    .await;
                    return Ok(());
                }
            };

            // Save locally
            let local_path = match save_tenor_media("sticker", "webp", &data).await {
                Ok(path) => Some(path),
                Err(e) => {
                    log::warn!("Could not cache downloaded sticker: {e:#}");
                    None
                }
            };

            match client
                .upload(data, wacore::download::MediaType::Image, Default::default())
                .await
            {
                Ok(upload) => {
                    let msg = wa::Message {
                        sticker_message: Some(Box::new(wa::message::StickerMessage {
                            mimetype: Some("image/webp".to_string()),
                            url: Some(upload.url),
                            direct_path: Some(upload.direct_path),
                            media_key: Some(upload.media_key.to_vec()),
                            file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                            file_sha256: Some(upload.file_sha256.to_vec()),
                            file_length: Some(upload.file_length),
                            ..Default::default()
                        })),
                        ..Default::default()
                    };
                    match client.send_message(jid, msg).await {
                        Ok(real_id) => {
                            let real_id = real_id.message_id;
                            log::info!("Sticker sent: {real_id}");
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let sent = IncomingMessage {
                                media_download: None,
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
                                media_local_path: local_path,
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
                            let _ = tx.send(WaEvent::MessageReceived(Box::new(sent))).await;
                            let _ = tx
                                .send(WaEvent::MessageConfirmed {
                                    tmp_id,
                                    real_id,
                                    chat_id,
                                })
                                .await;
                        }
                        Err(e) => {
                            log::warn!("Sticker send failed: {e:#}");
                            report_tenor_send_failure(
                                tx,
                                &tmp_id,
                                &chat_id,
                                "Couldn’t send that sticker. Please try again.",
                            )
                            .await;
                        }
                    }
                }
                Err(e) => {
                    log::warn!("Sticker upload failed: {e:#}");
                    report_tenor_send_failure(
                        tx,
                        &tmp_id,
                        &chat_id,
                        "Couldn’t upload that sticker. Please try again.",
                    )
                    .await;
                }
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
            // Guard: an unresolved optimistic id would put an unmatchable key on
            // the wire — the server ACKs it but the peer silently discards the
            // edit. Bounce the text back to the composer instead (the UI resolves
            // tmp→real before sending; this only fires if the send hadn't yet
            // confirmed when the user saved the edit).
            if msg_id.starts_with("tmp-") {
                log::warn!(
                    "EditMessage for unconfirmed message {msg_id} in {chat_id} — cannot edit until the send confirms"
                );
                let _ = tx
                    .send(WaEvent::EditFailed {
                        chat_id,
                        msg_id,
                        new_text,
                    })
                    .await;
                return Ok(());
            }
            let jid: Jid = chat_id.parse()?;
            let new_content = wa::Message {
                conversation: Some(new_text.clone()),
                ..Default::default()
            };
            match client.edit_message(jid, &msg_id, new_content).await {
                Ok(_) => {
                    // Update local cache
                    let is_latest = {
                        let mut s = state.lock().unwrap();
                        let mut is_latest = false;
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            if let Some(m) = msgs.iter_mut().find(|m| m.id == msg_id) {
                                m.text = Some(new_text.clone());
                                m.is_edited = true;
                            }
                            is_latest = msgs
                                .iter()
                                .max_by_key(|m| m.timestamp)
                                .map(|m| m.id == msg_id)
                                .unwrap_or(false);
                            s.queue_save_messages(&chat_id);
                        }
                        if is_latest {
                            // Edit of the latest message: refresh preview text,
                            // keep timestamp (no reorder); persists + emits (A5).
                            s.set_chat_preview(&chat_id, &new_text);
                        }
                        is_latest
                    };
                    let _ = tx
                        .send(WaEvent::MessageEdited {
                            chat_id,
                            msg_id,
                            new_text,
                            is_latest,
                        })
                        .await;
                }
                Err(e) => {
                    log::warn!("EditMessage failed: {e:#}");
                    let _ = tx
                        .send(WaEvent::ErrorToast(format!("Failed to edit message: {e}")))
                        .await;
                    // Give the edited text back to the composer so it isn't lost.
                    let _ = tx
                        .send(WaEvent::EditFailed {
                            chat_id,
                            msg_id,
                            new_text,
                        })
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
                    let sys_msg = make_system_message(&chat_id, &format!("You added {name}"));
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sys_msg))).await;
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
                                    is_admin: p.is_admin(),
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
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sys_msg))).await;
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
                                    is_admin: p.is_admin(),
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
                    let sys_msg =
                        make_system_message(&chat_id, &format!("You made {name} an admin"));
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sys_msg))).await;
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
                    let sys_msg =
                        make_system_message(&chat_id, &format!("You removed {name} as admin"));
                    {
                        let mut s = state.lock().unwrap();
                        if let Some(msgs) = s.history.get_mut(&chat_id) {
                            msgs.push(sys_msg.clone());
                            s.queue_save_messages(&chat_id);
                        }
                    }
                    let _ = tx.send(WaEvent::MessageReceived(Box::new(sys_msg))).await;
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
                    match client.upload(data, upload_type, Default::default()).await {
                        Ok(upload) => {
                            let now_ts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let msg = wa::Message {
                                audio_message: Some(Box::new(wa::message::AudioMessage {
                                    url: Some(upload.url),
                                    direct_path: Some(upload.direct_path),
                                    media_key: Some(upload.media_key.to_vec()),
                                    file_sha256: Some(upload.file_sha256.to_vec()),
                                    file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
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
                                    let real_id = real_id.message_id;
                                    // Build a sent voice-note message and route it
                                    // through the choke point (mirror SendImage):
                                    // persists the summary (→ ChatRowChanged) AND
                                    // the message itself. Without this the row
                                    // froze on voice-note sends once
                                    // bump_chat_to_top was deleted — and the note
                                    // was never persisted at all (correction 1).
                                    let mut sent_msg = IncomingMessage::outgoing(
                                        real_id.clone(),
                                        chat_id.clone(),
                                        None,
                                        now_ts,
                                    );
                                    sent_msg.media_type = Some(crate::bridge::MediaType::Audio);
                                    // Persist out of /tmp so it survives a reboot.
                                    sent_msg.media_local_path = Some(persist_outgoing_media(&path));
                                    sent_msg.receipt_status = ReceiptStatus::Sent;
                                    let (_, new_chat) = persist_new_message(&sent_msg, state);
                                    if let Some(s) = new_chat {
                                        let _ = tx.send(WaEvent::ChatAdded(s)).await;
                                    }
                                    let _ = tx
                                        .send(WaEvent::MessageConfirmed {
                                            tmp_id,
                                            real_id,
                                            chat_id,
                                        })
                                        .await;
                                    let _ =
                                        tx.send(WaEvent::MessageReceived(Box::new(sent_msg))).await;
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
                        let real_id = real_id.message_id;
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
                        // Route through the choke point so each target row's
                        // preview/timestamp updates and emits ChatRowChanged —
                        // the bare history push left broadcast rows frozen once
                        // bump_chat_to_top was deleted (correction 1).
                        let (_, new_chat) = persist_new_message(&self_msg, state);
                        if let Some(s) = new_chat {
                            let _ = tx.send(WaEvent::ChatAdded(s)).await;
                        }
                        let _ = tx.send(WaEvent::MessageReceived(Box::new(self_msg))).await;
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
                    .send(WaEvent::MultiSendComplete {
                        sent: 0,
                        failed: chat_ids.len() as u32,
                    })
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
                            let real_id = real_id.message_id;
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
                                s.history
                                    .entry(cid.clone())
                                    .or_default()
                                    .push(self_msg.clone());
                                s.queue_save_messages(cid);
                            }
                            let _ = tx.send(WaEvent::MessageReceived(Box::new(self_msg))).await;
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
        let messages: Vec<crate::bridge::IncomingMessage> = match bincode::deserialize(&data[4..]) {
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
static MEDIA_DOWNLOAD_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Copy an outgoing media file out of a volatile dir (`/tmp`, wiped on reboot)
/// into the persistent `wa_media` dir, so the local echo of a sent image still
/// has its file after a restart. Pasted/clipboard images are staged in
/// `/tmp/wa_paste_*`; persisting them here is the difference between a sent
/// image surviving a reboot and showing an empty placeholder. No-op for paths
/// that are already persistent; returns the original path if the copy fails.
fn persist_outgoing_media(path: &str) -> String {
    if !path.starts_with("/tmp/") {
        return path.to_string();
    }
    let src = std::path::Path::new(path);
    let Some(fname) = src.file_name() else {
        return path.to_string();
    };
    let dir = std::env::current_dir().unwrap_or_default().join(MEDIA_DIR);
    let _ = std::fs::create_dir_all(&dir);
    let dest = dir.join(fname);
    match std::fs::copy(src, &dest) {
        Ok(_) => dest.to_string_lossy().to_string(),
        Err(e) => {
            log::warn!(
                "persist_outgoing_media: copy {path} -> {} failed: {e}",
                dest.display()
            );
            path.to_string()
        }
    }
}

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

/// Pull the persistable media-download keys out of a base message if it carries
/// a downloadable attachment. Mirrors `extract_pending_download` but keeps only
/// the keys, so they ride on the persisted `IncomingMessage` and can drive an
/// on-demand re-download later (history-synced, failed, or skipped media).
fn extract_media_download_keys(base: &wa::Message) -> Option<crate::bridge::MediaDownloadKeys> {
    use crate::bridge::MediaDownloadKeys;
    macro_rules! keys_from {
        ($m:expr) => {{
            let m = $m;
            if m.direct_path.is_some() && m.media_key.is_some() {
                return Some(MediaDownloadKeys {
                    direct_path: m.direct_path.clone().unwrap_or_default(),
                    media_key: m.media_key.clone().unwrap_or_default(),
                    enc_sha256: m.file_enc_sha256.clone().unwrap_or_default(),
                    sha256: m.file_sha256.clone().unwrap_or_default(),
                    file_length: m.file_length.unwrap_or(0),
                    mimetype: m.mimetype.clone(),
                });
            }
        }};
    }
    if let Some(m) = &base.image_message {
        keys_from!(m);
    }
    if let Some(m) = &base.video_message {
        keys_from!(m);
    }
    if let Some(m) = &base.document_message {
        keys_from!(m);
    }
    if let Some(m) = &base.audio_message {
        keys_from!(m);
    }
    if let Some(m) = &base.sticker_message {
        keys_from!(m);
    }
    None
}

/// Rebuild a [`PendingDownload`] from persisted keys for an on-demand re-download.
/// `media_type` selects the protobuf shape, which drives the HKDF key context.
fn pending_from_keys(
    keys: &crate::bridge::MediaDownloadKeys,
    media_type: Option<&crate::bridge::MediaType>,
    filename: Option<String>,
) -> Option<PendingDownload> {
    use crate::bridge::MediaType;
    let dp = Some(keys.direct_path.clone());
    let mk = Some(keys.media_key.clone());
    let enc = Some(keys.enc_sha256.clone());
    let sha = Some(keys.sha256.clone());
    let len = Some(keys.file_length);
    let mime = keys.mimetype.clone();
    Some(match media_type? {
        MediaType::Image => PendingDownload::Image {
            msg: Box::new(wa::message::ImageMessage {
                direct_path: dp,
                media_key: mk,
                file_enc_sha256: enc,
                file_sha256: sha,
                file_length: len,
                mimetype: mime,
                ..Default::default()
            }),
        },
        MediaType::Video | MediaType::Gif => PendingDownload::Video {
            msg: Box::new(wa::message::VideoMessage {
                direct_path: dp,
                media_key: mk,
                file_enc_sha256: enc,
                file_sha256: sha,
                file_length: len,
                mimetype: mime,
                ..Default::default()
            }),
        },
        MediaType::Audio => PendingDownload::Audio {
            msg: Box::new(wa::message::AudioMessage {
                direct_path: dp,
                media_key: mk,
                file_enc_sha256: enc,
                file_sha256: sha,
                file_length: len,
                mimetype: mime,
                ..Default::default()
            }),
        },
        MediaType::Document => PendingDownload::Document {
            msg: Box::new(wa::message::DocumentMessage {
                direct_path: dp,
                media_key: mk,
                file_enc_sha256: enc,
                file_sha256: sha,
                file_length: len,
                mimetype: mime,
                file_name: filename.clone(),
                ..Default::default()
            }),
            filename,
        },
        MediaType::Sticker => PendingDownload::Sticker {
            msg: Box::new(wa::message::StickerMessage {
                direct_path: dp,
                media_key: mk,
                file_enc_sha256: enc,
                file_sha256: sha,
                file_length: len,
                mimetype: mime,
                ..Default::default()
            }),
        },
    })
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
    let Ok(_permit) = MEDIA_DOWNLOAD_LIMIT.acquire().await else {
        return;
    };
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
    let path_str = match tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, bytes)?;
        // Use absolute path so GTK can find it regardless of working directory.
        Ok(path
            .canonicalize()
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned())
    })
    .await
    {
        Ok(Ok(path)) => path,
        Ok(Err(e)) => {
            log::warn!("Failed to save media {local_name}: {e}");
            return;
        }
        Err(e) => {
            log::warn!("Media writer task failed for {local_name}: {e}");
            return;
        }
    };
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
/// Proactively resolve saved contacts' LID mappings by usyncing their PHONE
/// numbers (the direction the server actually answers — usync returns a `<lid>`
/// child only for phone-keyed queries). This pre-warms phone↔LID for the whole
/// phonebook so an incoming LID resolves on arrival instead of minting a phantom
/// "+<lid digits>" chat, and lets the subsequent merge collapse any existing
/// phantom whose contact is in the phonebook (and whose LID is current). Runs
/// once per session, skips already-mapped contacts, chunked + throttled.
async fn prewarm_contact_lids(client: &Arc<Client>, state: &Arc<Mutex<RuntimeState>>) {
    {
        let mut s = state.lock().unwrap();
        if s.did_phone_lid_sweep {
            return;
        }
        s.did_phone_lid_sweep = true;
    }

    // How much of the phonebook one launch is allowed to usync. The
    // ContactInfoSpec IQ times out under big loads (a 2000-contact sweep hung a
    // chunk), so we cover a small slice per launch and resume next time via the
    // persisted "done" set below.
    const CHUNK: usize = 50;
    const MAX_CHUNKS: usize = 4; // 200 phones/launch — safely under the IQ timeout

    // Resume set: phones already usynced on a previous launch. Contacts we
    // already have a LID for count as done without spending an IQ.
    let mut swept_done = load_lid_sweep_done();
    let done_before = swept_done.len();

    let all_phones = crate::contacts::global().saved_contact_phones();

    // Unswept = not in the resume set AND no cached LID yet. Already-mapped
    // contacts are recorded as done so a later launch never revisits them.
    let mut unswept: Vec<String> = Vec::new();
    let mut newly_done = 0usize;
    for digits in &all_phones {
        if swept_done.contains(digits) {
            continue;
        }
        if client.get_lid_for_phone(digits).await.is_some() {
            // Already mapped (e.g. learned organically) — mark done, skip usync.
            swept_done.insert(digits.clone());
            newly_done += 1;
            continue;
        }
        unswept.push(digits.clone());
    }

    if unswept.is_empty() {
        if newly_done > 0 {
            save_lid_sweep_done(&swept_done);
        }
        log::info!(
            "phone→LID prewarm: nothing left to usync ({}/{} contacts swept)",
            swept_done.len(),
            all_phones.len()
        );
        return;
    }

    // Prioritize contacts likely to message: phones whose phone JID already
    // matches an open chat go FIRST (active conversations — e.g. the Canadian
    // contact whose phantom we want healed), so the small per-launch budget is
    // spent where it heals something visible.
    {
        let s = state.lock().unwrap();
        let active: std::collections::HashSet<String> =
            s.chats.iter().map(|c| c.id.clone()).collect();
        // sort_by_key is stable, so phonebook order is preserved within each band.
        unswept.sort_by_key(|digits| {
            let phone_jid = format!("{digits}@s.whatsapp.net");
            u8::from(!active.contains(&phone_jid)) // 0 = active chat (first), 1 = rest
        });
    }

    log::info!(
        "phone→LID prewarm: {} unswept (of {} contacts, {} already swept); usyncing up to {} this launch",
        unswept.len(),
        all_phones.len(),
        done_before,
        (MAX_CHUNKS * CHUNK).min(unswept.len())
    );

    let mut learned = 0usize;
    for (i, chunk) in unswept.chunks(CHUNK).enumerate() {
        if i >= MAX_CHUNKS {
            log::info!(
                "phone→LID prewarm: hit per-launch cap of {} phones; resuming next launch",
                MAX_CHUNKS * CHUNK
            );
            break;
        }
        // ContactInfoSpec requests the <lid/> sidecar and persists every
        // discovered phone↔LID pair into the core LID-PN cache. Returns the
        // count of new mappings; the desktop-side mirrors below read them back.
        match client.resolve_contact_lids(chunk).await {
            Ok(n) => {
                learned += n;
            }
            Err(e) => {
                log::warn!("phone→LID prewarm: usync chunk {i} failed: {e:#}");
                // Don't mark this chunk done — retry it on the next launch.
                continue;
            }
        }
        // Mirror the just-learned mappings into the desktop sources that
        // merge_lid_chats consults (UI lid_to_phone + contact-directory by_lid),
        // and mark each phone in this chunk as swept.
        for digits in chunk {
            swept_done.insert(digits.clone());
            if let Some(lid_user) = client.get_lid_for_phone(digits).await {
                let lid_jid = format!("{lid_user}@lid");
                let phone_jid = format!("{digits}@s.whatsapp.net");
                state
                    .lock()
                    .unwrap()
                    .insert_lid_phone(lid_jid.clone(), phone_jid.clone());
                crate::contacts::global().record_lid_jid(&phone_jid, &lid_jid);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // Persist the enriched maps + resume set so the next launch starts warm and
    // continues where this one stopped.
    save_lid_sweep_done(&swept_done);
    let map = state.lock().unwrap().lid_to_phone.clone();
    if !map.is_empty() {
        std::thread::spawn(move || save_lid_phone_map(&map));
    }
    crate::contacts::global().save_if_dirty();
    log::info!(
        "phone→LID prewarm: learned {learned} new mapping(s); swept-set now {}/{} contacts",
        swept_done.len(),
        all_phones.len()
    );
}

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

/// Merge a single @lid phantom chat into its resolved phone-JID chat: fold the
/// message files (dedup by id, chronological), swap the chat-list entry, delete
/// the @lid message file, and tell the UI to drop the phantom row + refresh the
/// merged one. The caller must have already confirmed the mapping is trustworthy.
async fn merge_one_lid_chat(
    state: &Arc<Mutex<RuntimeState>>,
    tx: &Sender<WaEvent>,
    lid_chat: &ChatSummary,
    phone_jid: &str,
) {
    log::info!("Merging {} → {}", lid_chat.id, phone_jid);

    // Store the LID→phone mapping
    state
        .lock()
        .unwrap()
        .lid_to_phone
        .insert(lid_chat.id.clone(), phone_jid.to_string());

    // Load messages from both chats, remap + merge (dedup by id)
    let mut lid_msgs = load_messages(&lid_chat.id);
    let phone_msgs = load_messages(phone_jid);
    let mut merged = phone_msgs.clone();
    for mut m in lid_msgs.drain(..) {
        m.chat_id = phone_jid.to_string();
        if !merged.iter().any(|x| x.id == m.id) {
            merged.push(m);
        }
    }
    merged.sort_by_key(|m| m.timestamp);
    save_messages_scoped(phone_jid, crate::bridge::MessageSource::WhatsApp, &merged);

    // Build merged summary and update state (under mutex — serialized with JoinedGroup tasks)
    let last_msg = merged
        .iter()
        .rev()
        .find(|m| m.text.is_some() || m.media_type.is_some());
    let timestamp = last_msg.map(|m| m.timestamp).unwrap_or(lid_chat.timestamp);
    let preview = last_msg.map(media_preview).unwrap_or_default();

    let merged_summary = {
        let mut s = state.lock().unwrap();
        // Prefer an existing named phone chat, else the contact directory's name
        // for the phone JID, else the (usually "+<lid digits>") phantom name — so
        // merging a phantom with no prior phone chat still lands the real name.
        let phone_name = s
            .chats
            .iter()
            .find(|c| c.id == phone_jid)
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty() && !n.contains('@'))
            .or_else(|| crate::contacts::global().lookup(phone_jid))
            .filter(|n| !n.is_empty() && !n.contains('@'))
            .unwrap_or_else(|| lid_chat.name.clone());
        let summary = ChatSummary {
            id: phone_jid.to_string(),
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
            auto_mark_read: lid_chat.auto_mark_read,
        };
        // Remove @lid entry, upsert @s.whatsapp.net — all within the same lock
        s.chats.retain(|c| c.id != lid_chat.id);
        s.chat_names.remove(&lid_chat.id);
        s.upsert_chat(summary, false, false);
        s.history.remove(&lid_chat.id);
        s.history.insert(phone_jid.to_string(), merged.clone());
        // Return the POST-upsert row (correction 4): upsert_chat's guards may
        // have preserved a newer preview/timestamp for an existing phone chat,
        // so the pre-upsert payload would be stale for the ChatAdded below.
        // We just upserted phone_jid, so this find always succeeds in practice;
        // the fallback is a defensive placeholder only.
        s.chats
            .iter()
            .find(|c| c.id == phone_jid)
            .cloned()
            .unwrap_or_else(|| ChatSummary {
                id: phone_jid.to_string(),
                name: String::new(),
                last_message: String::new(),
                timestamp,
                unread_count: 0,
                is_group: false,
                is_muted: false,
                is_pinned: false,
                is_archived: false,
                is_favorite: false,
                label: None,
                pinned_msg_id: None,
                auto_mark_read: false,
            })
    };

    // Delete the @lid messages file
    let _ = std::fs::remove_file(messages_file(&lid_chat.id));

    // Persist the updated LID→phone map so the resolution survives restart
    let map = state.lock().unwrap().lid_to_phone.clone();
    if !map.is_empty() {
        std::thread::spawn(move || save_lid_phone_map(&map));
    }

    // Drop the phantom row (ChatsLoaded alone never removes rows) and refresh the
    // merged row's preview/timestamp/name (add_chat upserts an existing row).
    let _ = tx
        .send(WaEvent::ChatDeleted {
            chat_id: lid_chat.id.clone(),
        })
        .await;
    let _ = tx.send(WaEvent::ChatAdded(merged_summary)).await;
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

    for lid_chat in lid_chats {
        let phone_jid = match client.resolve_lid_to_phone_jid(&lid_chat.id).await {
            // Trusted source #2: the contact directory's explicit by_lid index
            // (from contact sync — authoritative, never fuzzy). This heals a
            // phantom whose mapping the core lid_pn_cache never learned, without
            // needing a fresh message.
            None => match crate::contacts::global().resolve_lid_to_phone(&lid_chat.id) {
                Some(p) => p,
                None => {
                    // Last resort: the UI-layer lid_to_phone map (populated at
                    // message arrival from peer_recipient_pn), but ONLY when the
                    // mapped phone chat already exists — a stale/corrupt mapping
                    // must never merge the lid history into a WRONG JID and then
                    // delete the lid file. That exists-guard is exactly the
                    // phantom scenario.
                    let s = state.lock().unwrap();
                    let candidate = s.lid_to_phone.get(&lid_chat.id).cloned();
                    match candidate {
                        Some(p) if s.chats.iter().any(|c| c.id == p) => p,
                        _ => {
                            log::debug!("No usable LID→PN mapping for {}", lid_chat.id);
                            continue;
                        }
                    }
                }
            },
            Some(p) => p,
        };
        merge_one_lid_chat(state, tx, &lid_chat, &phone_jid).await;
    }
}

// ── Group name refresh ────────────────────────────────────────────────────────

/// Fetch a single group's subject from the server and apply it as the chat
/// name. Used when a brand-new group first appears via a live message (before
/// any GroupUpdate notification), so the header shows the real group name
/// instead of the creator/sender placeholder within a second — not on restart.
async fn fetch_group_subject(
    client: &Arc<Client>,
    state: &Arc<Mutex<RuntimeState>>,
    tx: &Sender<WaEvent>,
    chat_id: &str,
) {
    let Ok(jid) = chat_id.parse::<Jid>() else {
        return;
    };
    match client.groups().get_metadata(&jid).await {
        Ok(meta) if !meta.subject.is_empty() => {
            log::info!("Resolved new group {chat_id} → {:?}", meta.subject);
            state.lock().unwrap().rename_chat(chat_id, &meta.subject);
            let _ = tx
                .send(WaEvent::ChatNameUpdated {
                    chat_id: chat_id.to_string(),
                    name: meta.subject,
                })
                .await;
        }
        Ok(_) => {}
        Err(e) => log::debug!("fetch_group_subject({chat_id}) failed: {e:#}"),
    }
}

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

            // Individually query groups whose name is STILL unresolved after the
            // get_participating pass — i.e. a raw JID or a "Alice, Bob, …"
            // participant-name placeholder. Narrowing to genuinely-unresolved
            // groups (rather than a blanket first-N) means every group that
            // needs a name eventually gets one; a small delay between queries
            // rate-limits so we don't burst the server on large accounts.
            let unresolved: Vec<String> = {
                let s = state.lock().unwrap();
                s.chats
                    .iter()
                    .filter(|c| c.id.ends_with("@g.us"))
                    .filter(|c| !resolved_ids.contains(&c.id))
                    .filter(|c| {
                        let n = &c.name;
                        let looks_raw = n.contains('@')
                            || (n.chars().all(|ch| ch.is_ascii_digit() || ch == '+')
                                && n.len() > 4);
                        looks_raw || n.ends_with('\u{2026}')
                    })
                    .map(|c| c.id.clone())
                    // Cap kept generous (was 20) but bounded so a huge account
                    // can't spin for minutes on connect; remaining groups also
                    // self-correct via the JoinedGroup fetch_group_subject path.
                    .take(100)
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
                        // Light rate-limit between individual metadata queries.
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
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
        media_download: web_msg
            .message
            .as_ref()
            .and_then(|pm| extract_media_download_keys(pm.get_base_message())),
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
        media_download: extract_media_download_keys(base),
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
        media_download: None,
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

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn server_error_is_never_shown_as_delivered() {
        assert_eq!(
            receipt_status_for_type(&ReceiptType::ServerError),
            Some(ReceiptStatus::Failed)
        );
    }

    #[test]
    fn retry_receipts_do_not_claim_delivery() {
        assert_eq!(receipt_status_for_type(&ReceiptType::Retry), None);
        assert_eq!(receipt_status_for_type(&ReceiptType::EncRekeyRetry), None);
    }
}

#[cfg(test)]
mod tenor_tests {
    use super::*;
    use serde_json::json;

    fn tenor_result(index: usize) -> serde_json::Value {
        json!({
            "content_description": format!("result {index}"),
            "media_formats": {
                "gifpreview": { "url": format!("https://cdn.example/{index}-preview.gif") },
                "mp4": { "url": format!("https://cdn.example/{index}.mp4") },
                "tinygif": { "url": format!("https://cdn.example/{index}.gif") },
                "webp_transparent": { "url": format!("https://cdn.example/{index}.webp") }
            }
        })
    }

    #[test]
    fn tenor_url_components_are_encoded_safely() {
        assert_eq!(
            encode_url_component("cats & dogs/😺"),
            "cats%20%26%20dogs%2F%F0%9F%98%BA"
        );
    }

    #[test]
    fn tenor_api_parser_caps_results_and_selects_gif_formats() {
        let values = (0..20).map(tenor_result).collect::<Vec<_>>();
        let body = json!({ "results": values }).to_string();
        let results = parse_tenor_api_results(&body, TenorSearchKind::Gif).unwrap();

        assert_eq!(results.len(), TENOR_RESULT_LIMIT);
        assert_eq!(results[0].preview_url, "https://cdn.example/0-preview.gif");
        assert_eq!(results[0].mp4_url, "https://cdn.example/0.mp4");
        assert_eq!(results[0].title, "result 0");
    }

    #[test]
    fn sticker_parser_never_sends_a_gif_as_webp() {
        let values = vec![tenor_result(7)];
        let results = parse_tenor_values(&values, TenorSearchKind::Sticker);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].preview_url, "https://cdn.example/7.webp");
        assert_eq!(results[0].mp4_url, "https://cdn.example/7.webp");
        assert!(!results[0].mp4_url.ends_with(".gif"));
    }

    #[test]
    fn tenor_script_extraction_is_scoped_to_the_requested_id() {
        let html = r#"<script id="other">wrong</script><script id="data" type="text/x-cache"> right </script>"#;
        assert_eq!(extract_script_contents(html, "data"), Some("right"));
    }

    #[test]
    #[ignore = "requires live Tenor network access"]
    fn live_tenor_gif_search_returns_sendable_mp4() {
        let results = search_tenor("hello", TenorSearchKind::Gif).unwrap();
        assert!(!results.is_empty());
        assert!(results.iter().all(|result| {
            result.preview_url.starts_with("https://") && result.mp4_url.starts_with("https://")
        }));
        let bytes = fetch_bounded_bytes(&results[0].mp4_url, TENOR_GIF_SEND_MAX_BYTES).unwrap();
        validate_mp4(&bytes).unwrap();
    }

    #[test]
    #[ignore = "requires live Tenor network access"]
    fn live_tenor_sticker_search_returns_sendable_webp() {
        let results = search_tenor("hello", TenorSearchKind::Sticker).unwrap();
        assert!(!results.is_empty());
        assert!(results.iter().all(|result| {
            result.preview_url.starts_with("https://") && result.mp4_url.starts_with("https://")
        }));
        let bytes = fetch_bounded_bytes(&results[0].mp4_url, TENOR_STICKER_SEND_MAX_BYTES).unwrap();
        validate_webp(&bytes).unwrap();
    }
}
