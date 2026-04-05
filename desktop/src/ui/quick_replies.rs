use serde::{Deserialize, Serialize};

const FILE: &str = "wa_quick_replies.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickReply {
    pub shortcut: String,
    pub text: String,
}

pub fn load() -> Vec<QuickReply> {
    let Ok(data) = std::fs::read_to_string(FILE) else {
        // No file yet — return empty, will be populated from WhatsApp sync
        return vec![];
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save(replies: &[QuickReply]) {
    if let Ok(data) = serde_json::to_string_pretty(replies) {
        let _ = std::fs::write(FILE, data);
    }
}
