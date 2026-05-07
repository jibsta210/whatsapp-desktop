//! Events emitted by [`Client`](crate::Client) to consumers.

use std::time::SystemTime;

use crate::gmproto::conversations::Message;

#[derive(Debug, Clone)]
pub enum Event {
    /// Pairing produced a QR string ready for the phone to scan.
    QrCode { url: String },

    /// UKEY2 emoji verification: user must confirm the same emojis appear on
    /// the phone before the pairing completes.
    PairingEmojis { emojis: Vec<String> },

    /// Pairing finished successfully.
    PairSuccess,

    /// Pairing was rejected or revoked.
    PairFailed { reason: String },

    /// Long-poll connection live; ready to receive events.
    Ready,

    /// Phone has not responded to recent ditto pings.
    PhoneNotResponding,

    /// Phone is responding again.
    PhoneRespondingAgain,

    /// One or more new/updated messages arrived.
    Messages {
        timestamp: SystemTime,
        messages: Vec<Message>,
    },

    /// A conversation was added or updated.
    ConversationUpdate { conversation_id: String },

    /// Typing indicator changed.
    Typing {
        conversation_id: String,
        participant_id: String,
        typing: bool,
    },

    /// Server requested re-authentication. Caller should restart pairing.
    AuthRevoked,
}
