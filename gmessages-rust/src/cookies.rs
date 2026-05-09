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
/// Firefox holds an exclusive lock on the cookie DB while running, so we
/// copy it to a tempfile first. The cost is one ~1 MB file copy per call.
pub fn read_firefox_google_cookies(profile_dir: &Path) -> Result<HashMap<String, String>> {
    let src = profile_dir.join("cookies.sqlite");
    if !src.exists() {
        return Err(Error::Pairing(format!(
            "no cookies.sqlite in firefox profile: {}",
            profile_dir.display()
        )));
    }

    let tmp = tempfile::Builder::new()
        .prefix("ffcookies-")
        .suffix(".sqlite")
        .tempfile()
        .map_err(|e| Error::Pairing(format!("create tempfile: {e}")))?;
    std::fs::copy(&src, tmp.path())
        .map_err(|e| Error::Pairing(format!("copy cookies.sqlite: {e}")))?;

    let conn = Connection::open_with_flags(
        tmp.path(),
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
            "Firefox has Google cookies but SAPISID is missing. Sign in fully and retry."
                .into(),
        ));
    }
    log::info!("gaia: found {} Google cookies in Firefox", cookies.len());
    Ok(cookies)
}

#[cfg(test)]
mod tests {
    use super::*;

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
