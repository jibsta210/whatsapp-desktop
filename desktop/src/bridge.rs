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
    /// Authoritative refresh of one chat's sidebar row. The row renders this
    /// VERBATIM — preview, timestamp, unread, flags — no guards, no UI-side
    /// clocks. Producer contract: ids starting "gm:" are emitted ONLY by the
    /// gmessages runtime; all other ids ONLY by the WA runtime (single owner
    /// per id). Producers guarantee monotonicity — never emit older-than-last
    /// state for a chat. This is the ONLY event that writes row state; the
    /// sidebar is a pure projection of the owning runtime's persisted summary.
    ChatRowChanged(ChatSummary),
    /// New or updated message arrived
    MessageReceived(Box<IncomingMessage>),
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
    /// A scroll-back page, oldest-first. `has_more` is false once the top of
    /// the chat is reached so the view stops asking.
    OlderMessages {
        chat_id: String,
        messages: Vec<IncomingMessage>,
        has_more: bool,
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
        /// True if the edited message is the chat's latest — only then should the
        /// sidebar preview be refreshed (editing an older message must not touch it).
        is_latest: bool,
    },
    /// An outgoing edit failed — restore the edited text so it isn't lost.
    EditFailed {
        chat_id: String,
        msg_id: String,
        new_text: String,
    },
    /// A group participant's real name resolved — refresh open bubbles.
    SenderNameResolved {
        chat_id: String,
        sender_id: String,
        name: String,
    },
    /// Error message to display as a toast notification
    ErrorToast(String),
    /// Neutral/positive confirmation to display as a toast (e.g. "Contact blocked").
    InfoToast(String),
    /// An update is staged. Rendered as a toast with a "Restart now" button so the
    /// user doesn't have to hunt for it in Settings.
    UpdateReadyToast(String),
    /// A verification code found on a message that never crossed the live-push
    /// path. The relay routinely lands an SMS on disk via the server-fetch merge
    /// instead, and that route used to bypass 2FA scanning entirely — the code
    /// was visible in the chat but never reached the clipboard.
    TwoFactorCodeDetected {
        msg_id: String,
        code: String,
        sender: String,
    },
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
    /// Link-preview metadata fetched for a message whose sender didn't attach one.
    LinkPreviewReady {
        chat_id: String,
        msg_id: String,
        url: String,
        title: Option<String>,
        description: Option<String>,
        thumbnail_url: Option<String>,
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
        /// The message's FULL deduped reactions (sender_jid, emoji) — the UI
        /// rebuilds the whole row from this so it groups/dedups and handles
        /// removals (empty vec clears the row).
        reactions: Vec<(String, String)>,
        /// True if the reacted-to message is the chat's latest — only then should
        /// the reaction show as the sidebar preview ("Reacted 👍").
        is_latest: bool,
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
        /// The new chat-list preview to show, if the deleted message affected it.
        /// `Some("🚫 Message deleted")` when the latest message was revoked for
        /// everyone; `Some(<preview of the new latest message>)` for delete-for-me;
        /// `None` when an older message was deleted (the preview must not change).
        new_preview: Option<String>,
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
        request_id: u64,
        query: String,
        gifs: Vec<GifResult>,
        error: Option<String>,
    },
    /// Sticker search results
    StickerResults {
        request_id: u64,
        query: String,
        stickers: Vec<GifResult>,
        error: Option<String>,
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
    /// Scroll-back paging: fetch the batch immediately older than
    /// `before_timestamp` for `chat_id`.
    LoadOlderMessages {
        chat_id: String,
        before_timestamp: i64,
    },
    SetTyping {
        chat_id: String,
        is_typing: bool,
    },
    MarkRead {
        chat_id: String,
    },
    /// The UI opened/closed a chat. Lets the runtime own unread counting: a
    /// message for the actively-viewed chat isn't counted as unread. `None`
    /// means no chat is open.
    SetActiveChat {
        chat_id: Option<String>,
    },
    /// INTERNAL (gm→WA): an SMS/MMS landed on (or was sent from, or was
    /// reacted-to on) a chat merged into a WhatsApp row. The WA runtime — sole
    /// owner of non-gm chat summaries — applies it to RuntimeState.chats
    /// (monotonic guards: timestamp never moves backward, empty preview never
    /// clobbers non-empty), persists, and emits ChatRowChanged. Replaces the
    /// old `touch_wa_chat_preview` behind-the-back disk write.
    /// `ephemeral: true` means render-but-don't-persist (reaction previews —
    /// restart intentionally shows the underlying message again).
    TouchChatSummary {
        chat_id: String,
        preview: String,
        timestamp: i64,
        is_from_me: bool,
        ephemeral: bool,
    },
    /// Toggle "auto-mark read on receive" for a chat (local-only flag,
    /// not synced to phone). When enabled, incoming messages trigger
    /// MarkRead automatically so the chat stays visually read.
    SetAutoMarkRead {
        chat_id: String,
        enabled: bool,
    },
    Logout,
    /// Trigger a Google Messages re-pair: wipe auth, force the runtime to
    /// run pairing flow again, and surface the QR in Settings.
    GmessagesRepair,
    /// Set own profile picture from a file path
    SetProfilePicture {
        path: String,
    },
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
    /// On-demand download of a received message's attachment — fired when the
    /// user clicks an undownloaded media/document placeholder. Re-fetches using
    /// the keys persisted on the message and emits `MediaReady` when done.
    RequestMediaDownload {
        chat_id: String,
        msg_id: String,
    },
    /// Search for GIFs via Tenor
    SearchGifs {
        request_id: u64,
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
        request_id: u64,
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
    /// Auto-mark received messages as read (per-chat, usually for groups).
    /// When true, every incoming MessageReceived fires a MarkRead cmd so
    /// the chat is never unread on desktop — useful for noisy groups.
    #[serde(default)]
    pub auto_mark_read: bool,
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
    /// WhatsApp media-download keys, persisted so an attachment that never
    /// downloaded (history-synced, or a failed/skipped auto-download) can be
    /// re-fetched on demand when the user clicks it. `#[serde(default)]` keeps
    /// old `wa_messages/*.bin` files readable (bincode tolerates a new field
    /// only with a default).
    #[serde(default)]
    pub media_download: Option<MediaDownloadKeys>,
}

/// The minimal WhatsApp media-download key set needed to (re)fetch and decrypt a
/// message's attachment at any time — even after a restart or a failed initial
/// download. Stored on [`IncomingMessage::media_download`] and used to rebuild a
/// download request from the persisted message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDownloadKeys {
    pub direct_path: String,
    pub media_key: Vec<u8>,
    pub enc_sha256: Vec<u8>,
    pub sha256: Vec<u8>,
    #[serde(default)]
    pub file_length: u64,
    #[serde(default)]
    pub mimetype: Option<String>,
}

/// Origin protocol for an [`IncomingMessage`]. Derived at render time from
/// `chat_id` — kept out of the struct itself because bincode 1.x doesn't
/// tolerate added fields in on-disk serialized data, which would break the
/// existing WhatsApp message history cache (`wa_messages/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageSource {
    #[default]
    WhatsApp,
    /// SMS / MMS / RCS via Google Messages (paired phone relay).
    GoogleMessages,
}

/// Persistent per-chat send-protocol preference. For merged chats (contact
/// has both WhatsApp and SMS), this controls which protocol the next
/// outgoing text goes through. Stored in
/// `<data_dir>/send_mode_prefs.json` as `{ chat_id: "whatsapp" | "sms" }`.
///
/// Default behavior when no preference exists: WhatsApp (richer features).
pub mod send_mode {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::sync::RwLock;

    const FILE: &str = "send_mode_prefs.json";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        WhatsApp,
        Sms,
    }

    impl Mode {
        fn as_str(self) -> &'static str {
            match self {
                Mode::WhatsApp => "whatsapp",
                Mode::Sms => "sms",
            }
        }
        fn from_str(s: &str) -> Option<Self> {
            match s {
                "whatsapp" => Some(Mode::WhatsApp),
                "sms" => Some(Mode::Sms),
                _ => None,
            }
        }
    }

    fn map() -> &'static RwLock<HashMap<String, String>> {
        static M: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
        M.get_or_init(|| {
            let map = std::fs::read_to_string(FILE)
                .ok()
                .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
                .unwrap_or_default();
            RwLock::new(map)
        })
    }

    /// Read the user's preference for `chat_id`. Returns None when the user
    /// has never made a choice for this chat.
    pub fn get(chat_id: &str) -> Option<Mode> {
        map()
            .read()
            .ok()
            .and_then(|m| m.get(chat_id).and_then(|s| Mode::from_str(s)))
    }

    /// Set and persist.
    pub fn set(chat_id: &str, mode: Mode) {
        if let Ok(mut m) = map().write() {
            m.insert(chat_id.to_string(), mode.as_str().to_string());
            // Persist eagerly — small file, infrequent change.
            if let Ok(json) = serde_json::to_vec_pretty(&*m) {
                let _ = std::fs::write(FILE, json);
            }
        }
    }
}

/// Synthetic chat ID for the "Verification Codes" inbox — every incoming
/// SMS detected as a 2FA / OTP code is routed here instead of opening a
/// new chat per shortcode (TD, Aeroplan, Google, etc.). Searchable in one
/// place; the originating sender is preserved in `IncomingMessage::sender_name`.
pub const VERIFICATION_CODES_CHAT_ID: &str = "gm:verification-codes";

/// Detect a 2FA / verification code in an SMS body. Returns the digits-only
/// code if the message looks like a transactional auth code from a bank,
/// service, or app. Requires both:
///
/// 1. A 4-8 digit run (with optional hyphen separator like Google's
///    `123-456`)
/// 2. Context keywords nearby — `"code"`, `"verify"`, `"OTP"`, `"PIN"`,
///    `"passcode"`, `"auth"`, `"login"`, `"security"`, `"confirm"`, etc.
///
/// Filters out e.g. someone texting their jersey number.
pub fn detect_two_factor_code(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    static KEYWORDS: &[&str] = &[
        "code",
        "verification",
        "verify",
        "verifying",
        "otp",
        "one-time",
        "one time",
        "passcode",
        "pin",
        "auth",
        "authenticat",
        "log in",
        "login",
        "sign in",
        "signin",
        "security",
        "confirm",
    ];
    if !KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return None;
    }
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut digits = String::new();
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c.is_ascii_digit() {
                digits.push(c);
                i += 1;
            } else if c == '-' && (i + 1 < bytes.len()) && (bytes[i + 1] as char).is_ascii_digit() {
                i += 1;
            } else {
                break;
            }
        }
        if digits.len() >= 4 && digits.len() <= 8 {
            let prev_char: String = body[..start].chars().rev().take(3).collect();
            if digits.len() == 4 && prev_char.contains('/') {
                continue;
            }
            return Some(digits);
        }
    }
    None
}

impl MessageSource {
    /// Detect the source protocol from the chat ID prefix.
    pub fn from_chat_id(chat_id: &str) -> Self {
        if chat_id.starts_with("gm:") {
            Self::GoogleMessages
        } else {
            Self::WhatsApp
        }
    }

    /// Detect the source from any field that might carry the `gm:` tag.
    /// Used by the bubble renderer because `chat_id` may be rewritten to
    /// the WhatsApp JID after a Phase 2 merge — we still want the SMS
    /// bubble blue. Both the chat_id and the message_id are checked.
    pub fn from_message(chat_id: &str, message_id: &str) -> Self {
        if chat_id.starts_with("gm:") || message_id.starts_with("gm:") {
            Self::GoogleMessages
        } else {
            Self::WhatsApp
        }
    }
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
            media_download: None,
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
        if self.cmd_tx.send(cmd).is_err() {
            log::error!("WhatsApp command channel is closed");
        }
    }
}

#[cfg(test)]
mod persistence_regression {
    //! Tripwire tests for the on-disk bincode formats. If you ADD a field to
    //! `ChatSummary` or `IncomingMessage`, bincode 1.x will NOT read old files
    //! (`#[serde(default)]` does not apply mid-`Vec`), so you MUST bump
    //! `BIN_HEADER` and add a legacy-decoder fallback (see `load_chats` /
    //! `load_messages` / `gm_load_chats_cache`). These tests just guard that the
    //! current structs remain bincode-round-trippable.
    use super::*;

    #[test]
    fn chat_summary_bincode_roundtrips() {
        let c = ChatSummary {
            id: "123@g.us".into(),
            name: "Group".into(),
            last_message: "hi".into(),
            timestamp: 42,
            unread_count: 3,
            is_group: true,
            is_muted: false,
            is_pinned: true,
            is_archived: false,
            is_favorite: true,
            label: Some("Work".into()),
            pinned_msg_id: None,
            auto_mark_read: false,
        };
        let bytes = bincode::serialize(&c).expect("serialize");
        let back: ChatSummary = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            (c.id, c.unread_count, c.is_favorite, c.auto_mark_read),
            (
                back.id,
                back.unread_count,
                back.is_favorite,
                back.auto_mark_read
            )
        );
    }

    #[test]
    fn incoming_message_bincode_roundtrips() {
        let m = IncomingMessage::outgoing(
            "m1".into(),
            "chat@s.whatsapp.net".into(),
            Some("hey".into()),
            7,
        );
        let bytes = bincode::serialize(&m).expect("serialize");
        let back: IncomingMessage = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(
            (m.id, m.text, m.timestamp),
            (back.id, back.text, back.timestamp)
        );
    }
}

#[cfg(test)]
mod td_2fa_tests {
    use super::detect_two_factor_code;

    #[test]
    fn td_real_world_formats() {
        let cases = [
            ("If you DID NOT initiate contact with TD, do not share this code and call the number on the back of your TD Card. Your one-time passcode is 098757.", "098757"),
            ("Please use 318078 as your TD security code to log in. We will never contact you for this code. Do not reveal it to anyone else.", "318078"),
            ("TD will not send you sign-in links by text. Beware of scams. Do not reveal this code. We will not contact you for it. Your security code is 641333.", "641333"),
        ];
        for (body, want) in cases {
            assert_eq!(detect_two_factor_code(body).as_deref(), Some(want), "body: {body}");
        }
    }
}

/// `IncomingMessage` as persisted before `media_download` was appended
/// (2026-06-16). bincode is positional and ignores `#[serde(default)]`, so a file
/// last written before that commit only decodes through this shape.
#[derive(Deserialize)]
pub struct LegacyIncomingMessageV2 {
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
    pub media_local_path: Option<String>,
    pub media_filename: Option<String>,
    pub media_caption: Option<String>,
    pub contact_name: Option<String>,
    pub contact_vcard: Option<String>,
    pub link_title: Option<String>,
    pub link_description: Option<String>,
    pub link_url: Option<String>,
    pub link_thumbnail_path: Option<String>,
    pub quoted_media_path: Option<String>,
    pub poll_question: Option<String>,
    pub poll_options: Vec<String>,
    pub poll_selectable: u32,
    pub poll_secret: Vec<u8>,
    pub poll_votes: Vec<(String, Vec<String>)>,
    pub receipt_status: ReceiptStatus,
    pub is_edited: bool,
    pub is_system_message: bool,
}

impl From<LegacyIncomingMessageV2> for IncomingMessage {
    fn from(m: LegacyIncomingMessageV2) -> Self {
        Self {
            id: m.id,
            chat_id: m.chat_id,
            sender_id: m.sender_id,
            sender_name: m.sender_name,
            text: m.text,
            media_type: m.media_type,
            timestamp: m.timestamp,
            is_from_me: m.is_from_me,
            quoted_msg_id: m.quoted_msg_id,
            quoted_text: m.quoted_text,
            quoted_sender: m.quoted_sender,
            is_forwarded: m.is_forwarded,
            forwarding_score: m.forwarding_score,
            reactions: m.reactions,
            media_local_path: m.media_local_path,
            media_filename: m.media_filename,
            media_caption: m.media_caption,
            contact_name: m.contact_name,
            contact_vcard: m.contact_vcard,
            link_title: m.link_title,
            link_description: m.link_description,
            link_url: m.link_url,
            link_thumbnail_path: m.link_thumbnail_path,
            quoted_media_path: m.quoted_media_path,
            poll_question: m.poll_question,
            poll_options: m.poll_options,
            poll_selectable: m.poll_selectable,
            poll_secret: m.poll_secret,
            poll_votes: m.poll_votes,
            receipt_status: m.receipt_status,
            is_edited: m.is_edited,
            is_system_message: m.is_system_message,
            media_download: None,
        }
    }
}

