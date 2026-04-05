//! Async bridge between Tokio (whatsapp-rust) and GTK main loop.
//!
//! GTK must run on the main thread. Tokio runs in a background thread pool.
//! We use glib::MainContext::channel to safely push events from Tokio → GTK.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Events that the Tokio runtime sends to the GTK UI.
#[derive(Debug, Clone)]
pub enum WaEvent {
    /// QR code string — display as scannable code
    QrCode(String),
    /// Successfully connected and authenticated
    Connected {
        phone: String,
        name: String,
    },
    /// Connection dropped
    Disconnected(String),
    /// Chat list refreshed (batch, used for initial load)
    ChatsLoaded(Vec<ChatSummary>),
    /// Single chat appeared from history sync (incremental)
    ChatAdded(ChatSummary),
    /// New or updated message arrived
    MessageReceived(IncomingMessage),
    /// Server confirmed an outgoing message; GTK should re-key the optimistic bubble.
    /// `tmp_id` is the local ID the GTK side used; `real_id` is the server-assigned ID.
    MessageConfirmed {
        tmp_id: String,
        real_id: String,
        chat_id: String,
    },
    /// An outgoing message failed to reach the server (show red ✗, allow resend).
    MessageFailed {
        msg_id: String,
        chat_id: String,
    },
    /// History messages for a chat the user just opened
    HistoryMessages {
        chat_id: String,
        chat_name: String,
        messages: Vec<IncomingMessage>,
    },
    /// Typing indicator for a chat
    TypingIndicator {
        chat_id: String,
        sender_name: String,
        is_typing: bool,
    },
    /// Message delivery/read receipt updated
    ReceiptUpdate {
        msg_id: String,
        status: ReceiptStatus,
    },
    /// Chat was read on another device — reset unread badge
    ChatReadOnOtherDevice {
        chat_id: String,
    },
    /// Poll vote received and decoded
    PollVoteUpdate {
        chat_id: String,
        poll_msg_id: String,
        all_votes: Vec<(String, Vec<String>)>,
    },
    /// Own profile data loaded
    OwnProfile {
        name: String,
        about: String,
        description: String,
        email: String,
        website: String,
        address: String,
        category: String,
    },
    /// History/offline sync progress — true while syncing, false when complete
    SyncProgress(bool),
    /// A message was edited (by sender or self on another device)
    MessageEdited {
        chat_id: String,
        msg_id: String,
        new_text: String,
    },
    /// Error message to display as a toast notification
    ErrorToast(String),
    /// A chat's display name was resolved/updated (group name fetch or push name)
    ChatNameUpdated {
        chat_id: String,
        name: String,
    },
    /// A media file has been downloaded and is ready to display
    MediaReady {
        msg_id: String,
        chat_id: String,
        path: String,
        media_type: MediaType,
    },
    /// A profile picture was downloaded and is ready to show on the avatar
    AvatarReady {
        chat_id: String,
        path: String,
    },
    // ── Chat context-menu responses ────────────────────────────────────────────
    ChatArchived {
        chat_id: String,
        archived: bool,
    },
    ChatMuted {
        chat_id: String,
        muted: bool,
    },
    ChatPinned {
        chat_id: String,
        pinned: bool,
    },
    ChatMarkedUnread {
        chat_id: String,
    },
    ChatFavorited {
        chat_id: String,
        favorite: bool,
    },
    ChatDeleted {
        chat_id: String,
    },
    ChatCleared {
        chat_id: String,
    },
    ChatLabeled {
        chat_id: String,
        label: Option<String>,
    },
    // ── Message action responses ──────────────────────────────────────────────
    ReactionUpdated {
        chat_id: String,
        msg_id: String,
        emoji: String,
    },
    MessageStarred {
        chat_id: String,
        msg_id: String,
        starred: bool,
    },
    MessagePinned {
        chat_id: String,
        msg_id: String,
    },
    MessageDeletedLocal {
        chat_id: String,
        msg_id: String,
    },
    /// Chat list preview text updated (e.g., after name resolution)
    ChatPreviewUpdated {
        chat_id: String,
        preview: String,
    },
    ForwardComplete {
        to_chat_id: String,
        count: u32,
    },
    /// Chat list snapshot for the forward picker dialog
    ChatListForPicker(Vec<ChatSummary>),
    // ── Profile events ────────────────────────────────────────────────────────
    GroupMembers {
        chat_id: String,
        members: Vec<GroupMember>,
    },
    GroupsInCommon {
        chat_id: String,
        groups: Vec<ChatSummary>,
    },
    QuickRepliesSynced {
        replies: Vec<QuickReplyData>,
    },
    ContactProfile {
        chat_id: String,
        phone: String,
        about: Option<String>,
        avatar_path: Option<String>,
    },
    GroupProfile {
        chat_id: String,
        subject: String,
        description: Option<String>,
        participants: Vec<GroupMember>,
        i_am_admin: bool,
    },
    /// GIF search results from Tenor
    GifResults {
        gifs: Vec<GifResult>,
    },
    /// Sticker search results
    StickerResults {
        stickers: Vec<GifResult>,
    },
    PhoneLookupResult {
        phone: String,
        jid: Option<String>,
        is_registered: bool,
    },
    GroupInviteLink {
        chat_id: String,
        link: String,
    },
    // ── Global search ────────────────────────────────────────────────────
    GlobalSearchResults {
        query: String,
        results: Vec<SearchHit>,
    },
    // ── Calls ────────────────────────────────────────────────────────────
    IncomingCall {
        chat_id: String,
        caller_name: String,
        is_video: bool,
    },
    CallEnded {
        chat_id: String,
        reason: String,
    },
    CallAccepted {
        chat_id: String,
    },
    // ── Broadcast / multi-send ───────────────────────────────────────────
    MultiSendProgress {
        sent: u32,
        total: u32,
        current_chat: String,
    },
    MultiSendComplete {
        sent: u32,
        failed: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub chat_id: String,
    pub chat_name: String,
    pub msg_id: String,
    pub sender_name: String,
    pub text: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct GifResult {
    pub preview_url: String,
    pub mp4_url: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct QuickReplyData {
    pub shortcut: String,
    pub message: String,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GroupMember {
    pub jid: String,
    pub name: String,
    pub is_admin: bool,
}

/// Commands the GTK UI sends to the Tokio runtime.
#[derive(Debug)]
pub enum WaCommand {
    /// `tmp_id` is the local ID already shown in the UI — runtime will confirm/fail it.
    SendText {
        chat_id: String,
        text: String,
        tmp_id: String,
        mentioned_jids: Vec<String>,
    },
    /// `tmp_id` same as SendText.
    SendReply {
        chat_id: String,
        text: String,
        quoted_msg_id: String,
        quoted_sender: String,
        tmp_id: String,
        mentioned_jids: Vec<String>,
    },
    /// Resend a previously-failed outgoing message. `msg_id` is the failed bubble's ID.
    ResendMessage {
        chat_id: String,
        msg_id: String,
        text: String,
    },
    ForwardMessage {
        to_chat_id: String,
        original_msg_id: String,
    },
    DeleteForEveryone {
        chat_id: String,
        msg_id: String,
    },
    LoadChat {
        chat_id: String,
        chat_name: String,
    },
    SetTyping {
        chat_id: String,
        is_typing: bool,
    },
    MarkRead {
        chat_id: String,
    },
    Logout,
    /// Set own profile picture from a file path
    SetProfilePicture { path: String },
    // ── Chat context-menu actions ──────────────────────────────────────────────
    ArchiveChat {
        chat_id: String,
        archived: bool,
    },
    MuteChat {
        chat_id: String,
        muted: bool,
    },
    PinChat {
        chat_id: String,
        pinned: bool,
    },
    LabelChat {
        chat_id: String,
        label: Option<String>,
    },
    MarkUnread {
        chat_id: String,
    },
    FavoriteChat {
        chat_id: String,
        favorite: bool,
    },
    BlockContact {
        chat_id: String,
    },
    ClearChat {
        chat_id: String,
    },
    DeleteChat {
        chat_id: String,
    },
    // ── Message actions ───────────────────────────────────────────────────────
    SendReaction {
        chat_id: String,
        msg_id: String,
        emoji: String,
        sender_jid: String,
        is_from_me: bool,
    },
    StarMessage {
        chat_id: String,
        msg_id: String,
        starred: bool,
        sender_jid: String,
        is_from_me: bool,
    },
    PinMessage {
        chat_id: String,
        msg_id: String,
    },
    DeleteForMe {
        chat_id: String,
        msg_id: String,
        sender_jid: String,
        is_from_me: bool,
    },
    ForwardMessages {
        to_chat_id: String,
        msg_ids: Vec<String>,
    },
    /// Send an image file as a message
    SendImage {
        chat_id: String,
        path: String,
        caption: Option<String>,
        tmp_id: String,
    },
    /// Search for GIFs via Tenor
    SearchGifs {
        query: String,
    },
    /// Send a GIF by downloading from URL and sending as video with gif_playback
    SendGif {
        chat_id: String,
        mp4_url: String,
        tmp_id: String,
    },
    /// Search stickers via Tenor
    SearchStickers {
        query: String,
    },
    /// Send a sticker by downloading WebP from URL
    SendSticker {
        chat_id: String,
        webp_url: String,
        tmp_id: String,
    },
    /// Send a contact card to a chat
    SendContact {
        to_chat_id: String,
        contact_name: String,
        contact_phone: String,
        tmp_id: String,
    },
    /// Save message text to wa_notes.txt
    SaveNote {
        text: String,
    },
    /// Request chat list for the forward picker dialog
    GetChatList,
    // ── Profile & new chat ────────────────────────────────────────────────────
    GetGroupMembers {
        chat_id: String,
    },
    CreateGroup {
        subject: String,
        participants: Vec<String>,
    },
    /// Sync quick replies from the connected WhatsApp account's app state
    SyncQuickReplies,
    /// Create or update a quick reply and sync to WhatsApp
    SaveQuickReply {
        shortcut: String,
        message: String,
    },
    /// Delete a quick reply and sync to WhatsApp
    DeleteQuickReply {
        shortcut: String,
    },
    GetContactProfile {
        chat_id: String,
    },
    GetGroupInfo {
        chat_id: String,
    },
    SetGroupSubject {
        chat_id: String,
        subject: String,
    },
    SendPoll {
        chat_id: String,
        question: String,
        options: Vec<String>,
        selectable_count: u32,
    },
    VotePoll {
        chat_id: String,
        poll_msg_id: String,
        poll_creator: String,
        poll_secret: Vec<u8>,
        selected_options: Vec<String>,
    },
    SetPushName {
        name: String,
    },
    SetStatus {
        text: String,
    },
    GetOwnProfile,
    CheckOnWhatsApp {
        phone: String,
    },
    StartNewChat {
        jid: String,
    },
    // ── Message editing ──────────────────────────────────────────────────────
    EditMessage {
        chat_id: String,
        msg_id: String,
        new_text: String,
    },
    // ── Group management ─────────────────────────────────────────────────────
    AddGroupParticipant {
        chat_id: String,
        phone: String,
    },
    RemoveGroupParticipant {
        chat_id: String,
        jid: String,
    },
    PromoteGroupAdmin {
        chat_id: String,
        jid: String,
    },
    DemoteGroupAdmin {
        chat_id: String,
        jid: String,
    },
    LeaveGroup {
        chat_id: String,
    },
    GetGroupInviteLink {
        chat_id: String,
    },
    // ── Disappearing messages ────────────────────────────────────────────────
    SetDisappearing {
        chat_id: String,
        /// 0 = off, 86400 = 24h, 604800 = 7d, 7776000 = 90d
        duration_secs: u32,
    },
    // ── Voice note ───────────────────────────────────────────────────────────
    SendAudio {
        chat_id: String,
        path: String,
        duration_secs: u32,
        is_voice_note: bool,
        tmp_id: String,
    },
    // ── Calls ────────────────────────────────────────────────────────────────
    InitiateCall {
        chat_id: String,
        is_video: bool,
    },
    AcceptCall {
        chat_id: String,
    },
    RejectCall {
        chat_id: String,
    },
    EndCall {
        chat_id: String,
    },
    // ── Global search ────────────────────────────────────────────────────────
    SearchAllMessages {
        query: String,
    },
    // ── Multi-send / broadcast ───────────────────────────────────────────────
    /// Send same message to multiple chats with randomised delays (anti-ban)
    MultiSend {
        chat_ids: Vec<String>,
        text: String,
    },
    /// Forward a message to multiple chats with randomised delays
    MultiForward {
        chat_ids: Vec<String>,
        original_msg_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub name: String,
    pub last_message: String,
    pub timestamp: i64,
    pub unread_count: u32,
    pub is_group: bool,
    pub is_muted: bool,
    pub is_pinned: bool,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_favorite: bool,
    #[serde(default)]
    pub label: Option<String>,
    /// ID of the pinned message in this chat (if any)
    #[serde(default)]
    pub pinned_msg_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Local filesystem path after download (None until downloaded)
    #[serde(default)]
    pub media_local_path: Option<String>,
    /// Original filename for documents
    #[serde(default)]
    pub media_filename: Option<String>,
    /// Caption text accompanying an image/video
    #[serde(default)]
    pub media_caption: Option<String>,
    /// Caption text accompanying an image/video — duplicate removed
    /// Contact card data (vCard)
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub contact_vcard: Option<String>,
    /// Link preview data (OpenGraph)
    #[serde(default)]
    pub link_title: Option<String>,
    #[serde(default)]
    pub link_description: Option<String>,
    #[serde(default)]
    pub link_url: Option<String>,
    #[serde(default)]
    pub link_thumbnail_path: Option<String>,
    /// Media path of the quoted (replied-to) message, for showing thumbnail in reply context
    #[serde(default)]
    pub quoted_media_path: Option<String>,
    /// Poll data: (question, options, selectable_count)
    #[serde(default)]
    pub poll_question: Option<String>,
    #[serde(default)]
    pub poll_options: Vec<String>,
    #[serde(default)]
    pub poll_selectable: u32,
    /// Poll message secret (needed for vote encryption)
    #[serde(default)]
    pub poll_secret: Vec<u8>,
    /// Poll votes: voter_name → vec of selected option names
    #[serde(default)]
    pub poll_votes: Vec<(String, Vec<String>)>,
    /// Receipt status for sent messages (Pending → Sent → Delivered → Read)
    #[serde(default = "default_receipt_status")]
    pub receipt_status: ReceiptStatus,
    /// Whether this message has been edited
    #[serde(default)]
    pub is_edited: bool,
    /// System/notification message (centered gray text, no bubble)
    /// e.g. "You added ~Derek Bevilacqua", "John left"
    #[serde(default)]
    pub is_system_message: bool,
}

fn default_receipt_status() -> ReceiptStatus {
    ReceiptStatus::Pending
}

impl IncomingMessage {
    /// Create a simple outgoing text message with all optional fields defaulted.
    pub fn outgoing(id: String, chat_id: String, text: Option<String>, timestamp: i64) -> Self {
        Self {
            id,
            chat_id,
            sender_id: String::new(),
            sender_name: String::new(),
            text,
            media_type: None,
            timestamp,
            is_from_me: true,
            is_forwarded: false,
            forwarding_score: 0,
            quoted_msg_id: None,
            quoted_text: None,
            quoted_sender: None,
            reactions: vec![],
            media_local_path: None,
            media_filename: None,
            media_caption: None,
            contact_name: None,
            contact_vcard: None,
            link_title: None,
            link_description: None,
            link_url: None,
            link_thumbnail_path: None,
            quoted_media_path: None,
            poll_question: None,
            poll_options: vec![],
            poll_selectable: 0,
            poll_secret: vec![],
            poll_votes: vec![],
            receipt_status: ReceiptStatus::Pending,
            is_edited: false,
            is_system_message: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaType {
    Image,
    Video,
    Audio,
    Document,
    Sticker,
    Gif,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReceiptStatus {
    /// Optimistic — message is in flight, not yet confirmed by server
    Pending,
    Sent,
    Delivered,
    Read,
    /// Server rejected / network error — show red ✗ and allow resend
    Failed,
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
