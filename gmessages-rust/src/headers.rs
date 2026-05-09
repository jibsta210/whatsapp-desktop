//! HTTP header constants and builders. Mirrors `pkg/libgm/util/func.go`.

pub const GOOGLE_API_KEY: &str = "AIzaSyCA4RsOZUFrm9whhtGosPlJLmVPnfSHKz8";
pub const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";
pub const SEC_UA: &str = r#""Google Chrome";v="146", "Chromium";v="146", "Not-A.Brand";v="24""#;
pub const UA_PLATFORM: &str = "Android";
pub const X_USER_AGENT: &str = "grpc-web-javascript/0.1";
pub const SEC_UA_MOBILE: &str = "?1";
pub const ORIGIN: &str = "https://messages.google.com";
pub const REFERER: &str = "https://messages.google.com/";

use reqwest::header::{HeaderMap, HeaderValue};

/// Compute the `SAPISIDHASH` Authorization header value used by Google's
/// `clients6.google.com` cookie-bearing endpoints (e.g. SignInGaia).
///
/// Algorithm (from the Chromium source `google_apis/gaia/oauth2_access_token_fetcher_impl.cc`
/// and many other public references):
///
/// 1. Take a UNIX timestamp in seconds (current time).
/// 2. Concatenate `"{timestamp} {sapisid} {origin}"`.
/// 3. SHA1 hex digest.
/// 4. Header value = `"SAPISIDHASH {timestamp}_{hexdigest}"`.
///
/// `origin` is typically `"https://messages.google.com"` for our use.
///
/// We also build per-prefix variants (`SAPISID1PHASH`, `SAPISID3PHASH`)
/// from the corresponding `__Secure-1PAPISID` / `__Secure-3PAPISID`
/// cookies when they're present, separated by spaces — this is how
/// chrome.google.com builds the header.
pub fn sapisid_authorization(
    cookies: &std::collections::HashMap<String, String>,
    origin: &str,
) -> Option<String> {
    use sha1::{Digest, Sha1};
    let timestamp = chrono::Utc::now().timestamp();
    let mut parts = Vec::new();

    let mut push = |prefix: &str, sapisid: &str| {
        let mut h = Sha1::new();
        h.update(format!("{timestamp} {sapisid} {origin}").as_bytes());
        let digest = h.finalize();
        let hex = digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        parts.push(format!("{prefix} {timestamp}_{hex}"));
    };

    if let Some(sapisid) = cookies.get("SAPISID") {
        push("SAPISIDHASH", sapisid);
    }
    if let Some(sapisid_1p) = cookies.get("__Secure-1PAPISID") {
        push("SAPISID1PHASH", sapisid_1p);
    }
    if let Some(sapisid_3p) = cookies.get("__Secure-3PAPISID") {
        push("SAPISID3PHASH", sapisid_3p);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Add the cookie-auth-specific headers to a HeaderMap when cookies
/// are present and the URL targets a `clients6.google.com` endpoint.
///
/// Specifically:
/// - `Cookie: ...`
/// - `Authorization: SAPISIDHASH ...` (from [`sapisid_authorization`])
/// - `X-Goog-AuthUser: N` (from `GMESSAGES_AUTHUSER` env var, default 0)
///
/// Centralized here so both [`crate::http::RelayHttp::post`] and the
/// long-poll (which bypasses post_inner for the streaming response)
/// produce identical headers.
pub fn apply_cookie_auth(
    headers: &mut HeaderMap,
    url: &str,
    cookies: &std::collections::HashMap<String, String>,
) {
    if cookies.is_empty() {
        return;
    }
    let cookie_header = cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    if let Ok(v) = HeaderValue::from_str(&cookie_header) {
        headers.insert(reqwest::header::HeaderName::from_static("cookie"), v);
    }
    if url.contains("clients6.google.com")
        && let Some(auth) = sapisid_authorization(cookies, ORIGIN)
        && let Ok(v) = HeaderValue::from_str(&auth)
    {
        headers.insert(
            reqwest::header::HeaderName::from_static("authorization"),
            v,
        );
    }
    if let Ok(authuser) = std::env::var("GMESSAGES_AUTHUSER")
        && let Ok(v) = HeaderValue::from_str(&authuser)
    {
        headers.insert(
            reqwest::header::HeaderName::from_static("x-goog-authuser"),
            v,
        );
    }
}

pub fn relay(content_type: &str, accept: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("sec-ch-ua", HeaderValue::from_static(SEC_UA));
    h.insert("x-user-agent", HeaderValue::from_static(X_USER_AGENT));
    h.insert("x-goog-api-key", HeaderValue::from_static(GOOGLE_API_KEY));
    if !content_type.is_empty()
        && let Ok(v) = HeaderValue::from_str(content_type)
    {
        h.insert("content-type", v);
    }
    h.insert("sec-ch-ua-mobile", HeaderValue::from_static(SEC_UA_MOBILE));
    h.insert("user-agent", HeaderValue::from_static(USER_AGENT));
    h.insert("sec-ch-ua-platform", HeaderValue::from_static("\"Android\""));
    if let Ok(v) = HeaderValue::from_str(accept) {
        h.insert("accept", v);
    }
    h.insert("origin", HeaderValue::from_static(ORIGIN));
    h.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
    h.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
    h.insert("sec-fetch-dest", HeaderValue::from_static("empty"));
    h.insert("referer", HeaderValue::from_static(REFERER));
    h.insert("accept-language", HeaderValue::from_static("en-US,en;q=0.9"));
    h
}
