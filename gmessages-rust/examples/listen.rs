//! Listen example: connects with stored AuthData and prints incoming events.
//!
//! ```bash
//! AUTH_PATH=./gmessages-auth.json cargo run -p gmessages-rust --example listen
//! ```

use gmessages_rust::gmproto::conversations::{Message, message_info};
use gmessages_rust::{AuthData, Client, Event};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let auth_path = std::env::var("AUTH_PATH").unwrap_or_else(|_| "gmessages-auth.json".into());
    let auth: AuthData = match std::fs::read(&auth_path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(_) => {
            anyhow::bail!("no auth at {auth_path}; run `--example pair` first");
        }
    };

    let client = std::sync::Arc::new(Client::new(auth));
    let mut events = client.take_event_receiver().await.unwrap();

    // Persist refreshed AuthData every time the token rotates.
    let path_for_cb = auth_path.clone();
    client
        .set_auth_changed_callback(std::sync::Arc::new(move |a| {
            if let Ok(json) = serde_json::to_vec_pretty(a)
                && let Err(e) = std::fs::write(&path_for_cb, &json)
            {
                eprintln!("failed to persist auth: {e}");
            }
        }))
        .await;

    client.connect().await?;

    while let Some(event) = events.recv().await {
        match event {
            Event::Ready => println!("[ready]"),
            Event::Messages {
                messages,
                timestamp: _,
            } => {
                for m in &messages {
                    let body = extract_body(m);
                    let from = if m.participant_id.is_empty() {
                        "<self>"
                    } else {
                        &m.participant_id
                    };
                    let kind = match m.r#type {
                        1 => "sms",
                        2 => "mms",
                        3 => "mms-pending",
                        4 => "rcs",
                        n => return Err(anyhow::anyhow!("unknown msg type {n}")),
                    };
                    let status = m.message_status.as_ref().map(|s| s.status).unwrap_or(0);
                    println!(
                        "[{kind} status={status}] {from} → conv={} : {body}",
                        m.conversation_id,
                    );
                }
            }
            Event::Typing {
                conversation_id,
                participant_id,
                typing,
            } => {
                println!("[typing] conv={conversation_id} from={participant_id} on={typing}");
            }
            Event::PhoneNotResponding => println!("[phone offline]"),
            Event::PhoneRespondingAgain => println!("[phone back]"),
            Event::AuthRevoked => {
                eprintln!("[auth revoked] re-pair needed");
                break;
            }
            other => log::debug!("{other:?}"),
        }
    }
    Ok(())
}

/// Pull text + media descriptors out of a `Message`.
fn extract_body(m: &Message) -> String {
    let mut parts = Vec::new();
    for info in &m.message_info {
        match &info.data {
            Some(message_info::Data::MessageContent(c)) => {
                if !c.content.is_empty() {
                    parts.push(c.content.clone());
                }
            }
            Some(message_info::Data::MediaContent(media)) => {
                let mime = if media.mime_type.is_empty() {
                    "?"
                } else {
                    media.mime_type.as_str()
                };
                parts.push(format!("[media {} {} bytes]", mime, media.size));
            }
            None => {}
        }
    }
    if parts.is_empty() {
        "<empty>".into()
    } else {
        parts.join(" ")
    }
}
