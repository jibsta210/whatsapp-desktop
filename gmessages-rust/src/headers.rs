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
