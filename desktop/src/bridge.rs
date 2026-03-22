//! Async bridge between Tokio (whatsapp-rust) and GTK main loop.
//!
//! GTK must run on the main thread. Tokio runs in a background thread pool.
//! We use glib::MainContext::channel to safely push events from Tokio → GTK.

use tokio::sync::mpsc;

/// Events that the Tokio runtime sends to the GTK UI.
#[derive(Debug, Clone)]
pub enum WaEvent {
    /// QR code string — display as scannable code
    QrCode(String),
    /// Successfully connected and authenticated
    Connected { phone: String, name: String },
    /// Connection dropped
    Disconnected(String),
    /// Chat list refreshed
    ChatsLoaded(Vec<ChatSummary>),
    /// New or updated message arrived
    MessageReceived(IncomingMessage),
    /// Typing indicator for a chat
    TypingIndicator { chat_id: String, is_typing: bool },
    /// Message delivery/read receipt updated
    ReceiptUpdate { msg_id: String, status: ReceiptStatus },
}

/// Commands the GTK UI sends to the Tokio runtime.
#[derive(Debug)]
pub enum WaCommand {
    SendText { chat_id: String, text: String },
    SendReply { chat_id: String, text: String, quoted_msg_id: String, quoted_sender: String },
    ForwardMessage { to_chat_id: String, original_msg_id: String },
    DeleteForEveryone { chat_id: String, msg_id: String },
    LoadChat { chat_id: String },
    SetTyping { chat_id: String, is_typing: bool },
    MarkRead { chat_id: String },
    Logout,
}

#[derive(Debug, Clone)]
pub struct ChatSummary {
    pub id: String,
    pub name: String,
    pub last_message: String,
    pub timestamp: i64,
    pub unread_count: u32,
    pub is_group: bool,
    pub is_muted: bool,
    pub is_pinned: bool,
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub id: String,
    pub chat_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub text: Option<String>,
    pub media_type: Option<MediaType>,
    pub timestamp: i64,
    pub is_from_me: bool,
    pub quoted_msg_id: Option<String>,
    pub quoted_text: Option<String>,
    pub quoted_sender: Option<String>,
    pub is_forwarded: bool,
    pub forwarding_score: u32,
    pub reactions: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum MediaType {
    Image,
    Video,
    Audio,
    Document,
    Sticker,
    Gif,
}

#[derive(Debug, Clone)]
pub enum ReceiptStatus {
    Sent,
    Delivered,
    Read,
}

/// Shared handle — GTK side keeps the cmd_tx to send commands to Tokio.
pub struct Bridge {
    pub cmd_tx: mpsc::UnboundedSender<WaCommand>,
}

impl Bridge {
    pub fn send_command(&self, cmd: WaCommand) {
        let _ = self.cmd_tx.send(cmd);
    }
}
