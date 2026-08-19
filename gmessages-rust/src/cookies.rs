//! Firefox cookie reader for Gaia (Google account) pairing.
//!
//! Firefox stores its cookies in a SQLite database (`cookies.sqlite`) inside
//! the user's profile directory. The DB is plaintext (unlike Chrome / Brave,
//! which encrypt it via libsecret). This module:
//!
//! 1. Locates the default Firefox profile.
//! 2. Copies `cookies.sqlite` to a tempfile (Firefox holds an exclusive WAL
//!    lock on the live DB while running).
//! 3. SELECTs the Google session cookies needed for `SignInGaia`.
//!
//! The user must already be signed into <https://messages.google.com/web/>
//! in Firefox. If they aren't, the cookies we need won't be present and
//! [`read_firefox_google_cookies`] returns an empty map (the caller should
//! prompt the user to sign in).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::{Error, Result};

/// Cookie names we care about for Gaia pairing. These are the standard
/// Google session cookies set on `*.google.com` after a successful login.
/// Without `__Secure-1PSID` and `SAPISID` the SignInGaia call will fail.
pub const GAIA_COOKIE_NAMES: &[&str] = &[
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PSID",
    "__Secure-3PSID",
    "__Secure-1PSIDTS",
    "__Secure-3PSIDTS",
    "__Secure-1PSIDCC",
    "__Secure-3PSIDCC",
    "__Secure-1PAPISID",
    "__Secure-3PAPISID",
    "NID",
    "OSID",
    "LSID",
    "SIDCC",
];

/// Locate the default Firefox profile directory. Tries the standard locations
/// and a couple of distro-specific variants.
pub fn find_default_firefox_profile() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let candidates = [
        // Standard
        PathBuf::from(&home).join(".mozilla/firefox"),
        // Some Linux distros (jakes' setup is one) put Firefox profiles
        // under ~/.config/mozilla/firefox.
        PathBuf::from(&home).join(".config/mozilla/firefox"),
        // Snap / Flatpak relocations
        PathBuf::from(&home).join("snap/firefox/common/.mozilla/firefox"),
        PathBuf::from(&home).join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ];

    for root in &candidates {
        if let Some(profile) = find_profile_in_root(root) {
            return Some(profile);
        }
    }
    None
}

/// Inside a `firefox/` root, find the active profile. Prefers
/// `*.default-release`, then `*.default`, then any directory containing
/// a `cookies.sqlite`.
fn find_profile_in_root(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("cookies.sqlite").exists() {
            candidates.push(path);
        }
    }
    // Sort so `*.default-release` ranks above `*.default` and others.
    candidates.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".default-release") {
            0
        } else if name.ends_with(".default") {
            1
        } else {
            2
        }
    });
    candidates.into_iter().next()
}

/// Read Google session cookies from a Firefox profile. Returns an empty map
/// if the user isn't signed into Google (no relevant cookies present).
///
/// Firefox holds an exclusive lock on the cookie DB while running. We copy
/// all three SQLite files — `cookies.sqlite`, `cookies.sqlite-wal`, and
/// `cookies.sqlite-shm` (if present) — into a tempdir so SQLite can
/// reconstruct the WAL-up-to-date view. Without the WAL copy, freshly
/// rotated cookies that Firefox hasn't checkpointed yet are invisible to
/// us, and we end up using stale cookies for hours after each rotation.
pub fn read_firefox_google_cookies(profile_dir: &Path) -> Result<HashMap<String, String>> {
    let src = profile_dir.join("cookies.sqlite");
    if !src.exists() {
        return Err(Error::Pairing(format!(
            "no cookies.sqlite in firefox profile: {}",
            profile_dir.display()
        )));
    }

    let tmp_dir = tempfile::Builder::new()
        .prefix("ffcookies-")
        .tempdir()
        .map_err(|e| Error::Pairing(format!("create tempdir: {e}")))?;
    let tmp_main = tmp_dir.path().join("cookies.sqlite");
    std::fs::copy(&src, &tmp_main)
        .map_err(|e| Error::Pairing(format!("copy cookies.sqlite: {e}")))?;
    // Best-effort WAL copies — missing files just mean FF has checkpointed.
    for sidecar in ["cookies.sqlite-wal", "cookies.sqlite-shm"] {
        let sc_src = profile_dir.join(sidecar);
        if sc_src.exists() {
            let _ = std::fs::copy(&sc_src, tmp_dir.path().join(sidecar));
        }
    }
    let conn = Connection::open_with_flags(
        &tmp_main,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| Error::Pairing(format!("open cookies DB: {e}")))?;

    // Build the IN-list. Use bound parameters; rusqlite's `?` doesn't
    // expand to a list, so we generate `?,?,?,...`.
    let placeholders = (0..GAIA_COOKIE_NAMES.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT name, value FROM moz_cookies \
         WHERE host LIKE '%.google.com' AND name IN ({placeholders})"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Error::Pairing(format!("prepare select: {e}")))?;

    let params: Vec<&dyn rusqlite::ToSql> = GAIA_COOKIE_NAMES
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();

    let mut rows = stmt
        .query(&*params)
        .map_err(|e| Error::Pairing(format!("query cookies: {e}")))?;

    let mut out: HashMap<String, String> = HashMap::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| Error::Pairing(format!("read cookie row: {e}")))?
    {
        let name: String = row
            .get(0)
            .map_err(|e| Error::Pairing(format!("cookie name: {e}")))?;
        let value: String = row
            .get(1)
            .map_err(|e| Error::Pairing(format!("cookie value: {e}")))?;
        // Prefer the longest value if a cookie is present on multiple
        // hosts (e.g. `.google.com` vs `accounts.google.com`).
        match out.get(&name) {
            Some(existing) if existing.len() >= value.len() => {}
            _ => {
                out.insert(name, value);
            }
        }
    }

    Ok(out)
}

/// Cookies Google rotates daily as part of session refresh. If the
/// freshest of these is more than ~22h old, the desktop is risking the
/// server-side TTL cliff — we want to nudge Firefox to refresh them
/// before that.
const ROTATING_COOKIES: &[&str] = &[
    "__Secure-1PSIDTS",
    "__Secure-3PSIDTS",
    "__Secure-1PSIDCC",
    "__Secure-3PSIDCC",
    "SIDCC",
];

/// Find the age of the freshest "rotating" Google session cookie. A
/// small age means Firefox refreshed cookies recently (we're healthy);
/// a large age means Firefox hasn't touched the session in a while and
/// the cookies may be approaching server-side expiry. Returns `None` if
/// no rotating cookies are present (user not signed in, or Firefox not
/// installed).
pub fn rotating_cookie_max_age() -> Option<std::time::Duration> {
    let profile = find_default_firefox_profile()?;
    let src = profile.join("cookies.sqlite");
    if !src.exists() {
        return None;
    }
    let tmp_dir = tempfile::Builder::new().prefix("ffage-").tempdir().ok()?;
    let tmp_main = tmp_dir.path().join("cookies.sqlite");
    std::fs::copy(&src, &tmp_main).ok()?;
    for sidecar in ["cookies.sqlite-wal", "cookies.sqlite-shm"] {
        let sc_src = profile.join(sidecar);
        if sc_src.exists() {
            let _ = std::fs::copy(&sc_src, tmp_dir.path().join(sidecar));
        }
    }
    let conn = Connection::open_with_flags(
        &tmp_main,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let placeholders = (0..ROTATING_COOKIES.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    // creationTime in moz_cookies is microseconds since UNIX epoch.
    let sql = format!(
        "SELECT MAX(creationTime) FROM moz_cookies \
         WHERE host LIKE '%.google.com' AND name IN ({placeholders})"
    );
    let params: Vec<&dyn rusqlite::ToSql> = ROTATING_COOKIES
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql).ok()?;
    let max_creation_us: i64 = stmt
        .query_row(&*params, |row| row.get::<_, Option<i64>>(0))
        .ok()??;
    if max_creation_us <= 0 {
        return None;
    }
    let creation_secs = (max_creation_us / 1_000_000) as u64;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(std::time::Duration::from_secs(
        now_secs.saturating_sub(creation_secs),
    ))
}

/// Forcefully invalidate the in-memory cookie cache so the next
/// `get_cached_firefox_cookies` call hits SQLite. Used after we trigger
/// Firefox to refresh — we want the very next request to pick up any
/// new rotations FF just performed.
pub fn invalidate_cookie_cache() {
    if let Some(c) = COOKIE_CACHE.get()
        && let Ok(mut g) = c.lock()
    {
        // Nothing to invalidate against when we own the session: the only
        // source is us, and dropping `last_read` would just make the next
        // read fall through to Firefox.
        if g.session_owned {
            return;
        }
        g.last_read = None;
    }
}

/// Install cookies from a sign-in this application performed itself, and
/// take ownership of the session from that point on.
///
/// Before this existed the only way to get a Gaia session was to read the
/// user's Firefox profile — which meant the bridge silently depended on
/// Firefox being installed, signed into the right Google account, and not
/// logged out. A session we obtained ourselves has none of those
/// couplings, so once one is adopted we stop consulting the browser
/// entirely and let Google's own `Set-Cookie` rotations keep it alive.
///
/// Filtered to [`GAIA_COOKIE_NAMES`] for the same reason
/// [`merge_into_cache`] is: a login flow hands back a pile of consent and
/// tracking cookies we have no business retaining. Adopting an empty set
/// is refused, so a failed login cannot silently disable the fallback.
pub fn adopt_login_session(cookies: HashMap<String, String>) -> Result<()> {
    let kept: HashMap<String, String> = cookies
        .into_iter()
        .filter(|(k, _)| GAIA_COOKIE_NAMES.contains(&k.as_str()))
        .collect();
    if !kept.contains_key("__Secure-1PSID") || !kept.contains_key("SAPISID") {
        return Err(Error::Pairing(
            "login session is missing __Secure-1PSID/SAPISID; not adopting".into(),
        ));
    }
    let cache = COOKIE_CACHE.get_or_init(|| {
        std::sync::Mutex::new(CookieCache {
            last_read: None,
            cookies: HashMap::new(),
            session_owned: false,
        })
    });
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    log::info!("cookies: adopting own login session ({} entries)", kept.len());
    guard.cookies = kept;
    guard.last_read = Some(std::time::Instant::now());
    guard.session_owned = true;
    Ok(())
}

/// Whether a session obtained by [`adopt_login_session`] is currently
/// installed. Used by the desktop UI to decide whether to show the
/// "sign in with Google" prompt or the legacy Firefox hint.
pub fn has_owned_session() -> bool {
    COOKIE_CACHE
        .get()
        .and_then(|c| c.lock().ok().map(|g| g.session_owned))
        .unwrap_or(false)
}

/// Drop an adopted session and fall back to reading the browser profile.
/// Used on sign-out and when the relay reports the session was revoked.
pub fn clear_owned_session() {
    if let Some(c) = COOKIE_CACHE.get()
        && let Ok(mut g) = c.lock()
    {
        g.session_owned = false;
        g.cookies.clear();
        g.last_read = None;
    }
}

/// Merge a batch of cookie updates (typically parsed from `Set-Cookie`
/// response headers) directly into the in-memory cache. Marks the cache
/// as fresh, so the next request uses these new values without hitting
/// SQLite again.
///
/// This is how we "rotate cookies ourselves" while running: any Google
/// response that includes `Set-Cookie: __Secure-1PSIDTS=NEW; ...`
/// (which Google sends opportunistically to refresh the session) gets
/// captured here and the next request ships the new value. No need to
/// spin up Firefox just to nudge a rotation.
pub fn merge_into_cache(updates: HashMap<String, String>) {
    if updates.is_empty() {
        return;
    }
    let cache = COOKIE_CACHE.get_or_init(|| {
        std::sync::Mutex::new(CookieCache {
            last_read: None,
            cookies: HashMap::new(),
            session_owned: false,
        })
    });
    if let Ok(mut g) = cache.lock() {
        let mut changed = 0;
        for (k, v) in updates {
            // Only honor names we already know about — avoids polluting
            // the cache with tracking cookies / consent cookies / etc.
            // we don't care about. If a brand-new rotating cookie shows
            // up that we DON'T know about yet, add it here.
            if GAIA_COOKIE_NAMES.contains(&k.as_str()) {
                if g.cookies.get(&k) != Some(&v) {
                    changed += 1;
                }
                g.cookies.insert(k, v);
            }
        }
        if changed > 0 {
            g.last_read = Some(std::time::Instant::now());
            log::debug!(
                "cookies: merged {changed} rotated cookie(s) from response (cache now {} entries)",
                g.cookies.len()
            );
        }
    }
}

/// Parse a list of `Set-Cookie` header values (`"name=value; Domain=...; Path=..."`)
/// into a name → value map. Best-effort: malformed entries are skipped.
pub fn parse_set_cookie_headers<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for raw in values {
        // The cookie name/value pair is everything up to the first `;`.
        let pair = raw.split(';').next().unwrap_or(raw).trim();
        if let Some((name, value)) = pair.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            if !name.is_empty() && !value.is_empty() {
                out.insert(name.to_string(), value.to_string());
            }
        }
    }
    out
}

/// In-memory cache of the last live cookie read. Refreshed lazily by
/// [`get_cached_firefox_cookies`] with a short TTL so per-request reads
/// don't pummel SQLite during bursty traffic. The cache holds the
/// last-successful read forever as a fallback for when Firefox is shut
/// down — we'd rather use slightly-stale-but-recent cookies than nothing.
struct CookieCache {
    last_read: Option<std::time::Instant>,
    cookies: HashMap<String, String>,
    /// Set once the caller has supplied cookies from a real sign-in of our
    /// own (see [`adopt_login_session`]). While true, this jar IS the
    /// session and Firefox is never consulted — the app owns its login
    /// instead of borrowing the browser's.
    session_owned: bool,
}

static COOKIE_CACHE: std::sync::OnceLock<std::sync::Mutex<CookieCache>> =
    std::sync::OnceLock::new();

/// Read Firefox cookies, using an in-memory cache with a 30s TTL. Always
/// returns the freshest copy we can get; if the live read fails (FF shut
/// down, lock contention, etc.) we fall back to whatever was last cached.
/// Returns an empty map only if nothing has EVER succeeded.
///
/// This is the public hot path. Every HTTP request that needs Google
/// cookies should go through here — no need to plumb cookie snapshots
/// through `AuthData` anymore.
pub fn get_cached_firefox_cookies() -> HashMap<String, String> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(30);
    let cache = COOKIE_CACHE.get_or_init(|| {
        std::sync::Mutex::new(CookieCache {
            last_read: None,
            cookies: HashMap::new(),
            session_owned: false,
        })
    });
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    // We own the login: this jar is the session. Google rotates it for us
    // via Set-Cookie (see `merge_into_cache`), so there is nothing Firefox
    // could tell us that we do not already know — and reading it here would
    // overwrite our fresher values with whatever some browser profile last
    // happened to persist.
    if guard.session_owned {
        return guard.cookies.clone();
    }
    // Fast path: cache hit.
    if let Some(t) = guard.last_read
        && t.elapsed() < TTL
        && !guard.cookies.is_empty()
    {
        return guard.cookies.clone();
    }
    // Slow path: read live, update cache.
    match read_default_firefox_cookies() {
        Ok(fresh) => {
            log::debug!(
                "cookies: refreshed cache live from FF ({} entries)",
                fresh.len()
            );
            guard.cookies = fresh.clone();
            guard.last_read = Some(std::time::Instant::now());
            fresh
        }
        Err(e) => {
            log::warn!(
                "cookies: live FF read failed ({e}); using cached {} entries",
                guard.cookies.len()
            );
            guard.cookies.clone()
        }
    }
}

/// Convenience: find the default Firefox profile and read its Google cookies.
/// Returns `Error::Pairing` with a descriptive message if no profile or no
/// cookies are found.
pub fn read_default_firefox_cookies() -> Result<HashMap<String, String>> {
    let profile = find_default_firefox_profile().ok_or_else(|| {
        Error::Pairing(
            "no Firefox profile found. Install Firefox and sign into messages.google.com.".into(),
        )
    })?;
    log::info!(
        "gaia: reading Google cookies from Firefox profile: {}",
        profile.display()
    );
    let cookies = read_firefox_google_cookies(&profile)?;
    if cookies.is_empty() {
        return Err(Error::Pairing(
            "no Google session cookies in Firefox profile. Sign into https://messages.google.com/web/ first."
                .into(),
        ));
    }
    // We need at least these two for SignInGaia to succeed.
    if !cookies.contains_key("SAPISID") {
        return Err(Error::Pairing(
            "Firefox has Google cookies but SAPISID is missing. Sign in fully and retry.".into(),
        ));
    }
    log::info!("gaia: found {} Google cookies in Firefox", cookies.len());
    Ok(cookies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn a_valid_session() -> HashMap<String, String> {
        session(&[
            ("__Secure-1PSID", "psid-value"),
            ("SAPISID", "sapisid-value"),
            ("SID", "sid-value"),
        ])
    }

    // These four share the process-wide COOKIE_CACHE, so they run as one
    // test: as separate #[test] fns they would race each other's ownership
    // flag under cargo's thread-per-test default.
    #[test]
    fn owned_session_lifecycle() {
        clear_owned_session();
        assert!(!has_owned_session());

        // A jar that is not a usable Google session must not take
        // ownership — otherwise a failed login silently disables the
        // Firefox fallback and the bridge just stops authenticating.
        assert!(adopt_login_session(session(&[("NID", "n")])).is_err());
        assert!(adopt_login_session(HashMap::new()).is_err());
        assert!(!has_owned_session());

        // Adopting keeps only the Google session cookies; a login flow
        // hands back consent/tracking cookies we have no business storing.
        let mut noisy = a_valid_session();
        noisy.insert("_ga".into(), "tracking".into());
        noisy.insert("CONSENT".into(), "yes".into());
        adopt_login_session(noisy).unwrap();
        assert!(has_owned_session());

        let jar = get_cached_firefox_cookies();
        assert_eq!(jar.get("__Secure-1PSID").map(String::as_str), Some("psid-value"));
        assert!(!jar.contains_key("_ga"), "tracking cookie was retained");
        assert!(!jar.contains_key("CONSENT"), "consent cookie was retained");

        // Google rotates the session for us in flight; the new value must
        // win, and must not be undone by a subsequent read.
        merge_into_cache(session(&[("__Secure-1PSIDTS", "rotated")]));
        assert_eq!(
            get_cached_firefox_cookies()
                .get("__Secure-1PSIDTS")
                .map(String::as_str),
            Some("rotated"),
        );

        // Invalidation is what would otherwise send us back to the browser
        // profile mid-session, so it has to be inert while we own the login.
        invalidate_cookie_cache();
        assert!(has_owned_session());
        assert_eq!(
            get_cached_firefox_cookies()
                .get("__Secure-1PSID")
                .map(String::as_str),
            Some("psid-value"),
        );

        clear_owned_session();
        assert!(!has_owned_session());
    }

    /// Build a fixture cookies.sqlite with a few Google cookies and
    /// verify `read_firefox_google_cookies` extracts them.
    #[test]
    fn extracts_google_cookies() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("cookies.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        // Minimal moz_cookies schema. Real Firefox has more columns; only
        // host, name, value matter to us.
        conn.execute(
            "CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, host TEXT, name TEXT, value TEXT)",
            [],
        )
        .unwrap();
        let rows = [
            (".google.com", "SAPISID", "test-sapisid"),
            (".google.com", "__Secure-1PSID", "test-1psid"),
            (".google.com", "NID", "test-nid"),
            // Should be excluded: not a Google cookie.
            (".example.com", "SAPISID", "should-not-appear"),
            // Should be excluded: not in our allowlist.
            (".google.com", "RandomTracker", "junk"),
        ];
        for (host, name, value) in rows {
            conn.execute(
                "INSERT INTO moz_cookies (host, name, value) VALUES (?1, ?2, ?3)",
                rusqlite::params![host, name, value],
            )
            .unwrap();
        }
        drop(conn);

        let got = read_firefox_google_cookies(tmp.path()).unwrap();
        assert_eq!(got.get("SAPISID").map(|s| s.as_str()), Some("test-sapisid"));
        assert_eq!(
            got.get("__Secure-1PSID").map(|s| s.as_str()),
            Some("test-1psid")
        );
        assert_eq!(got.get("NID").map(|s| s.as_str()), Some("test-nid"));
        assert!(!got.contains_key("RandomTracker"));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn missing_cookie_db_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_firefox_google_cookies(tmp.path()).unwrap_err();
        match err {
            Error::Pairing(_) => {}
            other => panic!("expected Pairing error, got {other:?}"),
        }
    }
}
