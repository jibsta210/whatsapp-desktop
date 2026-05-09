//! Enumerate the Google accounts signed into the user's browser via the
//! Google `ListAccounts` endpoint. Used by Gaia pairing so the user can
//! pick which of their accounts to register the desktop against (the
//! account that actually has Google Messages set up).
//!
//! Wire format: GET `https://accounts.google.com/ListAccounts` with cookies
//! returns a tiny HTML page containing
//!
//! ```text
//! window.parent.postMessage('<hex-escaped JSON array>', 'https://accounts.google.com');
//! ```
//!
//! The JSON, once unescaped, looks like:
//!
//! ```json
//! ["gaia.l.a.r", [
//!   ["gaia.l.a", 1, "Display Name", "user@example.com", "photo_url",
//!    1, 1, 0, null, 1, "obfuscated_id", ...],
//!   ...
//! ]]
//! ```
//!
//! Each inner array is one signed-in account. Index 7 is the authuser
//! number (0, 1, 2…) that matches the `authuser=N` URL parameter the
//! Gaia API understands.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// One Google account discovered via [`list_google_accounts`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoogleAccount {
    /// Index that matches the `authuser=N` URL parameter on Google APIs.
    pub authuser: u32,
    /// Account email (e.g. `alice@gmail.com`).
    pub email: String,
    /// Display name (e.g. `Alice Smith`).
    pub display_name: String,
    /// Profile photo URL, may be empty.
    pub photo_url: String,
}

/// Query Google's `ListAccounts` endpoint and return one entry per
/// signed-in account. Cookies must include the Google session cookies
/// (`SAPISID`, `SID`, `__Secure-1PSID`, etc.) — see [`crate::cookies`].
pub async fn list_google_accounts(
    http: &crate::http::RelayHttp,
    cookies: &HashMap<String, String>,
) -> Result<Vec<GoogleAccount>> {
    if cookies.is_empty() {
        return Err(Error::Pairing(
            "no Google cookies provided — sign into messages.google.com in Firefox first".into(),
        ));
    }
    // The endpoint requires an Origin/Referer at accounts.google.com, plus
    // listPages=0 so it returns the list (otherwise it 400s).
    let url = "https://accounts.google.com/ListAccounts?listPages=0&authuser=0";
    let cookie_header = cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");

    let resp = http
        .short
        .get(url)
        .header("Cookie", &cookie_header)
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0",
        )
        .header("Accept", "*/*")
        .header("Origin", "https://accounts.google.com")
        .header("Referer", "https://accounts.google.com/")
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(Error::Pairing(format!(
            "ListAccounts HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }

    parse_list_accounts(&body)
}

/// Pull the JSON out of the `postMessage('…', '…')` wrapper, unescape it,
/// and decode account entries.
fn parse_list_accounts(html: &str) -> Result<Vec<GoogleAccount>> {
    // Find the first quoted argument to postMessage. We look for the
    // call boundary `postMessage('` and then read until the next
    // unescaped `'`. The payload uses `\x` hex escapes (and `\\'`
    // shouldn't appear inside) so an unescaped `'` is a clean stop.
    let needle = "postMessage('";
    let start = html
        .find(needle)
        .ok_or_else(|| Error::Pairing("ListAccounts: no postMessage in body".into()))?
        + needle.len();
    let rest = &html[start..];
    // Stop at the first unescaped single quote.
    let mut end = None;
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            // Skip the next character (escape).
            chars.next();
            continue;
        }
        if c == '\'' {
            end = Some(i);
            break;
        }
    }
    let end = end.ok_or_else(|| Error::Pairing("ListAccounts: unterminated postMessage".into()))?;
    let escaped = &rest[..end];

    // Decode the `\xNN` hex escapes and `\/` slash escape.
    let decoded = decode_js_string(escaped);

    let parsed: serde_json::Value = serde_json::from_str(&decoded)
        .map_err(|e| Error::Pairing(format!("ListAccounts JSON decode: {e}")))?;
    let outer = parsed
        .as_array()
        .ok_or_else(|| Error::Pairing("ListAccounts: outer not array".into()))?;
    if outer.len() < 2 {
        return Err(Error::Pairing("ListAccounts: outer too short".into()));
    }
    let entries = outer[1]
        .as_array()
        .ok_or_else(|| Error::Pairing("ListAccounts: entries not array".into()))?;

    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let arr = match entry.as_array() {
            Some(a) if a.len() >= 8 => a,
            _ => continue,
        };
        let display_name = arr
            .get(2)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let email = arr
            .get(3)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let photo_url = arr
            .get(4)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // authuser index is at position 7. Has been observed as both
        // string ("0") and number (0); accept either.
        let authuser = arr
            .get(7)
            .and_then(|v| {
                v.as_u64()
                    .map(|n| n as u32)
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
            })
            .unwrap_or(0);
        if email.is_empty() {
            continue;
        }
        out.push(GoogleAccount {
            authuser,
            email,
            display_name,
            photo_url,
        });
    }
    // Sort by authuser to make UI ordering stable.
    out.sort_by_key(|a| a.authuser);
    Ok(out)
}

/// Decode JS single-quoted string with `\x` and `\/` escapes.
fn decode_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            if n == b'x' && i + 3 < bytes.len() {
                if let Ok(byte) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or(""),
                    16,
                ) {
                    out.push(byte as char);
                    i += 4;
                    continue;
                }
            }
            // Common simple escapes.
            match n {
                b'/' => {
                    out.push('/');
                    i += 2;
                    continue;
                }
                b'\\' => {
                    out.push('\\');
                    i += 2;
                    continue;
                }
                b'\'' => {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                b'"' => {
                    out.push('"');
                    i += 2;
                    continue;
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                    continue;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // Slow path: append the next UTF-8 codepoint.
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_listaccounts_response() {
        // Fixture captured via curl from a real (multi-account) Firefox.
        let body = r##"<!DOCTYPE html><html><body><script type="text/javascript">window.parent.postMessage('\x5b\x22gaia.l.a.r\x22,\x5b\x5b\x22gaia.l.a\x22,1,\x22Display One\x22,\x22first@example.com\x22,\x22https:\/\/photo1\x22,1,1,0,null,1,\x22id1\x22\x5d,\x5b\x22gaia.l.a\x22,1,\x22Display Two\x22,\x22second@example.com\x22,\x22https:\/\/photo2\x22,0,0,1,null,1,\x22id2\x22\x5d\x5d\x5d', 'https:\/\/accounts.google.com');</script></body></html>"##;
        let got = parse_list_accounts(body).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].authuser, 0);
        assert_eq!(got[0].email, "first@example.com");
        assert_eq!(got[0].display_name, "Display One");
        assert_eq!(got[1].authuser, 1);
        assert_eq!(got[1].email, "second@example.com");
    }

    #[test]
    fn missing_postmessage_errors() {
        let err = parse_list_accounts("<html><body>nope</body></html>").unwrap_err();
        match err {
            Error::Pairing(_) => {}
            other => panic!("unexpected err: {other:?}"),
        }
    }
}
