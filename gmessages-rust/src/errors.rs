//! Error types for gmessages-rust.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    #[error("pairing failed: {0}")]
    Pairing(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("PBLite encode/decode error: {0}")]
    PBLite(String),

    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("protobuf decode error: {0}")]
    Decode(#[from] prost::DecodeError),

    #[error("protobuf encode error: {0}")]
    Encode(#[from] prost::EncodeError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("base64 error: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("phone not responding (>{0} consecutive ping failures)")]
    PhoneNotResponding(u32),

    #[error("session expired or revoked; re-pair required")]
    AuthRevoked,

    #[error("rpc timeout for action {0}")]
    RpcTimeout(&'static str),

    #[error("rate limited by server (HTTP 429)")]
    RateLimited,

    #[error("unexpected server response: {0}")]
    Protocol(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
