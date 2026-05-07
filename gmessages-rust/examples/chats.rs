//! List the most recent conversations and the last few messages in each.
//!
//! ```bash
//! cargo run -p gmessages-rust --example chats
//! ```

use std::sync::Arc;

use gmessages_rust::gmproto::conversations::{Message, message_info};
use gmessages_rust::{AuthData, Client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let auth_path = std::env::var("AUTH_PATH").unwrap_or_else(|_| "gmessages-auth.json".into());
    let auth: AuthData = serde_json::from_slice(&std::fs::read(&auth_path)?)?;

    let client = Arc::new(Client::new(auth));
    let _events = client.take_event_receiver().await.unwrap();

    // Persist refreshed auth back to disk.
    let path_for_cb = auth_path.clone();
    client
        .set_auth_changed_callback(Arc::new(move |a| {
            if let Ok(json) = serde_json::to_vec_pretty(a) {
                let _ = std::fs::write(&path_for_cb, &json);
            }
        }))
        .await;

    client.connect().await?;

    // Give the long-poll a moment to settle.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let convs = client.list_conversations(20).await?;
    println!("=== {} conversations ===", convs.conversations.len());
    for c in &convs.conversations {
        let kind = match c.r#type {
            1 => "sms",
            2 => "rcs",
            n => return Err(anyhow::anyhow!("unknown conv type {n}")),
        };
        let participants: Vec<String> = c
            .participants
            .iter()
            .filter(|p| p.is_visible && !p.is_me)
            .map(|p| {
                if !p.full_name.is_empty() {
                    p.full_name.clone()
                } else {
                    p.id.as_ref().map(|id| id.participant_id.clone()).unwrap_or_default()
                }
            })
            .collect();
        let last = c
            .latest_message
            .as_ref()
            .map(|lm| {
                let s = lm.display_content.replace('\n', " ");
                if s.len() > 80 { format!("{}…", &s[..80]) } else { s }
            })
            .unwrap_or_default();
        println!(
            "{}  [{}] {}  →  {}",
            c.conversation_id,
            kind,
            participants.join(", "),
            last,
        );
    }

    if let Some(first) = convs.conversations.first() {
        println!("\n=== history of {} (last 10) ===", first.conversation_id);
        let history = client.fetch_messages(&first.conversation_id, 10).await?;
        for m in history.messages.iter().rev() {
            let from = if m.participant_id.is_empty() {
                "<self>".into()
            } else {
                m.participant_id.clone()
            };
            println!("  [{}] {}: {}", m.timestamp, from, short_body(m));
        }
    }

    Ok(())
}

fn short_body(m: &Message) -> String {
    for info in &m.message_info {
        match &info.data {
            Some(message_info::Data::MessageContent(c)) if !c.content.is_empty() => {
                let s = c.content.replace('\n', " ");
                if s.len() > 80 {
                    return format!("{}…", &s[..80]);
                }
                return s;
            }
            Some(message_info::Data::MediaContent(media)) => {
                return format!("[{} {}b]", media.mime_type, media.size);
            }
            _ => {}
        }
    }
    "<empty>".into()
}
