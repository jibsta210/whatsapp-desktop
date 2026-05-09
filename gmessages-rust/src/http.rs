//! Relay HTTP transport: branches on content-type between binary protobuf
//! (`application/x-protobuf`) and PBLite (`application/json+protobuf`).
//!
//! Mirrors `pkg/libgm/http.go`.

use std::collections::HashMap;
use std::time::Duration;

use prost::Message;
use prost_reflect::ReflectMessage;
use reqwest::header::{HeaderName, HeaderValue};

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
    ) -> Result<Resp>
    where
        Req: Message + ReflectMessage,
        Resp: Message + ReflectMessage + Default,
    {
        self.post_inner(url, req, ct, cookies, false).await
    }

    pub async fn post_long<Req, Resp>(
        &self,
        url: &str,
        req: &Req,
        ct: ContentType,
        cookies: &HashMap<String, String>,
    ) -> Result<Resp>
    where
        Req: Message + ReflectMessage,
        Resp: Message + ReflectMessage + Default,
    {
        self.post_inner(url, req, ct, cookies, true).await
    }

    async fn post_inner<Req, Resp>(
        &self,
        url: &str,
        req: &Req,
        ct: ContentType,
        cookies: &HashMap<String, String>,
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
        if !cookies.is_empty() {
            let cookie_header = cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            if let Ok(v) = HeaderValue::from_str(&cookie_header) {
                headers.insert(HeaderName::from_static("cookie"), v);
            }
            // clients6.google.com endpoints (SignInGaia, RegisterRefresh,
            // and the cookie-routed Messaging endpoints) reject requests
            // with cookies but no SAPISIDHASH Authorization. Compute it
            // from the SAPISID cookie + the messages.google.com origin.
            if url.contains("clients6.google.com")
                && let Some(auth) = headers::sapisid_authorization(
                    cookies,
                    headers::ORIGIN,
                )
                && let Ok(v) = HeaderValue::from_str(&auth)
            {
                headers.insert(HeaderName::from_static("authorization"), v);
            }
        }

        let client = if long_poll { &self.long } else { &self.short };
        let resp = client
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let resp_ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
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
            return Err(Error::Protocol(format!(
                "HTTP {status} from {url}: {} bytes body",
                body.len()
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
