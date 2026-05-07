//! RPC session: build, encrypt, send, route response.
//!
//! Mirrors `pkg/libgm/session_handler.go`. Each outgoing RPC gets a UUID
//! request ID and a oneshot channel; the long-poll receive loop dispatches
//! incoming messages by request ID.

use std::time::Duration;

use prost::Message as _;
use prost_reflect::ReflectMessage;
use tokio::sync::oneshot;
use tokio::time::timeout;
use uuid::Uuid;

use crate::gmproto::authentication::{AuthMessage, ConfigVersion};
use crate::gmproto::client::{
    AckMessageRequest, GetOrCreateConversationRequest, GetOrCreateConversationResponse,
    MessagePayload, MessageReadRequest, NotifyDittoActivityRequest, SendMessageRequest,
    SendMessageResponse, ack_message_request,
};
use crate::gmproto::conversations::{ContactNumber, MessageContent, MessageInfo, message_info};
use crate::gmproto::rpc::{
    ActionType, BugleRoute, MessageType, OutgoingRpcData, OutgoingRpcMessage, OutgoingRpcResponse,
    outgoing_rpc_message,
};
use crate::gmproto::util::EmptyArr;
use crate::http::ContentType;
use crate::{Client, Error, Result, urls};

/// Standard 5-second response timeout (matches Go reference).
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

/// Refresh the tachyon token if it expires in less than this. Mirrors
/// `RefreshTachyonBuffer` in the Go reference (1 hour).
/// Refresh tokens with this much margin before expiry. Wider than the Go
/// reference's 1h to handle laptop suspends well — we want the proactive
/// refresh to fire well before the phone has any reason to revoke us.
pub const REFRESH_TACHYON_BUFFER: Duration = Duration::from_secs(4 * 60 * 60);

/// POST `RegisterRefresh` to renew the tachyon auth token. No-op if the token
/// still has > [`REFRESH_TACHYON_BUFFER`] left, or if we have no `browser`
/// device descriptor (= not yet paired).
///
/// Mirrors `Client.refreshAuthToken()` in `pkg/libgm/client.go`.
pub async fn refresh_auth_token(client: &Client) -> Result<()> {
    let (browser, refresh_key, tachyon_token, expiry_ms) = {
        let auth = client.inner.auth.lock().await;
        (
            auth.browser.clone(),
            auth.refresh_key.clone(),
            auth.tachyon_auth_token.clone().unwrap_or_default(),
            auth.tachyon_expiry,
        )
    };

    let Some(browser) = browser else {
        return Ok(()); // not paired yet
    };
    let Some(refresh_key) = refresh_key else {
        return Err(Error::Pairing("no refresh_key in AuthData".into()));
    };

    // Skip if the existing token still has > 1h left.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let remaining_ms = expiry_ms - now_ms;
    if remaining_ms > REFRESH_TACHYON_BUFFER.as_millis() as i64 {
        log::trace!(
            "refresh_auth_token: skipping, token has {} minutes left",
            remaining_ms / 60_000
        );
        return Ok(());
    }
    log::info!(
        "refresh_auth_token: token expires in {} minutes, renewing",
        remaining_ms.max(0) / 60_000
    );

    // Sign sha256("{request_id}:{timestamp_us}").
    let request_id = Uuid::new_v4().to_string();
    let timestamp_us = chrono::Utc::now().timestamp_millis() * 1000;
    let to_sign = format!("{request_id}:{timestamp_us}");
    let signature = crate::crypto::ecdsa::sign_asn1_sha256(&refresh_key, to_sign.as_bytes())?;

    let payload = crate::gmproto::authentication::RegisterRefreshRequest {
        message_auth: Some(crate::gmproto::authentication::AuthMessage {
            request_id,
            tachyon_auth_token: tachyon_token,
            network: client.inner.auth.lock().await.auth_network().into(),
            config_version: Some(config_version()),
        }),
        curr_browser_device: Some(browser),
        unix_timestamp: timestamp_us,
        signature,
        parameters: None,
        message_type: 2, // matches Go's hardcoded value
    };

    let resp: crate::gmproto::authentication::RegisterRefreshResponse = client
        .post_protobuf(crate::urls::REGISTER_REFRESH, &payload, crate::http::ContentType::PBLite)
        .await?;
    let token_data = resp
        .token_data
        .ok_or_else(|| Error::Protocol("RegisterRefresh: missing token_data".into()))?;
    if token_data.tachyon_auth_token.is_empty() {
        return Err(Error::Protocol(
            "RegisterRefresh: empty tachyon token in response".into(),
        ));
    }

    // Update auth state.
    {
        let mut auth = client.inner.auth.lock().await;
        let ttl_us = if token_data.ttl > 0 {
            token_data.ttl
        } else {
            // Default 24h in microseconds.
            24 * 60 * 60 * 1_000_000
        };
        auth.tachyon_auth_token = Some(token_data.tachyon_auth_token);
        auth.tachyon_ttl = ttl_us;
        auth.tachyon_expiry = chrono::Utc::now().timestamp_millis() + ttl_us / 1000;
    }
    log::info!("refresh_auth_token: renewed; new TTL = {} hours", token_data.ttl / 3_600_000_000);
    client.notify_auth_changed().await;
    Ok(())
}

/// What the long-poll receive loop hands back to a waiter. The decrypted
/// payload is the inner protobuf bytes; the caller decodes them into the
/// concrete response proto.
#[derive(Debug, Clone)]
pub struct PendingResponse {
    pub action: i32,
    pub decrypted: Vec<u8>,
    pub session_id: String,
}

/// Hardcoded config version. Mirrors `util/config.go` from the Go reference.
pub fn config_version() -> ConfigVersion {
    ConfigVersion {
        year: 2026,
        month: 3,
        day: 18,
        v1: 4,
        v2: 6,
    }
}

/// Build, encrypt, send, and (optionally) wait for the response of an RPC.
///
/// `expect_response = false` means the RPC is fire-and-forget; the function
/// returns as soon as the HTTP POST succeeds.
pub async fn send_rpc<Req: prost::Message + ReflectMessage>(
    client: &Client,
    action: ActionType,
    req: Option<&Req>,
    expect_response: bool,
) -> Result<Option<PendingResponse>> {
    send_rpc_with_id(client, action, req, expect_response, None, false).await
}

/// Like [`send_rpc`] but lets the caller specify the `request_id` and choose
/// whether to omit the TTL. Used by [`set_active_session`], which must use
/// the session UUID as the request_id and omit TTL.
pub async fn send_rpc_with_id<Req: prost::Message + ReflectMessage>(
    client: &Client,
    action: ActionType,
    req: Option<&Req>,
    expect_response: bool,
    request_id_override: Option<String>,
    omit_ttl: bool,
) -> Result<Option<PendingResponse>> {
    // Ensure the tachyon token is fresh before any RPC. The long-poll loop
    // refreshes once per iteration, but RPCs that fire between iterations
    // (set_active_session, send_text, etc.) need their own refresh.
    if let Err(e) = refresh_auth_token(client).await {
        log::warn!("send_rpc: token refresh failed (continuing): {e}");
    }

    let request_id = request_id_override.unwrap_or_else(|| Uuid::new_v4().to_string());

    // Snapshot session + auth state under the lock.
    let (session_id, tachyon_token, tachyon_ttl, mobile, dest_reg_id) = {
        let session = client.inner.session.lock().await;
        let auth = client.inner.auth.lock().await;
        (
            session.session_id.clone(),
            auth.tachyon_auth_token.clone().unwrap_or_default(),
            auth.tachyon_ttl,
            auth.mobile.clone(),
            auth.dest_reg_id.clone(),
        )
    };

    // Encrypt the inner request body if present.
    let mut encrypted = Vec::new();
    if let Some(r) = req {
        let auth = client.inner.auth.lock().await;
        let crypto = auth
            .request_crypto
            .as_ref()
            .ok_or_else(|| Error::Crypto("missing request_crypto".into()))?;
        let mut serialized = Vec::with_capacity(r.encoded_len());
        r.encode(&mut serialized)?;
        encrypted = crypto.encrypt(&serialized)?;
    }

    // Build the inner OutgoingRPCData (gets put in `Data.message_data`).
    let inner_data = OutgoingRpcData {
        request_id: request_id.clone(),
        action: action as i32,
        unencrypted_proto_data: Vec::new(),
        encrypted_proto_data: encrypted,
        session_id: session_id.clone(),
    };
    let mut inner_buf = Vec::with_capacity(inner_data.encoded_len());
    inner_data.encode(&mut inner_buf)?;

    // Wrap into OutgoingRPCMessage.
    let dest_regs = match dest_reg_id {
        Some(id) => vec![id],
        None => Vec::new(),
    };
    let payload = OutgoingRpcMessage {
        mobile,
        data: Some(outgoing_rpc_message::Data {
            request_id: request_id.clone(),
            bugle_route: BugleRoute::DataEvent as i32,
            message_data: inner_buf,
            message_type_data: Some(outgoing_rpc_message::data::Type {
                empty_arr: Some(EmptyArr {}),
                message_type: MessageType::BugleMessage as i32,
            }),
        }),
        auth: Some(outgoing_rpc_message::Auth {
            request_id: request_id.clone(),
            tachyon_auth_token: tachyon_token,
            config_version: Some(config_version()),
        }),
        // TTL: zero when explicitly omitted (e.g. set_active_session),
        // otherwise the token's TTL — matches Go's `buildMessage` default.
        ttl: if omit_ttl { 0 } else { tachyon_ttl },
        dest_registration_i_ds: dest_regs,
    };

    // Register the response waiter BEFORE the HTTP POST so we can't miss the
    // race where the phone replies before send returns.
    let rx = if expect_response {
        let (tx, rx) = oneshot::channel();
        let mut session = client.inner.session.lock().await;
        session.response_waiters.insert(request_id.clone(), tx);
        Some(rx)
    } else {
        None
    };

    // Pick URL.
    let url = if client.inner.auth.lock().await.has_cookies() {
        urls::SEND_MESSAGE_GOOGLE
    } else {
        urls::SEND_MESSAGE
    };

    // POST it.
    let cookies = client.inner.auth.lock().await.cookies.clone();
    let post_result = client
        .inner
        .http
        .post::<OutgoingRpcMessage, OutgoingRpcResponse>(url, &payload, ContentType::PBLite, &cookies)
        .await;

    if let Err(e) = post_result {
        // Pull the waiter back out.
        if expect_response {
            let mut session = client.inner.session.lock().await;
            session.response_waiters.remove(&request_id);
        }
        return Err(e);
    }

    if let Some(rx) = rx {
        match timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(resp)) => Ok(Some(resp)),
            Ok(Err(_canceled)) => Err(Error::Protocol(format!(
                "response waiter canceled for request {request_id}"
            ))),
            Err(_) => {
                // Clean up the waiter on timeout.
                let mut session = client.inner.session.lock().await;
                session.response_waiters.remove(&request_id);
                Err(Error::RpcTimeout("send_rpc"))
            }
        }
    } else {
        Ok(None)
    }
}

/// Send `text` to `to`. If `to` looks like an E.164 phone number (starts with
/// `+`), resolves it to a conversation ID first via `GetOrCreateConversation`.
/// Otherwise treats `to` as a literal `conversation_id` and skips the lookup.
///
/// Returns the local `tmp_id` assigned to the message (server-side message
/// ID arrives later via the long-poll).
pub async fn send_text(client: &Client, to: &str, text: &str) -> Result<String> {
    let conversation_id = if to.starts_with('+') {
        log::info!("send_text: resolving phone {to} to conversation_id");
        let conv = get_or_create_conversation(client, to).await?;
        log::info!("send_text: resolved → conversation_id={conv}");
        conv
    } else {
        to.to_string()
    };

    // Build a 12-digit numeric tmp ID matching Go's `tmp_%012d` format.
    let tmp_id = format!("tmp_{:012}", rand::random::<u64>() % 1_000_000_000_000u64);

    let payload = MessagePayload {
        tmp_id: tmp_id.clone(),
        conversation_id: conversation_id.clone(),
        tmp_id2: tmp_id.clone(),
        // Body lives ONLY in message_info; message_payload_content stays None
        // (Go reference: handlematrix.go:133 sets it to nil).
        message_payload_content: None,
        message_info: vec![MessageInfo {
            action_message_id: None,
            data: Some(message_info::Data::MessageContent(MessageContent {
                content: text.into(),
            })),
        }],
        participant_id: String::new(),
    };

    let req = SendMessageRequest {
        conversation_id: conversation_id.clone(),
        tmp_id: tmp_id.clone(),
        message_payload: Some(payload),
        sim_payload: None,
        force_rcs: false,
        reply: None,
    };

    let resp_bytes = send_rpc::<SendMessageRequest>(
        client,
        ActionType::SendMessage,
        Some(&req),
        true,
    )
    .await?
    .ok_or_else(|| Error::Protocol("send_text: no response".into()))?;
    let resp = SendMessageResponse::decode(&*resp_bytes.decrypted).map_err(Error::from)?;
    log::info!("send_text: response status={}", resp.status);
    Ok(tmp_id)
}

/// Resolve an E.164 phone number to a conversation ID, creating the
/// conversation if it doesn't exist yet. Mirrors the Go reference's
/// `ResolveIdentifier`/`startchat.go` flow.
pub async fn get_or_create_conversation(client: &Client, phone: &str) -> Result<String> {
    let req = GetOrCreateConversationRequest {
        numbers: vec![ContactNumber {
            mysterious_int: 2,
            number: phone.into(),
            number2: phone.into(),
            formatted_number: None,
        }],
        rcs_group_name: None,
        create_rcs_group: None,
    };
    let resp_bytes = send_rpc::<GetOrCreateConversationRequest>(
        client,
        ActionType::GetOrCreateConversation,
        Some(&req),
        true,
    )
    .await?
    .ok_or_else(|| Error::Protocol("get_or_create_conversation: no response".into()))?;
    let resp = GetOrCreateConversationResponse::decode(&*resp_bytes.decrypted).map_err(Error::from)?;
    let conv = resp
        .conversation
        .ok_or_else(|| Error::Protocol(format!("no conversation in response (status={})", resp.status)))?;
    if conv.conversation_id.is_empty() {
        return Err(Error::Protocol("empty conversation_id in response".into()));
    }
    Ok(conv.conversation_id)
}

/// Download a media attachment by ID + decrypt it with the per-message
/// AES-GCM key (which is delivered alongside the message in the
/// `MediaContent.decryption_key` field). Returns the decrypted bytes.
///
/// Mirrors `pkg/libgm/media.go::DownloadMedia`.
pub async fn download_media(
    client: &Client,
    media_id: &str,
    decryption_key: &[u8],
) -> Result<Vec<u8>> {
    use crate::gmproto::authentication::AuthMessage;
    use crate::gmproto::client::{AttachmentInfo, DownloadAttachmentRequest};

    if let Err(e) = refresh_auth_token(client).await {
        log::warn!("download_media: token refresh failed (continuing): {e}");
    }

    let (tachyon_token, network) = {
        let auth = client.inner.auth.lock().await;
        (
            auth.tachyon_auth_token.clone().unwrap_or_default(),
            auth.auth_network().to_string(),
        )
    };
    let metadata = DownloadAttachmentRequest {
        info: Some(AttachmentInfo {
            attachment_id: media_id.into(),
            encrypted: true,
        }),
        auth_data: Some(AuthMessage {
            request_id: Uuid::new_v4().to_string(),
            tachyon_auth_token: tachyon_token,
            network,
            config_version: Some(config_version()),
        }),
    };
    let mut metadata_bytes = Vec::with_capacity(metadata.encoded_len());
    metadata.encode(&mut metadata_bytes)?;
    use base64::Engine;
    let metadata_b64 = base64::engine::general_purpose::STANDARD.encode(&metadata_bytes);

    let mut headers = crate::headers::relay("", "*/*");
    headers.insert(
        reqwest::header::HeaderName::from_static("x-goog-download-metadata"),
        reqwest::header::HeaderValue::from_str(&metadata_b64)
            .map_err(|e| Error::Protocol(format!("download_media metadata header: {e}")))?,
    );
    log::debug!(
        "download_media: GET {} with metadata header ({} bytes)",
        crate::urls::UPLOAD_MEDIA,
        metadata_b64.len()
    );
    let resp = client
        .inner
        .http
        .short
        .get(crate::urls::UPLOAD_MEDIA)
        .headers(headers)
        .send()
        .await?;
    let status = resp.status();
    log::debug!("download_media: HTTP {status} for media_id={media_id}");
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Error::AuthRevoked);
    }
    if !status.is_success() {
        let body = resp.bytes().await.unwrap_or_default();
        let preview = String::from_utf8_lossy(&body[..body.len().min(256)]);
        log::warn!(
            "download_media: HTTP {status}, body[..{}]: {preview:?}",
            body.len().min(256)
        );
        return Err(Error::Protocol(format!(
            "download_media: HTTP {status} ({} bytes)",
            body.len()
        )));
    }
    let encrypted = resp.bytes().await?;
    log::debug!(
        "download_media: got {} encrypted bytes for media_id={media_id}, decrypting…",
        encrypted.len()
    );
    let plain = crate::crypto::aesgcm::decrypt(decryption_key, &encrypted)
        .map_err(|e| {
            log::warn!(
                "download_media: AES-GCM decrypt failed (key len={}): {e}",
                decryption_key.len()
            );
            e
        })?;
    log::info!(
        "download_media: success media_id={media_id}, {} encrypted → {} plaintext bytes",
        encrypted.len(),
        plain.len()
    );
    Ok(plain)
}

/// List all contacts known to Google Messages on the phone. Used to
/// resolve participant phone numbers into contact names.
pub async fn list_contacts(
    client: &Client,
) -> Result<crate::gmproto::client::ListContactsResponse> {
    let req = crate::gmproto::client::ListContactsRequest {
        i1: 1,
        i2: 350,
        i3: 50,
    };
    let resp = send_rpc::<crate::gmproto::client::ListContactsRequest>(
        client,
        ActionType::ListContacts,
        Some(&req),
        true,
    )
    .await?
    .ok_or_else(|| Error::Protocol("list_contacts: no response".into()))?;
    crate::gmproto::client::ListContactsResponse::decode(&*resp.decrypted).map_err(Error::from)
}

/// List the top N (most recently messaged) contacts. Useful when full
/// `ListContacts` returns sparse data.
pub async fn list_top_contacts(
    client: &Client,
    count: i32,
) -> Result<crate::gmproto::client::ListTopContactsResponse> {
    let req = crate::gmproto::client::ListTopContactsRequest { count };
    let resp = send_rpc::<crate::gmproto::client::ListTopContactsRequest>(
        client,
        ActionType::ListTopContacts,
        Some(&req),
        true,
    )
    .await?
    .ok_or_else(|| Error::Protocol("list_top_contacts: no response".into()))?;
    crate::gmproto::client::ListTopContactsResponse::decode(&*resp.decrypted).map_err(Error::from)
}

/// List the N most recent conversations.
pub async fn list_conversations(
    client: &Client,
    count: i64,
) -> Result<crate::gmproto::client::ListConversationsResponse> {
    let req = crate::gmproto::client::ListConversationsRequest {
        count,
        folder: 1, // Folder_INBOX
        ..Default::default()
    };
    let resp = send_rpc::<crate::gmproto::client::ListConversationsRequest>(
        client,
        ActionType::ListConversations,
        Some(&req),
        true,
    )
    .await?
    .ok_or_else(|| Error::Protocol("list_conversations: no response".into()))?;
    crate::gmproto::client::ListConversationsResponse::decode(&*resp.decrypted).map_err(Error::from)
}

/// Mark a message as read. Fire-and-forget.
pub async fn mark_read(client: &Client, conversation_id: &str, message_id: &str) -> Result<()> {
    let req = MessageReadRequest {
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
    };
    send_rpc::<MessageReadRequest>(client, ActionType::MessageRead, Some(&req), false).await?;
    Ok(())
}

/// Set the typing indicator on a conversation. Fire-and-forget.
pub async fn set_typing(client: &Client, conversation_id: &str, typing: bool) -> Result<()> {
    use crate::gmproto::client::{TypingUpdateRequest, typing_update_request};
    let req = TypingUpdateRequest {
        data: Some(typing_update_request::Data {
            conversation_id: conversation_id.into(),
            typing,
        }),
        sim_payload: None,
    };
    send_rpc::<TypingUpdateRequest>(client, ActionType::TypingUpdates, Some(&req), false).await?;
    Ok(())
}

/// Add or remove an emoji reaction on a message.
///
/// `action` is one of `Add` (1), `Remove` (2), `Switch` (3) — see
/// [`crate::gmproto::client::send_reaction_request::Action`].
pub async fn send_reaction(
    client: &Client,
    conversation_id: &str,
    message_id: &str,
    emoji: &str,
    action: i32,
) -> Result<()> {
    use crate::gmproto::client::SendReactionRequest;
    use crate::gmproto::conversations::ReactionData;
    let req = SendReactionRequest {
        message_id: message_id.into(),
        reaction_data: Some(ReactionData {
            unicode: emoji.into(),
            r#type: 0, // EmojiType_REACTION_EMOJI_TYPE_UNSPECIFIED — server resolves
            custom_emoji: None,
        }),
        action,
        sim_payload: None,
    };
    let _ = conversation_id; // not on the proto; included in API for future use
    send_rpc::<SendReactionRequest>(client, ActionType::SendReaction, Some(&req), true).await?;
    Ok(())
}

/// Fetch the latest `count` messages of a conversation. Pass `cursor` from a
/// previous response to page backwards.
pub async fn fetch_messages(
    client: &Client,
    conversation_id: &str,
    count: i64,
    cursor: Option<crate::gmproto::client::Cursor>,
) -> Result<crate::gmproto::client::ListMessagesResponse> {
    use crate::gmproto::client::{ListMessagesRequest, ListMessagesResponse};
    let req = ListMessagesRequest {
        conversation_id: conversation_id.into(),
        count,
        cursor,
    };
    let resp = send_rpc::<ListMessagesRequest>(client, ActionType::ListMessages, Some(&req), true)
        .await?
        .ok_or_else(|| Error::Protocol("fetch_messages: no response".into()))?;
    ListMessagesResponse::decode(&*resp.decrypted).map_err(Error::from)
}

/// Ping the phone. Waits for the response.
pub async fn notify_ditto_activity(client: &Client) -> Result<()> {
    let req = NotifyDittoActivityRequest { success: true };
    send_rpc::<NotifyDittoActivityRequest>(
        client,
        ActionType::NotifyDittoActivity,
        Some(&req),
        true,
    )
    .await?;
    Ok(())
}

/// Tell the server we're the active desktop. Resets our session UUID and
/// sends a `GET_UPDATES` with the new session UUID **as the request_id** —
/// this is how the server identifies us as the active receiver. The Go
/// reference calls this in `postConnect`; without it the phone won't push
/// us live updates.
pub async fn set_active_session(client: &Client) -> Result<()> {
    let new_session_id = Uuid::new_v4().to_string();
    {
        let mut session = client.inner.session.lock().await;
        session.session_id = new_session_id.clone();
    }
    log::info!("set_active_session: new session_id={new_session_id}");
    // Send GET_UPDATES with request_id = session_id, omit_ttl=true (matches
    // Go's `SetActiveSession`: `RequestID: sessionID, OmitTTL: true`).
    send_rpc_with_id::<crate::gmproto::util::EmptyArr>(
        client,
        ActionType::GetUpdates,
        None,
        false,
        Some(new_session_id),
        true,
    )
    .await?;
    Ok(())
}

/// Send the queued message acks (clears the queue).
pub async fn flush_acks(client: &Client) -> Result<()> {
    let (acks, tachyon_token, browser, network) = {
        let mut session = client.inner.session.lock().await;
        let auth = client.inner.auth.lock().await;
        if session.ack_queue.is_empty() {
            return Ok(());
        }
        let acks = std::mem::take(&mut session.ack_queue);
        (
            acks,
            auth.tachyon_auth_token.clone().unwrap_or_default(),
            auth.browser.clone(),
            auth.auth_network().to_string(),
        )
    };
    let payload = AckMessageRequest {
        auth_data: Some(AuthMessage {
            request_id: Uuid::new_v4().to_string(),
            network,
            tachyon_auth_token: tachyon_token,
            config_version: Some(config_version()),
        }),
        empty_arr: Some(EmptyArr {}),
        acks: acks
            .into_iter()
            .map(|id| ack_message_request::Message {
                request_id: id,
                device: browser.clone(),
            })
            .collect(),
    };
    let url = if client.inner.auth.lock().await.has_cookies() {
        urls::ACK_MESSAGES_GOOGLE
    } else {
        urls::ACK_MESSAGES
    };
    let cookies = client.inner.auth.lock().await.cookies.clone();
    let _: OutgoingRpcResponse = client
        .inner
        .http
        .post(url, &payload, ContentType::PBLite, &cookies)
        .await?;
    Ok(())
}
