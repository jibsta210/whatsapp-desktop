//! Event handler: decrypt incoming RPC messages and dispatch them.
//!
//! Mirrors `pkg/libgm/event_handler.go`.
//!
//! For each incoming `IncomingRPCMessage`:
//! - PairEvent — decode `RPCPairData`; if `Paired`, complete the pairing
//! - DataEvent — decode `RPCMessageData`; decrypt `encrypted_data`; either
//!   route to a pending response waiter (matched by `session_id` field which
//!   is actually the request_id) or fan out as an `Event` for the caller
//! - GaiaEvent — Gaia (Google account) flow events; not yet wired up

use std::time::SystemTime;

use prost::Message as _;
use sha2::{Digest, Sha256};

use crate::gmproto::events::{RpcPairData, UpdateEvents, rpc_pair_data, update_events};
use crate::gmproto::rpc::{ActionType, BugleRoute, IncomingRpcMessage, RpcMessageData};
use crate::session::PendingResponse;
use crate::{Client, Error, Event, Result};

pub async fn handle_incoming_rpc(client: &Client, raw: IncomingRpcMessage) -> Result<()> {
    let route = BugleRoute::try_from(raw.bugle_route)
        .map_err(|_| Error::Protocol(format!("unknown bugle route: {}", raw.bugle_route)))?;

    log::debug!(
        "incoming RPC: route={:?} response_id={} message_data={} bytes",
        route,
        raw.response_id,
        raw.message_data.len()
    );

    match route {
        BugleRoute::PairEvent => handle_pair_event(client, &raw).await,
        BugleRoute::DataEvent => handle_data_event(client, &raw).await,
        BugleRoute::GaiaEvent => {
            log::debug!("gaia event ignored (gaia path not yet implemented)");
            Ok(())
        }
        BugleRoute::Unknown => Ok(()),
    }
}

async fn handle_pair_event(client: &Client, raw: &IncomingRpcMessage) -> Result<()> {
    log::info!("PairEvent received from server, decoding RPCPairData");
    let pair_data = RpcPairData::decode(&*raw.message_data).map_err(|e| {
        log::error!(
            "failed to decode RPCPairData from {} bytes: {e}",
            raw.message_data.len()
        );
        e
    })?;
    match pair_data.event {
        Some(rpc_pair_data::Event::Paired(paired)) => {
            log::info!(
                "PairEvent.Paired received; phone source_id={:?}",
                paired.mobile.as_ref().map(|m| &m.source_id)
            );
            // Persist mobile/browser/token.
            {
                let mut auth = client.inner.auth.lock().await;
                auth.mobile = paired.mobile.clone();
                auth.browser = paired.browser.clone();
                if let Some(token_data) = &paired.token_data {
                    auth.tachyon_auth_token = Some(token_data.tachyon_auth_token.clone());
                    auth.tachyon_ttl = token_data.ttl;
                    auth.tachyon_expiry = chrono::Utc::now().timestamp_millis()
                        + (token_data.ttl / 1000); // ttl is microseconds
                }
            }
            // Persist the now-complete auth.
            client.notify_auth_changed().await;

            // Notify the pairing waiter, if any.
            let waiter = client.inner.session.lock().await.pair_completion.take();
            if let Some(tx) = waiter {
                log::info!("PairEvent.Paired: handing PairedData to oneshot");
                if tx.send(paired).is_err() {
                    log::warn!("pair oneshot receiver dropped before we could send");
                }
            } else {
                log::warn!(
                    "PairEvent.Paired received but no oneshot waiter registered \
                     — falling back to Event::PairSuccess emit"
                );
                client.emit(Event::PairSuccess);
            }
        }
        Some(rpc_pair_data::Event::Revoked(revoked)) => {
            log::warn!("pair revoked: {revoked:?}");
            client.emit(Event::PairFailed {
                reason: format!("revoked: {revoked:?}"),
            });
        }
        None => {
            log::debug!("pair event with no inner event");
        }
    }
    Ok(())
}

async fn handle_data_event(client: &Client, raw: &IncomingRpcMessage) -> Result<()> {
    let msg_data = RpcMessageData::decode(&*raw.message_data)?;
    let request_id = msg_data.session_id.clone();

    // Decrypt the body if it is encrypted.
    let mut decrypted: Vec<u8> = Vec::new();
    if !msg_data.encrypted_data.is_empty() {
        let auth = client.inner.auth.lock().await;
        let crypto = auth
            .request_crypto
            .as_ref()
            .ok_or_else(|| Error::Crypto("missing request_crypto".into()))?;
        decrypted = crypto.decrypt(&msg_data.encrypted_data)?;
    }

    // Try to route to a waiting RPC.
    let waiter = {
        let mut session = client.inner.session.lock().await;
        session.response_waiters.remove(&request_id)
    };
    if let Some(tx) = waiter {
        let _ = tx.send(PendingResponse {
            action: msg_data.action,
            decrypted: decrypted.clone(),
            session_id: request_id.clone(),
        });
        return Ok(());
    }

    // Otherwise fan out as an event based on the action type.
    if !decrypted.is_empty()
        && (msg_data.action == ActionType::GetUpdates as i32
            || msg_data.action == ActionType::ConversationUpdates as i32
            || msg_data.action == ActionType::MessageUpdates as i32
            || msg_data.action == ActionType::TypingUpdates as i32)
    {
        let updates = UpdateEvents::decode(&*decrypted)?;
        handle_update_events(client, raw, &decrypted, updates).await;
    }
    Ok(())
}

async fn handle_update_events(
    client: &Client,
    raw: &IncomingRpcMessage,
    decrypted: &[u8],
    updates: UpdateEvents,
) {
    // Dedup using the same 8-slot ring buffer as the Go reference.
    let mut hasher = Sha256::new();
    hasher.update(decrypted);
    let hash: [u8; 32] = hasher.finalize().into();
    let id = raw.response_id.clone();
    if dedup(client, &id, hash).await {
        log::trace!("dedupped event {id}");
        return;
    }

    let timestamp = SystemTime::now();
    match updates.event {
        Some(update_events::Event::MessageEvent(evt)) => {
            client.emit(Event::Messages {
                timestamp,
                messages: evt.data,
            });
        }
        Some(update_events::Event::ConversationEvent(evt)) => {
            for conv in evt.data {
                client.emit(Event::ConversationUpdate {
                    conversation_id: conv.conversation_id,
                });
            }
        }
        Some(update_events::Event::TypingEvent(evt)) => {
            if let Some(data) = evt.data {
                let participant = data
                    .user
                    .as_ref()
                    .map(|u| u.number.clone())
                    .unwrap_or_default();
                client.emit(Event::Typing {
                    conversation_id: data.conversation_id,
                    participant_id: participant,
                    typing: data.r#type == 1, // TypingTypes_STARTED = 1
                });
            }
        }
        Some(update_events::Event::UserAlertEvent(_alert)) => {
            // No-op for now; alerts include things like "battery low", "RCS connected".
        }
        Some(update_events::Event::SettingsEvent(_)) => {}
        Some(update_events::Event::BrowserPresenceCheckEvent(_)) => {
            // The phone is checking we're alive; ack via session handler.
            // Fire-and-forget AckBrowserPresence.
            let client_clone = client.clone();
            tokio::spawn(async move {
                let _ = crate::session::send_rpc::<crate::gmproto::util::EmptyArr>(
                    &client_clone,
                    crate::gmproto::rpc::ActionType::AckBrowserPresence,
                    None,
                    false,
                )
                .await;
            });
        }
        Some(update_events::Event::AccountChange(_)) => {}
        None => {}
    }
}

async fn dedup(client: &Client, id: &str, hash: [u8; 32]) -> bool {
    let mut session = client.inner.session.lock().await;
    let len = session.recent_updates.len();
    // Walk the ring backwards looking for matching id.
    for i in 1..=len {
        let idx = (session.recent_updates_ptr + len - i) % len;
        let (eid, ehash) = &session.recent_updates[idx];
        if eid == id {
            return ehash == &hash;
        }
    }
    let ptr = session.recent_updates_ptr;
    session.recent_updates[ptr] = (id.to_string(), hash);
    session.recent_updates_ptr = (ptr + 1) % len;
    false
}
