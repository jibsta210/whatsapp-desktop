//! Endpoint URLs. Mirrors `pkg/libgm/util/paths.go`.

pub const MESSAGES_BASE: &str = "https://messages.google.com";
pub const GOOGLE_AUTHENTICATION: &str = "https://messages.google.com/web/authentication";
pub const GOOGLE_TIMESOURCE: &str = "https://messages.google.com/web/timesource";
pub const CONFIG: &str = "https://messages.google.com/web/config";
pub const QR_CODE_URL_BASE: &str = "https://support.google.com/messages/?p=web_computer#?c=";

const IM_BASE: &str = "https://instantmessaging-pa.googleapis.com";
const IM_BASE_GOOGLE: &str = "https://instantmessaging-pa.clients6.google.com";

pub const UPLOAD_MEDIA: &str = "https://instantmessaging-pa.googleapis.com/upload";

const PAIRING_BASE: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Pairing";
pub const REGISTER_PHONE_RELAY: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Pairing/RegisterPhoneRelay";
pub const REFRESH_PHONE_RELAY: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Pairing/RefreshPhoneRelay";
pub const GET_WEB_ENCRYPTION_KEY: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Pairing/GetWebEncryptionKey";
pub const REVOKE_RELAY_PAIRING: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Pairing/RevokeRelayPairing";

pub const RECEIVE_MESSAGES: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Messaging/ReceiveMessages";
pub const SEND_MESSAGE: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Messaging/SendMessage";
pub const ACK_MESSAGES: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.Messaging/AckMessages";

pub const RECEIVE_MESSAGES_GOOGLE: &str = "https://instantmessaging-pa.clients6.google.com/$rpc/google.internal.communications.instantmessaging.v1.Messaging/ReceiveMessages";
pub const SEND_MESSAGE_GOOGLE: &str = "https://instantmessaging-pa.clients6.google.com/$rpc/google.internal.communications.instantmessaging.v1.Messaging/SendMessage";
pub const ACK_MESSAGES_GOOGLE: &str = "https://instantmessaging-pa.clients6.google.com/$rpc/google.internal.communications.instantmessaging.v1.Messaging/AckMessages";

pub const SIGN_IN_GAIA: &str = "https://instantmessaging-pa.clients6.google.com/$rpc/google.internal.communications.instantmessaging.v1.Registration/SignInGaia";
pub const REGISTER_REFRESH: &str = "https://instantmessaging-pa.clients6.google.com/$rpc/google.internal.communications.instantmessaging.v1.Registration/RegisterRefresh";

// Suppress unused warnings for the constants we keep symmetrical for potential future use.
#[allow(dead_code)]
const _UNUSED: (&str, &str, &str) = (IM_BASE, IM_BASE_GOOGLE, PAIRING_BASE);
