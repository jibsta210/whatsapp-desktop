//! Events emitted by [`Client`](crate::Client) to consumers.

use std::time::SystemTime;

use crate::gmproto::conversations::Message;

#[derive(Debug, Clone)]
pub enum Event {
    /// Pairing produced a QR string ready for the phone to scan.
    QrCode { url: String },

    /// UKEY2 emoji verification (Gaia pairing): the user must confirm the
    /// same emoji appears on both desktop and phone before pairing
    /// finalizes. Caller responds via [`Client::confirm_pairing_emoji`].
    PairingEmoji { emoji: String },

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
