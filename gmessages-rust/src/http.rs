//! Relay HTTP transport: branches on content-type between binary protobuf
//! (`application/x-protobuf`) and PBLite (`application/json+protobuf`).
//!
//! Mirrors `pkg/libgm/http.go`.

use std::collections::HashMap;
use std::time::Duration;

use prost::Message;
use prost_reflect::ReflectMessage;

use crate::headers;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub enum ContentType {
    Protobuf,
    PBLite,
}

impl ContentType {
    pub fn as_str(self) -> &'static str {
        match self {
            ContentType::Protobuf => "application/x-protobuf",
            ContentType::PBLite => "application/json+protobuf",
        }
    }
}

/// Pre-configured HTTP client. Two underlying [`reqwest::Client`]s are kept:
/// one with a short timeout for normal RPCs, one with a long timeout for
/// long-polling.
#[derive(Clone)]
pub struct RelayHttp {
    pub(crate) short: reqwest::Client,
    pub(crate) long: reqwest::Client,
}

impl Default for RelayHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayHttp {
    pub fn new() -> Self {
        let short = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("short reqwest client should build");
        let long = reqwest::Client::builder()
            .timeout(Duration::from_secs(60 * 35))
            .build()
            .expect("long reqwest client should build");
        Self { short, long }
    }

    /// POST a typed protobuf message and decode the response. Picks
    /// short/long client based on `long_poll`.
    pub async fn post<Req, Resp>(
        &self,
        url: &str,
        req: &Req,
        ct: ContentType,
        cookies: &HashMap<String, String>,
        authuser: Option<u32>,
    ) -> Result<Resp>
    where
        Req: Message + ReflectMessage,
        Resp: Message + ReflectMessage + Default,
    {
        self.post_inner(url, req, ct, cookies, authuser, false)
            .await
    }

    pub async fn post_long<Req, Resp>(
        &self,
        url: &str,
        req: &Req,
        ct: ContentType,
        cookies: &HashMap<String, String>,
        authuser: Option<u32>,
    ) -> Result<Resp>
    where
        Req: Message + ReflectMessage,
        Resp: Message + ReflectMessage + Default,
    {
        self.post_inner(url, req, ct, cookies, authuser, true).await
    }

    async fn post_inner<Req, Resp>(
        &self,
        url: &str,
        req: &Req,
        ct: ContentType,
        cookies: &HashMap<String, String>,
        authuser: Option<u32>,
        long_poll: bool,
    ) -> Result<Resp>
    where
        Req: Message + ReflectMessage,
        Resp: Message + ReflectMessage + Default,
    {
        let body = match ct {
            ContentType::Protobuf => {
                let mut buf = Vec::with_capacity(req.encoded_len());
                req.encode(&mut buf)?;
                buf
            }
            ContentType::PBLite => crate::pblite::marshal(req)?,
        };

        let mut headers = headers::relay(ct.as_str(), "*/*");
        // Cookie + SAPISIDHASH + X-Goog-AuthUser, applied uniformly so
        // long-poll (which calls reqwest directly for the streaming
        // response) sees the same headers via the same helper.
        headers::apply_cookie_auth(&mut headers, url, cookies, authuser);

        let client = if long_poll { &self.long } else { &self.short };
        let resp = client.post(url).headers(headers).body(body).send().await?;

        let status = resp.status();
        let resp_ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .unwrap_or_default();

        // Self-rotation: capture any Set-Cookie headers Google sent back
        // and merge into the live cookie cache. This is how the desktop
        // session stays fresh without needing to spin Firefox back up —
        // Google routinely refreshes session cookies opportunistically
        // (`__Secure-1PSIDTS` etc.) on responses, and we just need to
        // honor them.
        if url.contains("clients6.google.com")
            || url.contains("instantmessaging-pa")
            || url.contains("messages.google.com")
        {
            let set_cookies: Vec<&str> = resp
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();
            if !set_cookies.is_empty() {
                let parsed = crate::cookies::parse_set_cookie_headers(set_cookies.iter().copied());
                crate::cookies::merge_into_cache(parsed);
            }
        }

        let body = resp.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(Error::RateLimited);
            }
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(Error::AuthRevoked);
            }
            // Log a snippet of the response body for diagnostics — Google's
            // 400/500 responses usually carry a helpful explanation.
            let preview = String::from_utf8_lossy(&body[..body.len().min(800)]).to_string();
            log::warn!(
                "http: HTTP {status} from {url} ({} bytes); body preview: {preview}",
                body.len()
            );
            return Err(Error::Protocol(format!(
                "HTTP {status} from {url}: {preview}"
            )));
        }

        decode_response::<Resp>(&body, &resp_ct)
    }
}

/// Decode a response based on the (already-stripped) content-type string.
fn decode_response<Resp: Message + ReflectMessage + Default>(
    body: &[u8],
    content_type: &str,
) -> Result<Resp> {
    match content_type {
        "application/x-protobuf" => Ok(Resp::decode(body)?),
        "application/json+protobuf" | "text/plain" | "" => crate::pblite::unmarshal(body),
        other => Err(Error::Protocol(format!(
            "unknown response content-type: {other}"
        ))),
    }
}
