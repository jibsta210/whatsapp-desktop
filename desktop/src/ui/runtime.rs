//! Tokio-side runtime: connects to WhatsApp, processes events, handles commands.

use std::sync::Arc;

use anyhow::Result;
use async_channel::Sender;
use tokio::sync::mpsc::UnboundedReceiver;

use whatsapp_rust::bot::Bot;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events::{ChatPresenceUpdate, Event, Receipt};
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::types::presence::{ChatPresence, ReceiptType};
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::{ChatStateType, Client, Jid, RevokeType, TokioRuntime};
use whatsapp_rust::proto_helpers::MessageExt;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::bridge::{IncomingMessage, ReceiptStatus, WaCommand, WaEvent};

pub async fn run_wa_runtime(
    event_tx: Sender<WaEvent>,
    mut cmd_rx: UnboundedReceiver<WaCommand>,
) {
    if let Err(e) = run_inner(event_tx.clone(), &mut cmd_rx).await {
        log::error!("WhatsApp runtime error: {e:#}");
        let _ = event_tx.send(WaEvent::Disconnected(e.to_string())).await;
    }
}

async fn run_inner(
    event_tx: Sender<WaEvent>,
    cmd_rx: &mut UnboundedReceiver<WaCommand>,
) -> Result<()> {
    let backend = Arc::new(SqliteStore::new("whatsapp.db").await?);
    let transport_factory = TokioWebSocketTransportFactory::new();
    let http_client = UreqHttpClient::new();

    let tx = event_tx.clone();
    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(transport_factory)
        .with_http_client(http_client)
        .with_runtime(TokioRuntime)
        .on_event(move |event, _client| {
            let tx = tx.clone();
            async move {
                handle_wa_event(&tx, event).await;
            }
        })
        .build()
        .await?;

    let client = bot.client();
    let mut bot_handle = bot.run().await?;

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                let c = client.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_command(&c, cmd).await {
                        log::warn!("Command error: {e:#}");
                    }
                });
            }
            _ = &mut bot_handle => { break; }
        }
    }

    Ok(())
}

async fn handle_wa_event(tx: &Sender<WaEvent>, event: Event) {
    let ev = match event {
        Event::PairingQrCode { code, .. } => WaEvent::QrCode(code),

        // Connected is a unit struct — push a generic connected event.
        // The device JID will be available via client.get_device_jid() later.
        Event::Connected(_) => WaEvent::Connected {
            phone: String::new(),
            name: String::new(),
        },

        Event::Disconnected(_) | Event::LoggedOut(_) => {
            WaEvent::Disconnected("Connection closed".to_string())
        }

        Event::Message(msg, info) => {
            match map_message(*msg, info) {
                Some(m) => WaEvent::MessageReceived(m),
                None => return,
            }
        }

        Event::Receipt(r) => {
            let status = match r.r#type {
                ReceiptType::Read | ReceiptType::ReadSelf => ReceiptStatus::Read,
                _ => ReceiptStatus::Delivered,
            };
            for msg_id in r.message_ids {
                let _ = tx.send(WaEvent::ReceiptUpdate { msg_id, status: status.clone() }).await;
            }
            return;
        }

        Event::ChatPresence(p) => {
            let is_typing = matches!(p.state, ChatPresence::Composing);
            WaEvent::TypingIndicator {
                chat_id: p.source.chat.to_string(),
                is_typing,
            }
        }

        _ => return,
    };

    let _ = tx.send(ev).await;
}

async fn handle_command(client: &Arc<Client>, cmd: WaCommand) -> Result<()> {
    match cmd {
        WaCommand::SendText { chat_id, text } => {
            let jid: Jid = chat_id.parse()?;
            let msg = wa::Message {
                conversation: Some(text),
                ..Default::default()
            };
            client.send_message(jid, msg).await?;
        }

        WaCommand::SendReply { chat_id, text, quoted_msg_id, quoted_sender } => {
            let jid: Jid = chat_id.parse()?;
            let sender_jid: Jid = quoted_sender.parse()?;

            let ctx = whatsapp_rust::proto_helpers::build_quote_context_with_info(
                &quoted_msg_id,
                &sender_jid,
                &jid,
                &Default::default(),
            );

            let msg = wa::Message {
                extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                    text: Some(text),
                    context_info: Some(Box::new(ctx)),
                    ..Default::default()
                })),
                ..Default::default()
            };
            client.send_message(jid, msg).await?;
        }

        WaCommand::ForwardMessage { to_chat_id, original_msg_id } => {
            log::info!("Forward {original_msg_id} → {to_chat_id} (not yet implemented)");
        }

        WaCommand::DeleteForEveryone { chat_id, msg_id } => {
            let jid: Jid = chat_id.parse()?;
            client.revoke_message(jid, msg_id, RevokeType::Sender).await?;
        }

        WaCommand::SetTyping { chat_id, is_typing } => {
            let jid: Jid = chat_id.parse()?;
            if is_typing {
                client.chatstate().send_composing(&jid).await?;
            } else {
                client.chatstate().send_paused(&jid).await?;
            }
        }

        WaCommand::LoadChat { chat_id } => {
            log::info!("LoadChat: {chat_id} — history TBD");
        }

        WaCommand::MarkRead { chat_id } => {
            log::info!("MarkRead: {chat_id} — TBD");
        }

        WaCommand::Logout => {
            client.disconnect().await;
        }
    }
    Ok(())
}

fn map_message(msg: wa::Message, info: MessageInfo) -> Option<IncomingMessage> {
    let text = msg.text_content().map(|s| s.to_string());

    let base = msg.get_base_message();

    // context_info lives on the individual message types, not on Message itself
    let ctx = base.extended_text_message.as_deref()
        .and_then(|m| m.context_info.as_deref())
        .or_else(|| base.image_message.as_deref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| base.video_message.as_deref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| base.audio_message.as_deref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| base.document_message.as_deref().and_then(|m| m.context_info.as_deref()));

    let quoted_msg_id = ctx.and_then(|c| c.stanza_id.clone());
    let quoted_sender = ctx.and_then(|c| c.participant.clone());
    let quoted_text = ctx.and_then(|c| {
        c.quoted_message.as_deref()?.text_content().map(|s: &str| s.to_string())
    });
    let is_forwarded = ctx.and_then(|c| c.is_forwarded).unwrap_or(false);
    let forwarding_score = ctx.and_then(|c| c.forwarding_score).unwrap_or(0);

    Some(IncomingMessage {
        id: info.id.to_string(),
        chat_id: info.source.chat.to_string(),
        sender_id: info.source.sender.to_string(),
        sender_name: info.push_name.clone(),
        text,
        media_type: None,
        timestamp: info.timestamp.timestamp(),
        is_from_me: info.source.is_from_me,
        quoted_msg_id,
        quoted_text,
        quoted_sender,
        is_forwarded,
        forwarding_score,
        reactions: vec![],
    })
}
