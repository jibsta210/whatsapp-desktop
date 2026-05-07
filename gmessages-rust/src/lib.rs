//! Rust client for the Google Messages Web relay protocol.
//!
//! Port of [mautrix-gmessages](https://github.com/mautrix/gmessages)'s
//! `pkg/libgm` to Rust. Speaks the same HTTPS long-poll + protobuf relay
//! protocol used by `messages.google.com/web` to bridge SMS/MMS/RCS through a
//! paired Android phone running Google Messages.
//!
//! # Layout
//!
//! - [`gmproto`] — generated protobuf types (`prost`)
//! - [`pblite`] — PBLite encoder/decoder (JSON-array-of-arrays form)
//! - [`crypto`] — AES-CTR + HMAC, AES-GCM, HKDF, ECDSA helpers
//! - [`http`] — relay HTTP transport (binary protobuf + PBLite branching)
//! - [`pairing`] — UKEY2 + Gaia pairing state machines
//! - [`session`] — RPC request/response routing, encryption envelope
//! - [`longpoll`] — receive loop, ditto pinger, reconnection backoff
//! - [`events`] — event types emitted to consumers
//! - [`client`] — top-level [`Client`] facade and [`AuthData`]
//! - [`urls`], [`headers`] — endpoint constants and request headers
//!
//! # Status
//!
//! Crypto, HTTP transport, PBLite, and proto types are wired up.
//! Pairing and long-poll are partial — see individual modules.

#![allow(clippy::large_enum_variant)]
#![allow(dead_code)]

pub mod gmproto;

/// Raw bytes of the protobuf `FileDescriptorSet` for all `.proto` files in
/// this crate. Wired to the `ReflectMessage` derive emitted by
/// `prost-reflect-build`.
pub const PBLITE_FILE_DESCRIPTOR_SET_BYTES: &[u8] =
    include_bytes!("gmproto/file_descriptor_set.bin");

pub mod client;
pub mod crypto;
pub mod errors;
pub mod event_handler;
pub mod events;
pub mod headers;
pub mod http;
pub mod longpoll;
pub mod pairing;
pub mod pblite;
pub mod session;
pub mod urls;

pub use client::{AuthData, Client};
pub use errors::{Error, Result};
pub use events::Event;
