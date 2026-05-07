//! Generated protobuf types from `proto/*.proto`.
//!
//! Regenerated only when `GENERATE_PROTO=1` is set. The generated `.rs` files
//! live in `src/gmproto/` and are checked in so a normal build does not
//! require `protoc`.

#![allow(clippy::module_inception)]

#[rustfmt::skip]
pub mod authentication { include!("gmproto/authentication.rs"); }
#[rustfmt::skip]
pub mod client         { include!("gmproto/client.rs"); }
#[rustfmt::skip]
pub mod config         { include!("gmproto/config.rs"); }
#[rustfmt::skip]
pub mod conversations  { include!("gmproto/conversations.rs"); }
#[rustfmt::skip]
pub mod events         { include!("gmproto/events.rs"); }
#[rustfmt::skip]
pub mod rpc            { include!("gmproto/rpc.rs"); }
#[rustfmt::skip]
pub mod settings       { include!("gmproto/settings.rs"); }
#[rustfmt::skip]
pub mod ukey           { include!("gmproto/ukey.rs"); }
#[rustfmt::skip]
pub mod util           { include!("gmproto/util.rs"); }
