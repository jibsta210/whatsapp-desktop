//! Cross-protocol contact directory.
//!
//! Both the WhatsApp runtime and the Google Messages runtime build their
//! own contact maps. This module unifies them into one shared,
//! phone-number-keyed table that any code path can write to and read from.
//!
//! ## Key design points
//!
//! - **Phone-number keyed.** All inputs are normalized to digits-only so
//!   `+1 (416) 400-0790`, `14164000790`, and `14164000790@s.whatsapp.net`
//!   all hit the same row.
//!
//! - **Sticky.** Once we've learned a contact's name, it stays in the
//!   directory forever — even if the contact gets removed from the user's
//!   phone or a re-sync comes back without it. The desktop client always
//!   has a name to show.
//!
//! - **Newer wins** (with a quality tiebreaker). On insert we keep:
//!   - the entry with alphabetic content over a digits-only "name"
//!     (e.g. `Lorne` beats `+14164000790`)
//!   - otherwise the entry with the more recent `updated_at`
//!
//! - **Persisted.** Loaded from `contacts_directory.bin` at startup,
//!   saved on every change (debounced).
//!
//! ## Sources
//!
//! Each entry records which sources have seen it (`"whatsapp"`,
//! `"gmessages"`, …) so we can later cross-reference the same person across
//! protocols — Phase 2 of the unified-chat-list work.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One contact entry. Phone-number-keyed (digits only). All known
/// identifiers for that person live on the same row — name from the
/// phone book, every chat_id we've seen them under (WhatsApp JID,
/// gmessages chat_id, etc.), and any anonymous LID JIDs WhatsApp has
/// minted for them. Persisted; never forgets.
///
/// "Joe Mysak" → `+15551234567` → {
///     name: "Joe Mysak",
///     chat_ids: { "whatsapp": "15551234567@s.whatsapp.net", "gmessages": "gm:42" },
///     lid_jids: ["137340286709870@lid"],
/// }
///
/// Lookup works against any of those identifiers via the secondary
/// indices on `ContactDirectory`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContactEntry {
    pub name: String,
    /// Unix seconds since epoch when this entry was last upgraded.
    pub updated_at: u64,
    /// Upstream sources that contributed data to this entry (provenance).
    #[serde(default)]
    pub sources: HashSet<String>,
    /// Per-source canonical chat id. WhatsApp uses phone JIDs here, not
    /// LIDs — LIDs go in `lid_jids` so we don't pin a "primary" LID.
    ///   `"whatsapp"`  → `"15551234567@s.whatsapp.net"`
    ///   `"gmessages"` → `"gm:42"`
    #[serde(default)]
    pub chat_ids: std::collections::HashMap<String, String>,
    /// Anonymous WhatsApp LID JIDs we've observed for this contact.
    /// Populated whenever WhatsApp's lid→phone resolution fires for them
    /// or when a chat row is created with a LID id and we later pin it
    /// to a phone.
    #[serde(default)]
    pub lid_jids: HashSet<String>,
    /// Trust tier of the source that set the current `name` (see
    /// `source_priority`). Higher = more authoritative (phonebook/contacts
    /// beats push-name/history beats typing). A higher-tier name is never
    /// overwritten by a lower-tier one regardless of length; length/word
    /// heuristics only tiebreak WITHIN the same tier. Defaults to 0 so
    /// entries persisted before this field existed are treated as the
    /// lowest tier and can be upgraded by any real source.
    #[serde(default)]
    pub name_priority: u8,
    /// True if this row was keyed off a `@lid` JID (an opaque server id, not
    /// a phone number). `insert()` skips the phone-suffix indices for these so
    /// the LID digits can't fuzzy-match a real phone; `load()` reads this back
    /// so the skip survives a restart (otherwise the indices get re-polluted on
    /// every startup). Defaults to false for entries persisted before this
    /// field existed.
    #[serde(default)]
    pub is_lid: bool,
}

impl ContactEntry {
    fn has_alphabetic(&self) -> bool {
        self.name.chars().any(|c| c.is_alphabetic())
    }

    /// The chat row that should "own" this contact in the unified UI.
    /// Preference order: WhatsApp (richer features) → Google Messages →
    /// any other source we've recorded. None if we have no chat-id at all.
    pub fn canonical_chat_id(&self) -> Option<String> {
        for src in ["whatsapp", "gmessages"] {
            if let Some(id) = self.chat_ids.get(src) {
                return Some(id.clone());
            }
        }
        self.chat_ids.values().next().cloned()
    }
}

/// Shared, lock-protected directory of phone-digits → contact entry.
/// Cheap to clone (just an `Arc`).
#[derive(Clone, Default)]
pub struct ContactDirectory {
    inner: Arc<RwLock<DirectoryInner>>,
}

#[derive(Default)]
struct DirectoryInner {
    /// Canonical full-digits key → entry.
    by_digits: HashMap<String, ContactEntry>,
    /// Secondary indices for fuzzy phone matching.
    by_suffix_10: HashMap<String, String>,
    by_suffix_7: HashMap<String, Vec<String>>,
    /// `<lid_numeric>@lid` → canonical phone digits. Built when WhatsApp's
    /// lid_phone map is fed into the directory; lets us look up a contact
    /// by the LID JID WhatsApp shows in chat rows for not-yet-resolved
    /// contacts.
    by_lid: HashMap<String, String>,
    /// Path the directory was loaded from / saves to.
    persist_path: Option<PathBuf>,
    /// Whether modified since last save.
    dirty: bool,
}

impl DirectoryInner {
    fn add_indices(&mut self, full_digits: &str) {
        if full_digits.len() >= 10 {
            let s10 = full_digits[full_digits.len() - 10..].to_string();
            self.by_suffix_10.insert(s10, full_digits.to_string());
        }
        if full_digits.len() >= 7 {
            let s7 = full_digits[full_digits.len() - 7..].to_string();
            self.by_suffix_7
                .entry(s7)
                .or_default()
                .push(full_digits.to_string());
        }
    }
}

impl ContactDirectory {
    /// Load the directory from `path`, creating an empty one if the file
    /// doesn't exist. Subsequent writes will save back to `path`.
    pub fn load(path: PathBuf) -> Self {
        let by_digits: HashMap<String, ContactEntry> = match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => decode_directory(&bytes).unwrap_or_else(|| {
                // Present but undecodable by BOTH the current and legacy
                // layouts — preserve the raw bytes so a future decoder can
                // recover them instead of letting the next save() overwrite
                // the file with an empty directory.
                backup_corrupt_once(&path);
                log::warn!(
                    "contacts: {} is undecodable (schema drift?); preserved as .corrupt, treating as empty",
                    path.display()
                );
                HashMap::new()
            }),
            // Missing or empty file → fresh empty directory (normal first run).
            _ => HashMap::new(),
        };
        let mut inner = DirectoryInner {
            by_digits: HashMap::new(),
            by_suffix_10: HashMap::new(),
            by_suffix_7: HashMap::new(),
            by_lid: HashMap::new(),
            persist_path: Some(path),
            dirty: false,
        };
        // Rebuild secondary indices from the loaded data. Skip the phone-suffix
        // indices for LID-keyed rows — their opaque numeric part must never
        // fuzzy-match a real phone. This mirrors insert()'s skip so the fix
        // survives a restart (previously every row was re-added, re-polluting
        // the indices after one restart).
        for (digits, entry) in by_digits {
            if !entry.is_lid {
                inner.add_indices(&digits);
            }
            for lid in &entry.lid_jids {
                inner.by_lid.insert(lid.clone(), digits.clone());
            }
            inner.by_digits.insert(digits, entry);
        }
        Self {
            inner: Arc::new(RwLock::new(inner)),
        }
    }

    /// In-memory only (for tests).
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one mapping. `key` can be any phone-format variant or JID;
    /// digits are extracted. `source` is a free-form tag like
    /// `"whatsapp"` or `"gmessages"` recording where the name came from.
    ///
    /// Behavior:
    /// - If no entry exists, insert.
    /// - If the existing entry is alphabetic and the new one isn't, keep
    ///   existing (don't downgrade).
    /// - Otherwise, the entry with the newer `updated_at` wins.
    /// - Either way, the source is recorded.
    pub fn insert(&self, key: &str, name: &str, source: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let digits = digits_only(key);
        if digits.is_empty() {
            return;
        }
        // A LID JID's numeric part is an opaque server id, not a phone number.
        // Store the entry keyed by its digits (so an explicit lid→phone
        // mapping can still find it), but do NOT register it in the
        // phone-suffix indices — that would let a real phone number fuzzy-match
        // the LID number and resolve to the wrong contact.
        let is_lid_key = key.ends_with("@lid");
        let mut inner = match self.inner.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let new_has_alpha = name.chars().any(|c| c.is_alphabetic());
        let new_priority = source_priority(source);

        let was_new = !inner.by_digits.contains_key(&digits);
        let entry = inner
            .by_digits
            .entry(digits.clone())
            .or_insert_with(|| inner_default_entry(name, now, source, is_lid_key));
        // Always record the source even if we don't change the name.
        entry.sources.insert(source.to_string());
        // A later LID-keyed insert on an existing row must still mark it LID so
        // load() keeps skipping the phone-suffix indices for it.
        if is_lid_key {
            entry.is_lid = true;
        }

        let mut changed = false;
        if entry.name != name {
            let existing_alpha = entry.has_alphabetic();
            let existing_priority = entry.name_priority;
            let upgrade = match (existing_alpha, new_has_alpha) {
                // Don't downgrade alpha → numeric, even from a higher-trust
                // source: a numeric string is never a real name.
                (true, false) => false,
                // Upgrade numeric → alpha (a real name always beats a
                // placeholder number).
                (false, true) => true,
                // Both alphabetic. Source trust dominates: a phonebook name
                // must never be shadowed by a longer typing/history/push name,
                // and vice-versa a higher-trust name always replaces a
                // lower-trust one regardless of length. Length/word-count is
                // only a tiebreak WITHIN the same tier (typing events often
                // emit just a first name or initial like "S", which would
                // otherwise stomp the full saved-contact name).
                (true, true) => {
                    if new_priority > existing_priority {
                        true
                    } else if new_priority < existing_priority {
                        false
                    } else {
                        let existing_words = entry.name.split_whitespace().count();
                        let new_words = name.split_whitespace().count();
                        if new_words > existing_words {
                            true
                        } else if new_words == existing_words {
                            name.len() > entry.name.len()
                        } else {
                            false
                        }
                    }
                }
                // Both numeric placeholders: higher tier wins, else recency.
                (false, false) => {
                    new_priority > existing_priority
                        || (new_priority == existing_priority && now >= entry.updated_at)
                }
            };
            if upgrade {
                entry.name = name.to_string();
                entry.updated_at = now;
                entry.name_priority = new_priority;
                changed = true;
            }
        } else if new_priority > entry.name_priority {
            // Same name arriving from a more authoritative source: raise the
            // stored tier so a later longer-but-lower-trust name can't
            // override this now-confirmed phonebook name.
            entry.name_priority = new_priority;
            changed = true;
        }
        if was_new {
            if !is_lid_key {
                inner.add_indices(&digits);
            }
            changed = true;
        }
        if changed {
            inner.dirty = true;
        }
    }

    /// Record that the WhatsApp LID JID `lid_jid` belongs to the contact
    /// at `phone_key`. After this, looking up the contact by `lid_jid`
    /// returns the same entry as looking up by phone.
    ///
    /// Idempotent. Persists.
    pub fn record_lid_jid(&self, phone_key: &str, lid_jid: &str) {
        let digits = digits_only(phone_key);
        if digits.is_empty() || lid_jid.is_empty() {
            return;
        }
        let mut inner = match self.inner.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = inner.by_digits.entry(digits.clone()).or_insert_with(|| {
            ContactEntry {
                name: lid_jid.to_string(),
                updated_at: now,
                sources: HashSet::new(),
                chat_ids: std::collections::HashMap::new(),
                lid_jids: HashSet::new(),
                // Placeholder name (a raw LID JID), lowest tier — any real
                // source can upgrade it.
                name_priority: 0,
                // Keyed by resolved phone digits, so it's a real phone row and
                // belongs in the suffix indices.
                is_lid: false,
            }
        });
        if entry.lid_jids.insert(lid_jid.to_string()) {
            inner.dirty = true;
        }
        inner.by_lid.insert(lid_jid.to_string(), digits.clone());
        if digits.len() >= 10 {
            let s10 = digits[digits.len() - 10..].to_string();
            if !inner.by_suffix_10.contains_key(&s10) {
                inner.add_indices(&digits);
            }
        }
    }

    /// Record that `chat_id` belongs to the contact at `phone_key` on the
    /// given `source` (`"whatsapp"`, `"gmessages"`, …). Used the FIRST
    /// time we see a chat — subsequent calls are no-ops if the mapping
    /// already exists. Persists.
    ///
    /// This is the "unified merge map" baked into the directory: once
    /// recorded here, the Phase 2 merger doesn't need to recompute on
    /// every startup. Lookups via `unified_chat_id()` will route both
    /// protocols to the same row.
    pub fn record_chat_id(&self, phone_key: &str, source: &str, chat_id: &str) {
        let digits = digits_only(phone_key);
        if digits.is_empty() || chat_id.is_empty() {
            return;
        }
        let mut inner = match self.inner.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = inner.by_digits.entry(digits.clone()).or_insert_with(|| {
            // No name yet — placeholder until ListContacts/sender data
            // arrives. The chat_id mapping is still useful immediately.
            ContactEntry {
                name: chat_id.to_string(),
                updated_at: now,
                sources: HashSet::new(),
                chat_ids: std::collections::HashMap::new(),
                lid_jids: HashSet::new(),
                // Placeholder name (a raw chat_id), lowest tier — any real
                // source can upgrade it.
                name_priority: 0,
                // The typing self-heal can pass a raw `@lid` as `phone_key`;
                // mark such rows so load() keeps them out of the suffix indices.
                is_lid: phone_key.ends_with("@lid"),
            }
        });
        entry.sources.insert(source.to_string());
        match entry.chat_ids.get(source) {
            Some(existing) if existing == chat_id => {} // no-op
            _ => {
                entry.chat_ids.insert(source.to_string(), chat_id.to_string());
                inner.dirty = true;
            }
        }
        // Make sure the secondary indices know about this digit string
        // even if no name was inserted yet. Skip LID keys — their opaque
        // numeric part must never pollute the phone-suffix indices.
        if !phone_key.ends_with("@lid") && digits.len() >= 10 {
            let s10 = digits[digits.len() - 10..].to_string();
            if !inner.by_suffix_10.contains_key(&s10) {
                inner.add_indices(&digits);
            }
        }
    }

    /// Look up the canonical chat row for a given phone — the one that
    /// should own the unified inbox entry across protocols.
    pub fn canonical_chat_id(&self, phone_key: &str) -> Option<String> {
        self.lookup_full(phone_key)
            .and_then(|e| e.canonical_chat_id())
    }

    /// Given a chat_id from one source, find the chat_id from another
    /// source for the same contact. Returns None if we have no record of
    /// that pair. Used by the gmessages runtime to redirect a `gm:N`
    /// chat_id to the matching WhatsApp JID.
    pub fn other_chat_id(&self, chat_id: &str, target_source: &str) -> Option<String> {
        let digits = digits_only(chat_id);
        if digits.is_empty() {
            return None;
        }
        let entry = self.lookup_full(&digits)?;
        entry.chat_ids.get(target_source).cloned()
    }

    /// Bulk insert from any iterator. Single source tag.
    pub fn extend<I, K, N>(&self, source: &str, entries: I)
    where
        I: IntoIterator<Item = (K, N)>,
        K: AsRef<str>,
        N: AsRef<str>,
    {
        for (k, n) in entries {
            self.insert(k.as_ref(), n.as_ref(), source);
        }
    }

    /// Look up by any phone-format variant or JID. Returns the canonical
    /// name. None if not present.
    ///
    /// **Fuzzy matching:** if the exact digit string isn't a hit, falls
    /// back to matching the last 10 digits (handles country-code
    /// differences like `+14164000790` vs `4164000790`), then the last 7
    /// (handles same-area-code numbers without the area code, only when
    /// unambiguous).
    pub fn lookup(&self, key: &str) -> Option<String> {
        self.lookup_full(key).map(|e| e.name)
    }

    /// Look up the full entry (for debugging or source-aware code).
    pub fn lookup_full(&self, key: &str) -> Option<ContactEntry> {
        let inner = match self.inner.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        // 0. LID JID lookup — `137340286709870@lid` → resolved phone digits.
        // A LID's numeric part is an opaque ~15-digit server id, NOT a phone
        // number. If we have an explicit lid→phone mapping, use it; otherwise
        // bail out. We must NOT fall through to digits/suffix matching, or the
        // LID number gets fuzzy-matched (by last-10/last-7) against a real
        // phone and attributes the message to a completely unrelated contact.
        if key.ends_with("@lid") {
            if let Some(canonical_digits) = inner.by_lid.get(key)
                && let Some(entry) = inner.by_digits.get(canonical_digits)
            {
                return Some(entry.clone());
            }
            return None;
        }
        let digits = digits_only(key);
        if digits.is_empty() {
            return None;
        }
        // 1. Exact full match.
        if let Some(entry) = inner.by_digits.get(&digits) {
            return Some(entry.clone());
        }
        // 2. Last-10 match (country-code-agnostic for NA-style numbers).
        if digits.len() >= 10 {
            let s10 = &digits[digits.len() - 10..];
            if let Some(canonical) = inner.by_suffix_10.get(s10)
                && let Some(entry) = inner.by_digits.get(canonical)
            {
                return Some(entry.clone());
            }
        } else if let Some(canonical) = inner.by_suffix_10.get(&digits)
            && let Some(entry) = inner.by_digits.get(canonical)
        {
            return Some(entry.clone());
        }
        // 3. Last-7 match — only commit if there's exactly one candidate,
        //    otherwise it's ambiguous and we'd rather return None than guess.
        if digits.len() >= 7 {
            let s7 = &digits[digits.len() - 7..];
            if let Some(candidates) = inner.by_suffix_7.get(s7)
                && candidates.len() == 1
                && let Some(entry) = inner.by_digits.get(&candidates[0])
            {
                return Some(entry.clone());
            }
        }
        None
    }

    /// Persist any pending changes to disk. Idempotent / cheap if nothing
    /// changed. Call from the runtime periodically.
    pub fn save_if_dirty(&self) {
        let mut inner = match self.inner.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if !inner.dirty {
            return;
        }
        let Some(path) = inner.persist_path.clone() else {
            inner.dirty = false;
            return;
        };
        let snapshot = inner.by_digits.clone();
        // Drop the lock before doing IO.
        drop(inner);
        if let Ok(bytes) = bincode::serialize(&snapshot) {
            if let Err(e) = atomic_write(&path, &bytes) {
                log::warn!("contacts: failed to persist {}: {e}", path.display());
                return;
            }
            // Re-take the lock to clear the dirty flag.
            if let Ok(mut inner) = self.inner.write() {
                inner.dirty = false;
            }
        }
    }

    pub fn len(&self) -> usize {
        match self.inner.read() {
            Ok(g) => g.by_digits.len(),
            Err(e) => e.into_inner().by_digits.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Pre-batch on-disk layout of `ContactEntry`, exactly as it was serialized
/// before `name_priority` and `is_lid` were added. bincode is field-order- and
/// field-count-sensitive and ignores `#[serde(default)]`, so a new-format
/// decode of an old file fails outright; we retry into this struct and migrate.
///
/// IMPORTANT: keep these fields byte-identical (order + types) to the historical
/// `ContactEntry`. Do NOT add the new fields here.
///
/// `Serialize` is derived only so tests can synthesize an old-format blob; the
/// production code path uses this struct purely as a decode fallback.
#[derive(Deserialize, Serialize)]
struct LegacyContactEntry {
    name: String,
    updated_at: u64,
    #[serde(default)]
    sources: HashSet<String>,
    #[serde(default)]
    chat_ids: std::collections::HashMap<String, String>,
    #[serde(default)]
    lid_jids: HashSet<String>,
}

impl From<LegacyContactEntry> for ContactEntry {
    fn from(e: LegacyContactEntry) -> Self {
        ContactEntry {
            name: e.name,
            updated_at: e.updated_at,
            sources: e.sources,
            chat_ids: e.chat_ids,
            lid_jids: e.lid_jids,
            // Fields that didn't exist in the old layout get their defaults;
            // any real source can raise the tier on the next insert.
            name_priority: 0,
            is_lid: false,
        }
    }
}

/// Decode the persisted directory, tolerating the pre-`name_priority` layout.
/// Tries the current format first; on failure retries the legacy layout and
/// migrates. Returns `None` only if BOTH decodes fail (genuine corruption).
fn decode_directory(bytes: &[u8]) -> Option<HashMap<String, ContactEntry>> {
    if let Ok(map) = bincode::deserialize::<HashMap<String, ContactEntry>>(bytes) {
        return Some(map);
    }
    // Retry the historical layout (no name_priority / is_lid fields).
    let legacy: HashMap<String, LegacyContactEntry> = bincode::deserialize(bytes).ok()?;
    Some(legacy.into_iter().map(|(k, v)| (k, v.into())).collect())
}

/// Preserve a present-but-undecodable file as `<path>.corrupt` before anything
/// can overwrite it, so the raw bytes stay recoverable by a future decoder.
/// Only copies once (won't clobber an existing `.corrupt`). Mirrors
/// `ui::runtime::backup_corrupt_once`, kept local to avoid a cross-file edit.
fn backup_corrupt_once(path: &std::path::Path) {
    let bak = path.with_extension("corrupt");
    if bak.exists() {
        return;
    }
    if let Err(e) = std::fs::copy(path, &bak) {
        log::warn!("contacts backup_corrupt_once({}): {e}", path.display());
    } else {
        log::warn!(
            "contacts: preserved undecodable {} → {} (needs a decoder to recover)",
            path.display(),
            bak.display()
        );
    }
}

/// Monotonic counter so concurrent atomic writes don't collide on the tmp name.
static ATOMIC_WRITE_CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write `data` to `path` atomically: write to a unique temp file in the same
/// directory, fsync it, then rename over the target. A crash/power-loss mid-write
/// leaves either the intact old file or the complete new one — never a truncated
/// aggregate (which previously meant a wiped contact directory). Rename within a
/// directory is atomic on Linux. Kept local to avoid a cross-file edit.
fn atomic_write(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
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

fn inner_default_entry(name: &str, now: u64, source: &str, is_lid: bool) -> ContactEntry {
    let mut sources = HashSet::new();
    sources.insert(source.to_string());
    ContactEntry {
        name: name.to_string(),
        updated_at: now,
        sources,
        chat_ids: std::collections::HashMap::new(),
        lid_jids: HashSet::new(),
        name_priority: source_priority(source),
        is_lid,
    }
}

/// Trust tier for a name source. Higher wins. A name from a higher tier is
/// never overwritten by a lower tier (regardless of length), so a saved
/// phonebook contact name can't be shadowed by a longer typing/history/
/// push-name string.
///
///   2 — phonebook / saved contacts (authoritative): the WhatsApp contacts
///       map, Google Messages ListContacts, lid→phone contact names, and any
///       explicit user rename / ContactUpdate (tags containing "contact" or
///       tagged "whatsapp"/"gmessages").
///   1 — message-derived / push names (e.g. a sender's self-chosen display
///       name learned from an incoming message).
///   0 — ephemeral / low-trust (typing events) and unknown sources.
pub fn source_priority(source: &str) -> u8 {
    // Message/push sources first, so a "gmessages-msg" tag isn't caught by the
    // broad "gmessages" phonebook check below.
    if source.contains("msg")
        || source.contains("push")
        || source.contains("history")
        || source.contains("sender")
    {
        return 1;
    }
    if source == "typing" {
        return 0;
    }
    if source.contains("contact")
        || source == "whatsapp"
        || source == "gmessages"
        || source == "whatsapp-lid-phone"
    {
        return 2;
    }
    // Unknown / unclassified source: treat as low trust so it can be upgraded
    // but doesn't stomp a phonebook name.
    0
}

/// Strip everything that isn't a digit. Strips leading `+`, parens,
/// dashes, spaces, and the `@s.whatsapp.net` / `@lid` / `@g.us` JID suffix.
/// Returns "" for inputs with no digits.
pub fn digits_only(s: &str) -> String {
    let pre_at = match s.find('@') {
        Some(i) => &s[..i],
        None => s,
    };
    pre_at.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Process-wide singleton, persisted to `<data_dir>/contacts_directory.bin`.
///
/// First call MUST happen after the desktop has chdir'd into its data dir
/// (so `PathBuf::from(...)` resolves correctly). Subsequent calls return
/// the same instance.
static GLOBAL: OnceLock<ContactDirectory> = OnceLock::new();

pub fn global() -> &'static ContactDirectory {
    GLOBAL.get_or_init(|| ContactDirectory::load(PathBuf::from("contacts_directory.bin")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_only_handles_jid() {
        assert_eq!(digits_only("14164000790@s.whatsapp.net"), "14164000790");
        assert_eq!(digits_only("1234@lid"), "1234");
        assert_eq!(digits_only("+14164000790"), "14164000790");
        assert_eq!(digits_only("+1 (416) 400-0790"), "14164000790");
        assert_eq!(digits_only("Bob"), "");
    }

    #[test]
    fn lookup_normalizes_variants() {
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "Lorne", "test");
        assert_eq!(dir.lookup("+1 (416) 400-0790").as_deref(), Some("Lorne"));
        assert_eq!(dir.lookup("14164000790@s.whatsapp.net").as_deref(), Some("Lorne"));
        assert_eq!(dir.lookup("16472876066").as_deref(), None);
    }

    #[test]
    fn alpha_beats_numeric_regardless_of_recency() {
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "Lorne", "first");
        dir.insert("14164000790", "+1 416-400-0790", "second");
        assert_eq!(dir.lookup("14164000790").as_deref(), Some("Lorne"));
    }

    #[test]
    fn numeric_can_be_replaced_by_alpha() {
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "+1 416-400-0790", "first");
        dir.insert("14164000790", "Lorne", "second");
        assert_eq!(dir.lookup("14164000790").as_deref(), Some("Lorne"));
    }

    #[test]
    fn sticky_after_re_insert_with_empty() {
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "Lorne", "first");
        dir.insert("14164000790", "", "second"); // ignored
        dir.insert("16472876066", "Agnes", "second");
        // Lorne still there even though "second" sync didn't include them.
        assert_eq!(dir.lookup("14164000790").as_deref(), Some("Lorne"));
        assert_eq!(dir.lookup("16472876066").as_deref(), Some("Agnes"));
    }

    #[test]
    fn longer_name_replaces_shorter() {
        // Typing event emits just a first name; saved contact has full name.
        // The longer one should win regardless of insertion order.
        let dir = ContactDirectory::new();
        dir.insert("14164566960", "Jake", "typing");
        dir.insert("14164566960", "Jake Steinman", "whatsapp-contacts");
        assert_eq!(dir.lookup("14164566960").as_deref(), Some("Jake Steinman"));
    }

    #[test]
    fn shorter_name_does_not_replace_longer() {
        // Reverse order: save full name first, then a typing event fires
        // with just the first name.
        let dir = ContactDirectory::new();
        dir.insert("16475311689", "Saad Suleman", "whatsapp-contacts");
        dir.insert("16475311689", "Saad", "typing"); // would-be regression
        dir.insert("16475311689", "S", "typing");    // even worse
        assert_eq!(dir.lookup("16475311689").as_deref(), Some("Saad Suleman"));
    }

    #[test]
    fn single_word_initial_does_not_replace_two_word_name() {
        let dir = ContactDirectory::new();
        dir.insert("18005551212", "agnes contact", "whatsapp");
        dir.insert("18005551212", "agnes", "typing");
        assert_eq!(dir.lookup("18005551212").as_deref(), Some("agnes contact"));
    }

    #[test]
    fn fuzzy_country_code() {
        let dir = ContactDirectory::new();
        // Stored with country code.
        dir.insert("+14164000790", "Lorne", "whatsapp");
        // Looked up without — should still hit.
        assert_eq!(dir.lookup("4164000790").as_deref(), Some("Lorne"));
        assert_eq!(dir.lookup("(416) 400-0790").as_deref(), Some("Lorne"));
    }

    #[test]
    fn fuzzy_local_number() {
        let dir = ContactDirectory::new();
        dir.insert("+14164000790", "Lorne", "src");
        // 7-digit local lookup (drops area code) — only one candidate.
        assert_eq!(dir.lookup("4000790").as_deref(), Some("Lorne"));
    }

    #[test]
    fn fuzzy_local_ambiguous_returns_none() {
        let dir = ContactDirectory::new();
        dir.insert("+14164000790", "Lorne", "s");
        dir.insert("+12124000790", "Bob", "s");
        // Two contacts share last 7 digits → don't guess.
        assert_eq!(dir.lookup("4000790"), None);
    }

    #[test]
    fn sources_accumulate() {
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "Lorne", "whatsapp");
        dir.insert("14164000790", "Lorne", "gmessages");
        let entry = dir.lookup_full("14164000790").unwrap();
        assert!(entry.sources.contains("whatsapp"));
        assert!(entry.sources.contains("gmessages"));
    }

    #[test]
    fn lid_jid_resolves_to_named_contact() {
        // Joe Mysak case: WhatsApp gives anonymous LID like
        // `137340286709870@lid`. The lid→phone map links it to a phone
        // JID, and ListContacts has a name for the phone. The directory
        // should find the name when looked up by the raw LID JID.
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "Joe Mysak", "whatsapp-listcontacts");
        dir.record_lid_jid("14164000790@s.whatsapp.net", "137340286709870@lid");
        assert_eq!(
            dir.lookup("137340286709870@lid").as_deref(),
            Some("Joe Mysak"),
        );
    }

    #[test]
    fn unmapped_lid_never_fuzzy_matches_a_phone_contact() {
        // A LID's numeric part is an opaque ~15-digit server id, not a phone.
        // It must NOT resolve to an unrelated contact via last-10/last-7
        // suffix fuzzy matching.
        let dir = ContactDirectory::new();
        dir.insert("14164000790", "Lorne", "whatsapp");
        // Craft a LID whose last 10 digits collide with Lorne's number.
        let lid = "999994164000790@lid"; // last 10 = 4164000790
        assert_eq!(dir.lookup(lid), None);
    }

    #[test]
    fn inserting_by_lid_key_does_not_pollute_phone_suffix_index() {
        // The typing self-heal can call insert(&chat_id, ...) where chat_id
        // is a raw @lid. That must not register the LID number in the
        // phone-suffix indices, or a real phone lookup could fuzzy-match it.
        let dir = ContactDirectory::new();
        dir.insert("137340286709870@lid", "Ghost", "typing");
        // A phone whose last-10 digits equal the LID number's last-10
        // ("6286709870") must NOT fuzzy-match "Ghost".
        assert_eq!(dir.lookup("15556286709870"), None);
        assert_eq!(dir.lookup("6286709870"), None);
        // Looking up the raw LID still works only via an explicit lid→phone
        // mapping (which we never recorded here), so it stays None too.
        assert_eq!(dir.lookup("137340286709870@lid"), None);
    }

    #[test]
    fn phonebook_name_not_shadowed_by_longer_low_trust_name() {
        // Core longer-name-heuristic bug: a longer string from a low-trust
        // source (typing/history/push) must NOT lock out a shorter phonebook
        // name, in either insertion order.
        let dir = ContactDirectory::new();
        // Low-trust longer name arrives first.
        dir.insert("14165551234", "Craigy🔥 the Best", "gmessages-msg");
        // Phonebook contact name (shorter) arrives later — should win.
        dir.insert("14165551234", "Craig Thompson", "gmessages");
        assert_eq!(dir.lookup("14165551234").as_deref(), Some("Craig Thompson"));
    }

    #[test]
    fn low_trust_longer_name_cannot_override_phonebook() {
        // Reverse order: phonebook name first, then a longer low-trust push
        // name — the phonebook name must stick.
        let dir = ContactDirectory::new();
        dir.insert("14165551234", "Craig Thompson", "gmessages");
        dir.insert("14165551234", "Craigy🔥 the Legend", "gmessages-msg");
        assert_eq!(dir.lookup("14165551234").as_deref(), Some("Craig Thompson"));
    }

    #[test]
    fn same_tier_still_prefers_longer_name() {
        // Within the same trust tier the length/word heuristic still applies.
        let dir = ContactDirectory::new();
        dir.insert("14165550000", "Saad", "gmessages");
        dir.insert("14165550000", "Saad Suleman", "whatsapp");
        assert_eq!(dir.lookup("14165550000").as_deref(), Some("Saad Suleman"));
    }

    #[test]
    fn confirmed_name_from_authoritative_source_blocks_later_low_trust() {
        // Same name confirmed by a phonebook source raises its tier so a
        // later longer low-trust name can't override it.
        let dir = ContactDirectory::new();
        dir.insert("14165559999", "Bob", "typing"); // tier 0
        dir.insert("14165559999", "Bob", "gmessages"); // confirm → tier 2
        dir.insert("14165559999", "Bob Longer Nickname", "typing"); // tier 0
        assert_eq!(dir.lookup("14165559999").as_deref(), Some("Bob"));
    }

    #[test]
    fn legacy_blob_decodes_and_migrates_without_wiping() {
        // Simulate a contacts_directory.bin written before name_priority /
        // is_lid existed: serialize a HashMap<String, LegacyContactEntry> and
        // prove decode_directory() reads it back (not wiped) with sane
        // migrated defaults.
        let mut legacy: HashMap<String, LegacyContactEntry> = HashMap::new();
        legacy.insert(
            "14164000790".to_string(),
            LegacyContactEntry {
                name: "Lorne".to_string(),
                updated_at: 42,
                sources: HashSet::from(["whatsapp".to_string()]),
                chat_ids: std::collections::HashMap::from([(
                    "whatsapp".to_string(),
                    "14164000790@s.whatsapp.net".to_string(),
                )]),
                lid_jids: HashSet::from(["137340286709870@lid".to_string()]),
            },
        );
        let bytes = bincode::serialize(&legacy).unwrap();

        // A current-format decode of the OLD bytes must fail (this is the very
        // bug C2 guards against); the fallback must succeed.
        assert!(
            bincode::deserialize::<HashMap<String, ContactEntry>>(&bytes).is_err(),
            "old layout should not decode as the new struct (else the test proves nothing)"
        );
        let map = decode_directory(&bytes).expect("legacy fallback must decode old file");
        let entry = map.get("14164000790").expect("row must survive migration");
        assert_eq!(entry.name, "Lorne");
        assert_eq!(entry.updated_at, 42);
        assert_eq!(entry.name_priority, 0); // migrated default
        assert!(!entry.is_lid); // migrated default
        assert!(entry.lid_jids.contains("137340286709870@lid"));
    }

    #[test]
    fn current_format_round_trips_through_decode() {
        // A freshly serialized new-format directory decodes via the primary
        // path (not the legacy fallback), preserving new fields.
        let mut map: HashMap<String, ContactEntry> = HashMap::new();
        map.insert(
            "137340286709870".to_string(),
            ContactEntry {
                name: "Ghost".to_string(),
                updated_at: 7,
                sources: HashSet::new(),
                chat_ids: std::collections::HashMap::new(),
                lid_jids: HashSet::new(),
                name_priority: 2,
                is_lid: true,
            },
        );
        let bytes = bincode::serialize(&map).unwrap();
        let decoded = decode_directory(&bytes).expect("new format must decode");
        let e = decoded.get("137340286709870").unwrap();
        assert_eq!(e.name_priority, 2);
        assert!(e.is_lid);
    }

    #[test]
    fn is_lid_row_skips_suffix_index_on_load() {
        // A LID-keyed row persisted with is_lid=true must NOT be re-added to the
        // phone-suffix indices on load (the G1 restart regression). Round-trip
        // through a temp file via load() and confirm no fuzzy match.
        let dir_path = std::env::temp_dir().join(format!(
            "contacts_islid_test_{}_{}.bin",
            std::process::id(),
            ATOMIC_WRITE_CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut map: HashMap<String, ContactEntry> = HashMap::new();
        map.insert(
            "137340286709870".to_string(),
            ContactEntry {
                name: "Ghost".to_string(),
                updated_at: 1,
                sources: HashSet::new(),
                chat_ids: std::collections::HashMap::new(),
                lid_jids: HashSet::new(),
                name_priority: 0,
                is_lid: true,
            },
        );
        std::fs::write(&dir_path, bincode::serialize(&map).unwrap()).unwrap();

        let loaded = ContactDirectory::load(dir_path.clone());
        // The LID digits' last-10 ("6286709870") must not fuzzy-match.
        assert_eq!(loaded.lookup("15556286709870"), None);
        assert_eq!(loaded.lookup("6286709870"), None);
        let _ = std::fs::remove_file(&dir_path);
    }

    #[test]
    fn source_priority_tiers() {
        assert_eq!(source_priority("gmessages"), 2);
        assert_eq!(source_priority("whatsapp"), 2);
        assert_eq!(source_priority("whatsapp-listcontacts"), 2);
        assert_eq!(source_priority("whatsapp-lid-phone"), 2);
        assert_eq!(source_priority("gmessages-msg"), 1);
        assert_eq!(source_priority("push-name"), 1);
        assert_eq!(source_priority("typing"), 0);
        assert_eq!(source_priority("unknown-src"), 0);
    }
}
