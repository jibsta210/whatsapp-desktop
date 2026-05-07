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

/// One contact entry. Persisted to disk so we never forget a name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContactEntry {
    pub name: String,
    /// Unix seconds since epoch when this entry was last upgraded.
    pub updated_at: u64,
    /// Set of upstream sources that have contributed this name (e.g.
    /// `"whatsapp"`, `"gmessages"`). Used to debug provenance.
    #[serde(default)]
    pub sources: HashSet<String>,
    /// Chat-id-per-source mapping. The merge map for unified rows: when
    /// gmessages and WhatsApp both reach the same person, both chat IDs
    /// land here, and `canonical_chat_id()` resolves to WhatsApp first.
    ///   `"whatsapp"` → `"14164000790@s.whatsapp.net"`
    ///   `"gmessages"` → `"gm:14"`
    /// Built up the first time we see each chat — never recomputed.
    #[serde(default)]
    pub chat_ids: std::collections::HashMap<String, String>,
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
    /// Secondary index: last-10-digits → canonical key. Built so
    /// `+14164000790`, `14164000790`, and `4164000790` all hit the same
    /// entry (covers country-code variants for NA-style 10-digit numbers).
    /// Last 7 digits also indexed for very fuzzy fallback (e.g. local
    /// numbers without area code), but only when unambiguous.
    by_suffix_10: HashMap<String, String>,
    by_suffix_7: HashMap<String, Vec<String>>,
    /// Path the directory was loaded from / saves to. None when running
    /// without persistence (tests).
    persist_path: Option<PathBuf>,
    /// Whether the directory has been modified since the last save.
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
        let by_digits: HashMap<String, ContactEntry> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
            .unwrap_or_default();
        let mut inner = DirectoryInner {
            by_digits: HashMap::new(),
            by_suffix_10: HashMap::new(),
            by_suffix_7: HashMap::new(),
            persist_path: Some(path),
            dirty: false,
        };
        // Rebuild secondary indices from the loaded data.
        for (digits, entry) in by_digits {
            inner.add_indices(&digits);
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
        let mut inner = match self.inner.write() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let new_has_alpha = name.chars().any(|c| c.is_alphabetic());

        let was_new = !inner.by_digits.contains_key(&digits);
        let entry = inner
            .by_digits
            .entry(digits.clone())
            .or_insert_with(|| inner_default_entry(name, now, source));
        // Always record the source even if we don't change the name.
        entry.sources.insert(source.to_string());

        let mut changed = false;
        if entry.name != name {
            let existing_alpha = entry.has_alphabetic();
            let upgrade = match (existing_alpha, new_has_alpha) {
                // Don't downgrade alpha → numeric.
                (true, false) => false,
                // Upgrade numeric → alpha.
                (false, true) => true,
                // Both alphabetic: prefer the LONGER name. Typing events
                // often emit just a first name or initial (e.g. "S"),
                // which would otherwise stomp the full saved-contact
                // name. Length is a coarse proxy for "more complete".
                (true, true) => {
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
                // Both numeric: tiebreak by recency.
                (false, false) => now >= entry.updated_at,
            };
            if upgrade {
                entry.name = name.to_string();
                entry.updated_at = now;
                changed = true;
            }
        }
        if was_new {
            inner.add_indices(&digits);
            changed = true;
        }
        if changed {
            inner.dirty = true;
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
        // even if no name was inserted yet.
        if digits.len() >= 10 {
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
        let digits = digits_only(key);
        if digits.is_empty() {
            return None;
        }
        let inner = match self.inner.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
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
            if let Err(e) = std::fs::write(&path, bytes) {
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

fn inner_default_entry(name: &str, now: u64, source: &str) -> ContactEntry {
    let mut sources = HashSet::new();
    sources.insert(source.to_string());
    ContactEntry {
        name: name.to_string(),
        updated_at: now,
        sources,
        chat_ids: std::collections::HashMap::new(),
    }
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
}
