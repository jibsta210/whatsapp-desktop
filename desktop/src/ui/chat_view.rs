use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, ListBox, Orientation, Revealer,
    RevealerTransitionType, ScrolledWindow, SearchEntry, Separator, TextView,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, IncomingMessage, ReceiptStatus, WaCommand};
use crate::ui::message_bubble::MessageBubble;

#[derive(Clone)]
enum PendingAttachment {
    File(String),
    Gif {
        mp4_url: String,
        preview_url: String,
    },
}

#[derive(Clone)]
struct PickerResultSet {
    request_id: u64,
    query: String,
    results: Vec<crate::bridge::GifResult>,
    error: Option<String>,
}

/// Safely remove all children from a GTK Box.
/// Guards against infinite loops if `remove` fails (non-child widget).
fn remove_all_children(parent: &Box) {
    let mut guard = 0u32;
    while let Some(child) = parent.first_child() {
        let prev = child.clone();
        parent.remove(&child);
        guard += 1;
        // If first_child() still returns the same widget after remove, it's stuck
        if guard > 10_000
            || parent
                .first_child()
                .as_ref()
                .map(|c| c == &prev)
                .unwrap_or(false)
        {
            log::warn!("remove_all_children: breaking stuck loop after {guard} iterations");
            break;
        }
    }
}

#[derive(Clone)]
pub struct ChatViewPanel {
    inner: Rc<ChatViewInner>,
}

struct ChatViewInner {
    root: Box,
    /// Message viewport + typing indicator. The header and composer deliberately
    /// live outside this surface so switching chats can animate without making
    /// either persistent interaction target flicker or move.
    conversation_surface: Box,
    /// Keep the current animation alive and make a rapid switch able to finish
    /// it before starting the next one.
    chat_switch_animation: RefCell<Option<adw::TimedAnimation>>,
    messages_box: Box,
    scroll: ScrolledWindow,
    input_view: TextView,
    send_button: Button,
    typing_box: Box,
    typing_name: Label,
    header_name: Label,
    header_subtitle: Label,
    /// Toggle button in the chat header that switches the send-target
    /// protocol between WhatsApp and SMS for the current chat. Visible
    /// only when the current chat has both protocols available.
    send_mode_btn: Button,
    /// Favourite state is stored explicitly per chat. The header icon is only
    /// a presentation of this state and is never used as the source of truth.
    favorite_button: Button,
    favorite_chats: RefCell<HashSet<String>>,
    pin_banner: Box,
    bridge: Arc<Bridge>,
    current_chat_id: RefCell<Option<String>>,
    /// Outbound typing indicator throttle: last time we sent typing=true (ms).
    typing_last_true_ms: std::cell::Cell<i64>,
    /// Pending "typing stopped" timer, rearmed on each keystroke.
    typing_stop_source: RefCell<Option<gtk4::glib::SourceId>>,
    /// The routed chat target we last sent SetTyping{true} for. Stored so
    /// cancel_typing can flush a SetTyping{false} to the RIGHT chat even after
    /// current_chat_id has moved on (chat switch, send).
    typing_target: RefCell<Option<String>>,
    /// When true, the next buffer `connect_changed` is a programmatic set_text
    /// (draft restore, edit restore, event creator) — NOT a user keystroke — so
    /// the typing-indicator block early-returns and doesn't broadcast "typing…".
    suppress_typing: Cell<bool>,
    /// The user's own JID (for avatar loading on sent messages)
    own_jid: RefCell<Option<String>>,
    own_name: RefCell<String>,
    /// LID→phone mapping for avatar resolution
    lid_to_phone: RefCell<HashMap<String, String>>,
    /// Currently typing users in the active chat
    /// Per-chat typing state so it survives chat switches
    all_typers: RefCell<HashMap<String, Vec<String>>>,
    /// Counter: >0 means "scroll to bottom on next vadjustment change"
    scroll_pending: Rc<Cell<u32>>,
    /// True when user is at or near the bottom of the scroll
    at_bottom: Rc<Cell<bool>>,
    /// "Go to latest" floating button
    goto_latest_btn: Button,
    // msg_id → bubble (for receipt updates)
    bubbles: RefCell<HashMap<String, MessageBubble>>,
    /// A message requested by an external search result before history has
    /// finished rendering. It is consumed as soon as that bubble is inserted.
    pending_message_jump: RefCell<Option<String>>,
    // msg_id → searchable text (for the in-chat search filter)
    search_texts: RefCell<HashMap<String, String>>,
    // optimistic tmp id → real server id. Bubble menu closures capture the
    // message (and its id) at bubble-creation time, when a just-sent message is
    // still keyed by its "tmp-…" id; confirm_bubble records the mapping here so
    // menu actions (edit/star/pin/react/delete/reply) resolve to the real id.
    id_remap: RefCell<HashMap<String, String>>,
    // Ordered list of (msg_id, abs_path) for visual media (images/gifs/stickers) — powers the carousel
    media_items: RefCell<Vec<(String, String)>>,
    // Last message date seen — used to insert day separator labels
    last_msg_date: RefCell<Option<chrono::NaiveDate>>,
    // For reply: the message being replied to
    reply_context: RefCell<Option<(String, String, String, Option<String>)>>, // (msg_id, sender, text, media_path)
    reply_bar: Box,
    reply_label: Label,
    search_revealer: Revealer,
    search_entry: SearchEntry,
    // Forward mode
    forward_selected: RefCell<Vec<String>>,
    forward_mode: RefCell<bool>,
    forward_bar: Box,
    forward_count_label: Label,
    input_bar: Box,
    // @ mentions
    group_members: RefCell<Vec<crate::bridge::GroupMember>>,
    mention_popover: gtk4::Popover,
    mention_list: ListBox,
    pending_mentions: RefCell<Vec<String>>,
    // / quick replies
    slash_popover: gtk4::Popover,
    slash_list: ListBox,
    quick_replies: RefCell<Vec<crate::ui::quick_replies::QuickReply>>,
    // Pending pasted image or GIF
    pending_image_path: RefCell<Option<String>>,
    pending_gif_url: RefCell<Option<String>>,
    pending_gif_preview_url: RefCell<Option<String>>,
    image_preview_bar: Box,
    image_preview_pic: gtk4::Picture,
    /// Label inside the image preview bar that shows the filename/icon
    /// for non-image attachments. Updated each time we set a new
    /// pending file so the user can tell what's queued.
    preview_label: Label,
    /// Per-chat pending attachment paths (image / document / GIF).
    /// On chat switch, the current pending attachment is moved into
    /// this map under the OLD chat id, and the NEW chat id's entry is
    /// restored as the active pending. Drafts work the same way — see
    /// `drafts` field. Without this, a screenshot pasted in chat A
    /// would persist into chat B if you switched before sending.
    pending_attachments: RefCell<HashMap<String, PendingAttachment>>,
    // Profile open callback (set by window)
    on_profile_open: RefCell<Option<std::boxed::Box<dyn Fn(String, String)>>>,
    // Emoji/GIF/Sticker panel
    emoji_popover: gtk4::Popover,
    gif_grid: gtk4::FlowBox,
    sticker_grid: gtk4::FlowBox,
    gif_status: Label,
    sticker_status: Label,
    gif_search_request: Cell<u64>,
    sticker_search_request: Cell<u64>,
    picker_page: Cell<u32>,
    gif_result_cache: RefCell<Option<PickerResultSet>>,
    sticker_result_cache: RefCell<Option<PickerResultSet>>,
    gif_rendered_request: Cell<u64>,
    sticker_rendered_request: Cell<u64>,
    // Message editing state: (chat_id, msg_id) of the message being edited
    editing_msg: RefCell<Option<(String, String)>>,
    edit_banner: Revealer,
    // AI autocorrect spinner (tiny dots indicator in the input bar)
    ai_spinner: gtk4::Spinner,
    /// When true, Enter/send waits for AI autocorrect to finish (up to 2s).
    /// When false, sends immediately even if AI is still processing.
    ac_delay_send: Rc<Cell<bool>>,
    // Per-chat draft text persistence: chat_id → draft text
    drafts: RefCell<HashMap<String, String>>,
    /// When Some, current "chat" is a send group — do_send fires MultiSend instead of SendText
    send_group_ids: RefCell<Option<Vec<String>>>,
    /// Message ids the user has starred this session. Lets the context menu
    /// offer Star ↔ Unstar as a real toggle (the backend supports both, but
    /// IncomingMessage carries no starred flag from history, so this is a
    /// best-effort per-session view seeded on each Star/Unstar action).
    starred_msgs: RefCell<HashSet<String>>,
}

#[derive(Clone, Copy)]
enum PickerSearchKind {
    Gif,
    Sticker,
}

// Runtime-side stale-request suppression is process-wide, so request IDs must
// also remain monotonic if the chat view is ever rebuilt during this process.
static PICKER_SEARCH_REQUEST_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn queue_picker_search(
    inner: &Rc<ChatViewInner>,
    kind: PickerSearchKind,
    query: String,
    debounce: bool,
) {
    use std::sync::atomic::Ordering;

    let query = query.trim().to_string();
    let (request_cell, status, grid, noun) = match kind {
        PickerSearchKind::Gif => (
            &inner.gif_search_request,
            &inner.gif_status,
            &inner.gif_grid,
            "GIFs",
        ),
        PickerSearchKind::Sticker => (
            &inner.sticker_search_request,
            &inner.sticker_status,
            &inner.sticker_grid,
            "stickers",
        ),
    };
    let request_id = PICKER_SEARCH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    request_cell.set(request_id);
    status.remove_css_class("error");

    if !query.is_empty() && query.chars().count() < 2 {
        grid.remove_all();
        grid.set_sensitive(true);
        status.set_text("Type at least 2 characters");
        return;
    }

    let effective_query = if query.is_empty() {
        "trending".to_string()
    } else {
        query
    };
    grid.set_sensitive(false);
    status.set_text(&format!("Searching Tenor {noun}…"));

    let inner = inner.clone();
    let send = move || {
        let is_current = match kind {
            PickerSearchKind::Gif => inner.gif_search_request.get() == request_id,
            PickerSearchKind::Sticker => inner.sticker_search_request.get() == request_id,
        };
        if !is_current {
            return;
        }
        let command = match kind {
            PickerSearchKind::Gif => WaCommand::SearchGifs {
                request_id,
                query: effective_query,
            },
            PickerSearchKind::Sticker => WaCommand::SearchStickers {
                request_id,
                query: effective_query,
            },
        };
        inner.bridge.send_command(command);
    };
    if debounce {
        gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(350), send);
    } else {
        send();
    }
}

const PICKER_PREVIEW_MAX_BYTES: usize = 2 * 1024 * 1024;
const PICKER_PREVIEW_QUEUE_DEPTH: usize = 24;
const PICKER_PREVIEW_WORKERS: usize = 4;

struct PickerPreviewJob {
    url: String,
    result: async_channel::Sender<std::result::Result<Vec<u8>, String>>,
}

static PICKER_PREVIEW_QUEUE: std::sync::LazyLock<std::sync::mpsc::SyncSender<PickerPreviewJob>> =
    std::sync::LazyLock::new(|| {
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<PickerPreviewJob>(PICKER_PREVIEW_QUEUE_DEPTH);
        let rx = Arc::new(std::sync::Mutex::new(rx));
        for worker in 0..PICKER_PREVIEW_WORKERS {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("tenor-preview-{worker}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let receiver = rx.lock().unwrap_or_else(|e| e.into_inner());
                            receiver.recv()
                        };
                        let Ok(job) = job else { break };
                        let result = download_picker_preview(&job.url);
                        let _ = job.result.send_blocking(result);
                    }
                });
        }
        tx
    });

fn download_picker_preview(url: &str) -> std::result::Result<Vec<u8>, String> {
    use std::io::Read as _;

    if !url.starts_with("https://") {
        return Err("invalid preview URL".to_string());
    }
    let response = ureq::get(url)
        .set("User-Agent", "whatsapp-desktop/0.1 (Tenor picker)")
        .timeout(std::time::Duration::from_secs(12))
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(128 * 1024);
    response
        .into_reader()
        .take((PICKER_PREVIEW_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > PICKER_PREVIEW_MAX_BYTES {
        return Err("preview was too large".to_string());
    }
    Ok(bytes)
}

fn load_picker_preview(
    pic: &gtk4::Picture,
    url: String,
    ready_label: Option<Label>,
    pending_gif_guard: Option<(std::rc::Weak<ChatViewInner>, String)>,
) {
    let (result_tx, result_rx) = async_channel::bounded(1);
    let job = PickerPreviewJob {
        url,
        result: result_tx,
    };
    if let Err(error) = PICKER_PREVIEW_QUEUE.try_send(job) {
        let job = match error {
            std::sync::mpsc::TrySendError::Full(job)
            | std::sync::mpsc::TrySendError::Disconnected(job) => job,
        };
        let _ = job
            .result
            .try_send(Err("preview queue is busy".to_string()));
    }

    let pic = pic.clone();
    gtk4::glib::MainContext::default().spawn_local(async move {
        let result = result_rx.recv().await;
        if let Some((inner, expected_url)) = pending_gif_guard {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            if inner.pending_gif_preview_url.borrow().as_deref() != Some(&expected_url) {
                return;
            }
        }
        match result {
            Ok(Ok(bytes)) => {
                let bytes = glib::Bytes::from(&bytes);
                match gtk4::gdk::Texture::from_bytes(&bytes) {
                    Ok(texture) => {
                        pic.set_paintable(Some(&texture));
                        pic.set_tooltip_text(None);
                        if let Some(label) = ready_label.as_ref() {
                            label.set_text("Press Enter to send, Escape to cancel");
                        }
                    }
                    Err(error) => {
                        pic.set_tooltip_text(Some(&format!("Preview unavailable: {error}")));
                        if let Some(label) = ready_label.as_ref() {
                            label.set_text(
                                "GIF preview unavailable · Enter to send or Escape to cancel",
                            );
                        }
                    }
                }
            }
            Ok(Err(error)) => {
                pic.set_tooltip_text(Some(&format!("Preview unavailable: {error}")));
                if let Some(label) = ready_label.as_ref() {
                    label.set_text("GIF preview unavailable · Enter to send or Escape to cancel");
                }
            }
            Err(_) => {
                pic.set_tooltip_text(Some("Preview unavailable"));
                if let Some(label) = ready_label.as_ref() {
                    label.set_text("GIF preview unavailable · Enter to send or Escape to cancel");
                }
            }
        }
    });
}

fn stage_pending_gif(
    inner: &Rc<ChatViewInner>,
    mp4_url: String,
    preview_url: String,
    paintable: Option<gtk4::gdk::Paintable>,
) {
    *inner.pending_gif_url.borrow_mut() = Some(mp4_url.clone());
    *inner.pending_gif_preview_url.borrow_mut() = Some(preview_url.clone());
    *inner.pending_image_path.borrow_mut() = None;
    inner
        .image_preview_pic
        .set_paintable(None::<&gtk4::gdk::Paintable>);
    inner
        .preview_label
        .set_text("Loading GIF… (Enter to send, Escape to cancel)");
    inner.image_preview_bar.set_visible(true);
    if let Some(paintable) = paintable {
        inner.image_preview_pic.set_paintable(Some(&paintable));
        inner
            .preview_label
            .set_text("Press Enter to send, Escape to cancel");
    } else {
        load_picker_preview(
            &inner.image_preview_pic,
            preview_url.clone(),
            Some(inner.preview_label.clone()),
            Some((Rc::downgrade(inner), preview_url.clone())),
        );
    }
    if let Some(chat_id) = inner.current_chat_id.borrow().clone() {
        inner.pending_attachments.borrow_mut().insert(
            chat_id,
            PendingAttachment::Gif {
                mp4_url,
                preview_url,
            },
        );
    }
    update_send_button_state(inner);
    inner.input_view.grab_focus();
}

impl ChatViewPanel {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        let root = Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.add_css_class("message-pane-bg");

        // ── Header ──
        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        header.set_show_start_title_buttons(false);
        header.add_css_class("message-pane-hdr");
        let header_title_box = Box::new(Orientation::Vertical, 0);
        header_title_box.set_halign(Align::Start);
        header_title_box.set_hexpand(true);

        let header_name = Label::new(Some("Select a chat"));
        header_name.add_css_class("title");
        header_name.set_halign(Align::Start);

        let header_subtitle = Label::new(None);
        header_subtitle.add_css_class("dim-label");
        header_subtitle.add_css_class("caption");
        header_subtitle.set_halign(Align::Start);
        header_subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        header_subtitle.set_max_width_chars(50);
        header_subtitle.set_visible(false);

        header_title_box.append(&header_name);
        header_title_box.append(&header_subtitle);
        header.set_title_widget(Some(&header_title_box));

        // Send-mode toggle (WhatsApp ↔ SMS). Hidden by default; revealed
        // when the open_chat() call detects a merged chat (contact has
        // both protocols). Click cycles the per-chat preference.
        let send_mode_btn = Button::from_icon_name("chat-message-new-symbolic");
        send_mode_btn.add_css_class("flat");
        send_mode_btn.set_tooltip_text(Some("Send via WhatsApp (click to switch to SMS)"));
        send_mode_btn.set_visible(false);
        header.pack_end(&send_mode_btn);

        // Favourite button
        let fav_button = Button::from_icon_name("non-starred-symbolic");
        fav_button.add_css_class("flat");
        fav_button.set_tooltip_text(Some("Add to favourites"));
        header.pack_end(&fav_button);

        // Label dropdown
        let label_button = Button::from_icon_name("tag-symbolic");
        label_button.add_css_class("flat");
        label_button.set_tooltip_text(Some("Label"));
        header.pack_end(&label_button);

        let search_button = Button::from_icon_name("system-search-symbolic");
        search_button.add_css_class("flat");
        search_button.set_tooltip_text(Some("Search messages"));
        header.pack_end(&search_button);

        // Voice call button — hidden until WebRTC calling is implemented (the
        // buttons were a false affordance: click did nothing visible).
        let voice_call_btn = Button::from_icon_name("call-start-symbolic");
        voice_call_btn.add_css_class("flat");
        voice_call_btn.set_tooltip_text(Some("Voice call"));
        voice_call_btn.set_visible(false);
        header.pack_end(&voice_call_btn);

        // Video call button — hidden until WebRTC calling is implemented.
        let video_call_btn = Button::from_icon_name("camera-video-symbolic");
        video_call_btn.add_css_class("flat");
        video_call_btn.set_tooltip_text(Some("Video call"));
        video_call_btn.set_visible(false);
        header.pack_end(&video_call_btn);

        // ── Search bar (hidden until search button clicked) ──
        let search_entry = SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search messages…"));
        search_entry.set_margin_start(8);
        search_entry.set_margin_end(8);
        search_entry.set_margin_top(4);
        search_entry.set_margin_bottom(4);

        let search_revealer = Revealer::new();
        search_revealer.set_transition_type(RevealerTransitionType::SlideDown);
        search_revealer.set_child(Some(&search_entry));
        search_revealer.set_reveal_child(false);

        // ── Message area ──
        let messages_box = Box::new(Orientation::Vertical, 0);
        messages_box.set_vexpand(true);

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_child(Some(&messages_box));
        // Enable kinetic scrolling so mouse-wheel ticks animate smoothly
        // instead of jumping. Default is enabled for touch but mouse wheel
        // may default to discrete jumps; setting it explicitly fixes the
        // "jump and stop" feel reported by the user.
        scroll.set_kinetic_scrolling(true);
        scroll.set_overlay_scrolling(true);

        // "Go to latest" floating button — visible when user scrolls up
        // "Go to latest" button — floats above the scroll via Overlay
        let goto_latest_btn = Button::from_icon_name("go-down-symbolic");
        goto_latest_btn.add_css_class("goto-latest");
        goto_latest_btn.add_css_class("circular");
        goto_latest_btn.set_halign(Align::Center);
        goto_latest_btn.set_valign(Align::End);
        goto_latest_btn.set_margin_bottom(8);
        goto_latest_btn.set_size_request(36, 36);
        goto_latest_btn.set_visible(false);
        goto_latest_btn.set_tooltip_text(Some("Go to latest"));

        let scroll_overlay = gtk4::Overlay::new();
        scroll_overlay.set_child(Some(&scroll));
        scroll_overlay.add_overlay(&goto_latest_btn);
        scroll_overlay.set_vexpand(true);

        // ── Typing indicator (animated bouncing dots) ──
        let typing_box = Box::new(Orientation::Horizontal, 4);
        typing_box.set_halign(Align::Start);
        typing_box.set_margin_start(16);
        typing_box.set_margin_bottom(2);
        typing_box.set_visible(false);

        let typing_name = Label::new(None);
        typing_name.add_css_class("caption");
        typing_name.add_css_class("dim-label");
        typing_box.append(&typing_name);

        for i in 1..=3 {
            let dot = Label::new(Some("●"));
            dot.add_css_class("typing-dot");
            dot.add_css_class(&format!("typing-dot-{i}"));
            typing_box.append(&dot);
        }

        // ── Reply bar (shown when replying) ──
        let reply_bar = Box::new(Orientation::Horizontal, 8);
        reply_bar.set_margin_start(12);
        reply_bar.set_margin_end(12);
        reply_bar.set_margin_top(4);
        reply_bar.add_css_class("reply-bar");
        reply_bar.set_visible(false);

        let reply_icon = Label::new(Some("↩"));
        let reply_label = Label::new(None);
        reply_label.set_hexpand(true);
        reply_label.add_css_class("dim-label");
        reply_label.add_css_class("caption");

        let cancel_reply = Button::with_label("✕");
        cancel_reply.add_css_class("flat");

        reply_bar.append(&reply_icon);
        reply_bar.append(&reply_label);
        reply_bar.append(&cancel_reply);

        // ── Edit banner ──
        let edit_banner_box = Box::new(Orientation::Horizontal, 8);
        edit_banner_box.set_margin_start(12);
        edit_banner_box.set_margin_end(12);
        edit_banner_box.set_margin_top(4);
        edit_banner_box.add_css_class("reply-bar");
        let edit_icon = Label::new(Some("✏"));
        let edit_label = Label::new(Some("Editing message"));
        edit_label.set_hexpand(true);
        edit_label.add_css_class("dim-label");
        edit_label.add_css_class("caption");
        let cancel_edit = Button::with_label("✕");
        cancel_edit.add_css_class("flat");
        edit_banner_box.append(&edit_icon);
        edit_banner_box.append(&edit_label);
        edit_banner_box.append(&cancel_edit);

        let edit_banner = Revealer::new();
        edit_banner.set_child(Some(&edit_banner_box));
        edit_banner.set_reveal_child(false);
        edit_banner.set_transition_type(RevealerTransitionType::SlideUp);

        // ── Input bar ──
        // Outer bar with margins
        let input_bar = Box::new(Orientation::Horizontal, 0);
        input_bar.set_margin_start(8);
        input_bar.set_margin_end(8);
        input_bar.set_margin_top(4);
        input_bar.set_margin_bottom(6);

        // Single rounded container for everything: [emoji][attach][text][send]
        let input_frame = Box::new(Orientation::Horizontal, 0);
        input_frame.add_css_class("message-input-frame");
        input_frame.set_hexpand(true);

        // Emoji button
        let emoji_btn = Button::from_icon_name("face-smile-symbolic");
        emoji_btn.add_css_class("flat");
        emoji_btn.add_css_class("input-action-btn");
        emoji_btn.set_valign(Align::Center);
        emoji_btn.set_margin_start(4);
        emoji_btn.set_tooltip_text(Some("Emoji, GIF & Stickers"));

        // Attach button
        let attach_btn = Button::from_icon_name("list-add-symbolic");
        attach_btn.add_css_class("flat");
        attach_btn.add_css_class("input-action-btn");
        attach_btn.set_valign(Align::Center);
        attach_btn.set_tooltip_text(Some("Attach"));

        // Text input
        let input_view = TextView::new();
        input_view.set_hexpand(true);
        input_view.set_vexpand(false);
        input_view.set_wrap_mode(gtk4::WrapMode::WordChar);
        // Enable spell-check underlines (uses system spell checker if available)
        input_view.set_input_hints(gtk4::InputHints::SPELLCHECK);
        // Install autocorrect — fixes common typos on space/punctuation
        crate::ui::autocorrect::install_on_textview(&input_view);
        input_view.set_top_margin(17);
        input_view.set_bottom_margin(11);
        input_view.set_left_margin(6);
        input_view.set_right_margin(6);
        input_view.add_css_class("message-input");

        let input_scroll = ScrolledWindow::new();
        input_scroll.set_child(Some(&input_view));
        input_scroll.set_hexpand(true);
        input_scroll.set_vexpand(false);
        input_scroll.set_max_content_height(120);
        input_scroll.set_propagate_natural_height(true);
        input_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

        // GtkTextView has no native placeholder — overlay a dim "Type a message"
        // label over the scroll that hides as soon as the buffer has content.
        // The overlay wraps the ScrolledWindow (not the TextView) so the view
        // keeps its native scrolling; the label is click-through.
        let input_placeholder = Label::new(Some("Type a message"));
        input_placeholder.add_css_class("dim-label");
        input_placeholder.set_halign(Align::Start);
        input_placeholder.set_valign(Align::Start);
        input_placeholder.set_margin_top(17);
        input_placeholder.set_margin_start(8);
        input_placeholder.set_can_target(false);

        let input_overlay = gtk4::Overlay::new();
        input_overlay.set_child(Some(&input_scroll));
        input_overlay.add_overlay(&input_placeholder);
        input_overlay.set_hexpand(true);

        {
            let ph = input_placeholder.clone();
            input_view.buffer().connect_changed(move |buf| {
                ph.set_visible(buf.char_count() == 0);
            });
        }

        // Send button — same visual size as emoji/attach
        let send_button = Button::from_icon_name("go-up-symbolic");
        send_button.add_css_class("suggested-action");
        send_button.add_css_class("circular");
        send_button.set_size_request(42, 42);
        send_button.set_valign(Align::Center);
        send_button.set_margin_end(4);
        send_button.set_tooltip_text(Some("Send"));
        // Nothing to send on an empty compose field — start disabled. Toggled
        // on by update_send_button_state as text/attachments come and go.
        send_button.set_sensitive(false);

        // AI autocorrect spinner (hidden by default, shown during correction)
        let ai_spinner = gtk4::Spinner::new();
        ai_spinner.set_size_request(16, 16);
        ai_spinner.set_valign(Align::Center);
        ai_spinner.set_margin_end(4);
        ai_spinner.set_visible(false);

        // Autocorrect send mode toggle: "AC" button toggles delay-for-autocorrect
        let ac_delay_send = Rc::new(Cell::new(true)); // default: wait for AC
        let ac_toggle = gtk4::ToggleButton::with_label("AC");
        ac_toggle.set_active(true);
        ac_toggle.add_css_class("ac-mode-btn");
        ac_toggle.add_css_class("flat");
        ac_toggle.set_valign(Align::Center);
        ac_toggle.set_tooltip_text(Some(
            "Autocorrect mode: ON = wait for correction before sending, OFF = send immediately",
        ));
        {
            let delay = ac_delay_send.clone();
            ac_toggle.connect_toggled(move |btn| {
                delay.set(btn.is_active());
            });
        }

        // Voice note record button — toggles to stop when recording
        let mic_btn = Button::from_icon_name("audio-input-microphone-symbolic");
        mic_btn.add_css_class("flat");
        mic_btn.add_css_class("input-action-btn");
        mic_btn.set_valign(Align::Center);
        mic_btn.set_margin_end(2);
        mic_btn.set_tooltip_text(Some("Record voice note"));

        // All inside the single rounded frame
        input_frame.append(&emoji_btn);
        input_frame.append(&attach_btn);
        input_frame.append(&input_overlay);
        input_frame.append(&ai_spinner);
        input_frame.append(&ac_toggle);
        input_frame.append(&mic_btn);
        input_frame.append(&send_button);
        input_bar.append(&input_frame);

        // ── Forward bar (hidden until forward mode) ──
        let forward_bar = Box::new(Orientation::Horizontal, 8);
        forward_bar.set_margin_start(8);
        forward_bar.set_margin_end(8);
        forward_bar.set_margin_top(8);
        forward_bar.set_margin_bottom(8);
        forward_bar.set_visible(false);

        let forward_cancel_btn = Button::with_label("Cancel");
        forward_cancel_btn.add_css_class("destructive-action");
        forward_cancel_btn.add_css_class("pill");
        let forward_count_label = Label::new(Some("0 selected"));
        forward_count_label.set_hexpand(true);
        forward_count_label.set_halign(Align::Center);
        let forward_send_btn = Button::from_icon_name("mail-forward-symbolic");
        forward_send_btn.add_css_class("suggested-action");
        forward_send_btn.add_css_class("circular");
        forward_send_btn.set_size_request(42, 42);
        forward_send_btn.set_tooltip_text(Some("Forward"));

        forward_bar.append(&forward_cancel_btn);
        forward_bar.append(&forward_count_label);
        forward_bar.append(&forward_send_btn);

        // ── @ Mention popover (above input) ──
        let mention_list = ListBox::new();
        mention_list.set_selection_mode(gtk4::SelectionMode::Single);
        mention_list.add_css_class("navigation-sidebar");
        let mention_scroll = ScrolledWindow::new();
        mention_scroll.set_child(Some(&mention_list));
        mention_scroll.set_max_content_height(250);
        mention_scroll.set_propagate_natural_height(true);
        mention_scroll.set_size_request(-1, -1);
        let mention_popover = gtk4::Popover::new();
        mention_popover.set_parent(&input_view);
        mention_popover.set_child(Some(&mention_scroll));
        mention_popover.set_autohide(false);
        mention_popover.set_has_arrow(false);
        mention_popover.set_position(gtk4::PositionType::Top);

        // ── / Quick reply popover (above input) ──
        let slash_list = ListBox::new();
        slash_list.set_selection_mode(gtk4::SelectionMode::Single);
        slash_list.add_css_class("navigation-sidebar");
        let slash_scroll = ScrolledWindow::new();
        slash_scroll.set_child(Some(&slash_list));
        slash_scroll.set_max_content_height(250);
        slash_scroll.set_propagate_natural_height(true);
        slash_scroll.set_size_request(-1, -1);
        let slash_popover = gtk4::Popover::new();
        slash_popover.set_parent(&input_view);
        slash_popover.set_child(Some(&slash_scroll));
        slash_popover.set_autohide(false);
        slash_popover.set_has_arrow(false);
        slash_popover.set_position(gtk4::PositionType::Top);

        // Dismiss mention + slash popovers when the window loses focus or
        // is minimized. Without this, autohide=false popovers can float
        // above other windows as orphaned popups.
        {
            let mp = mention_popover.clone();
            let sp = slash_popover.clone();
            let iv_for_hook = input_view.clone();
            input_view.connect_realize(move |iv| {
                if let Some(root) = iv.root() {
                    if let Some(window) = root.downcast_ref::<gtk4::Window>() {
                        let mp2 = mp.clone();
                        let sp2 = sp.clone();
                        window.connect_is_active_notify(move |w| {
                            if !w.is_active() {
                                mp2.popdown();
                                sp2.popdown();
                            }
                        });
                        // Also dismiss when window gets unmapped (minimized)
                        let mp3 = mp.clone();
                        let sp3 = sp.clone();
                        window.connect_unmap(move |_| {
                            mp3.popdown();
                            sp3.popdown();
                        });
                    }
                }
                // Keep reference alive
                let _ = &iv_for_hook;
            });
        }

        // ── Emoji/GIF/Sticker popover (persistent) ──
        let emoji_popover = gtk4::Popover::new();
        emoji_popover.set_parent(&input_view);
        emoji_popover.set_position(gtk4::PositionType::Top);
        emoji_popover.set_has_arrow(false);
        emoji_popover.set_size_request(440, 400);

        let notebook = gtk4::Notebook::new();

        // Tab 1: bounded, ranked local emoji search. Weak widget references
        // keep the picker's button callbacks from retaining the chat panel.
        let input_weak = input_view.downgrade();
        let popover_weak = emoji_popover.downgrade();
        let emoji_picker = crate::ui::emoji_picker::EmojiPicker::new(move |emoji| {
            if let Some(input) = input_weak.upgrade() {
                input.buffer().insert_at_cursor(emoji);
                input.grab_focus();
            }
            if let Some(popover) = popover_weak.upgrade() {
                popover.popdown();
            }
        });
        let emoji_search = emoji_picker.search_entry().clone();
        notebook.append_page(emoji_picker.widget(), Some(&Label::new(Some("😀 Emoji"))));

        // Tab 2: GIF search
        let gif_tab = Box::new(Orientation::Vertical, 4);
        let gif_search = SearchEntry::new();
        gif_search.set_placeholder_text(Some("Search GIFs"));
        gif_search.set_margin_start(4);
        gif_search.set_margin_end(4);
        gif_search.set_margin_top(4);
        let gif_status = Label::new(Some("Powered by Tenor"));
        gif_status.add_css_class("caption");
        gif_status.add_css_class("dim-label");
        gif_status.set_halign(Align::Start);
        gif_status.set_margin_start(6);
        gif_status.set_margin_end(6);

        let gif_grid = gtk4::FlowBox::new();
        gif_grid.set_max_children_per_line(2);
        gif_grid.set_min_children_per_line(2);
        gif_grid.set_selection_mode(gtk4::SelectionMode::None);
        gif_grid.set_homogeneous(true);
        gif_grid.set_column_spacing(4);
        gif_grid.set_row_spacing(4);

        let gif_scroll = ScrolledWindow::new();
        gif_scroll.set_child(Some(&gif_grid));
        gif_scroll.set_vexpand(true);

        gif_tab.append(&gif_search);
        gif_tab.append(&gif_status);
        gif_tab.append(&gif_scroll);
        notebook.append_page(&gif_tab, Some(&Label::new(Some("GIF"))));

        // Tab 3: Sticker search
        let sticker_tab = Box::new(Orientation::Vertical, 4);
        let sticker_search = SearchEntry::new();
        sticker_search.set_placeholder_text(Some("Search stickers"));
        sticker_search.set_margin_start(4);
        sticker_search.set_margin_end(4);
        sticker_search.set_margin_top(4);
        let sticker_status = Label::new(Some("Powered by Tenor"));
        sticker_status.add_css_class("caption");
        sticker_status.add_css_class("dim-label");
        sticker_status.set_halign(Align::Start);
        sticker_status.set_margin_start(6);
        sticker_status.set_margin_end(6);

        let sticker_grid = gtk4::FlowBox::new();
        sticker_grid.set_max_children_per_line(4);
        sticker_grid.set_min_children_per_line(4);
        sticker_grid.set_selection_mode(gtk4::SelectionMode::None);
        sticker_grid.set_homogeneous(true);
        sticker_grid.set_column_spacing(4);
        sticker_grid.set_row_spacing(4);

        let sticker_scroll = ScrolledWindow::new();
        sticker_scroll.set_child(Some(&sticker_grid));
        sticker_scroll.set_vexpand(true);

        sticker_tab.append(&sticker_search);
        sticker_tab.append(&sticker_status);
        sticker_tab.append(&sticker_scroll);
        notebook.append_page(&sticker_tab, Some(&Label::new(Some("🎭 Stickers"))));

        emoji_popover.set_child(Some(&notebook));

        // Pinned message banner (between header and scroll, hidden by default)
        let pin_banner = Box::new(Orientation::Horizontal, 8);
        pin_banner.add_css_class("pin-banner");
        pin_banner.set_visible(false);

        // Keep the persistent header controls outside the animated message
        // surface. Dimming the header after its title has already changed reads
        // as a flash, while animating only the replaceable content makes the
        // switch obvious without moving the composer or its keyboard focus.
        root.append(&header);
        root.append(&search_revealer);
        root.append(&pin_banner);
        let conversation_surface = Box::new(Orientation::Vertical, 0);
        conversation_surface.set_vexpand(true);
        conversation_surface.append(&scroll_overlay);
        conversation_surface.append(&typing_box);
        root.append(&conversation_surface);
        // Image preview bar (shown when pasting an image)
        let image_preview_bar = Box::new(Orientation::Horizontal, 8);
        image_preview_bar.set_margin_start(12);
        image_preview_bar.set_margin_end(12);
        image_preview_bar.set_margin_top(4);
        image_preview_bar.set_visible(false);

        let image_preview_pic = gtk4::Picture::new();
        image_preview_pic.set_size_request(120, 120);
        image_preview_pic.set_content_fit(gtk4::ContentFit::Contain);
        image_preview_pic.set_can_shrink(true);

        let preview_label = Label::new(Some("Press Enter to send, Escape to cancel"));
        preview_label.add_css_class("dim-label");
        preview_label.add_css_class("caption");
        preview_label.set_hexpand(true);

        let cancel_preview = Button::from_icon_name("window-close-symbolic");
        cancel_preview.add_css_class("flat");

        image_preview_bar.append(&image_preview_pic);
        image_preview_bar.append(&preview_label);
        image_preview_bar.append(&cancel_preview);

        root.append(&reply_bar);
        root.append(&edit_banner);
        root.append(&image_preview_bar);
        // Disabled until a chat is opened — no chat means Send/typing/attach
        // would silently do nothing (open_chat re-enables it).
        input_bar.set_sensitive(false);
        root.append(&input_bar);
        root.append(&forward_bar);

        let inner = Rc::new(ChatViewInner {
            root,
            conversation_surface,
            chat_switch_animation: RefCell::new(None),
            messages_box,
            scroll,
            input_view,
            send_button,
            typing_last_true_ms: std::cell::Cell::new(0),
            typing_stop_source: RefCell::new(None),
            typing_target: RefCell::new(None),
            suppress_typing: Cell::new(false),
            typing_box,
            typing_name,
            header_name,
            header_subtitle,
            send_mode_btn: send_mode_btn.clone(),
            favorite_button: fav_button.clone(),
            favorite_chats: RefCell::new(HashSet::new()),
            pin_banner,
            bridge,
            current_chat_id: RefCell::new(None),
            own_jid: RefCell::new(None),
            own_name: RefCell::new("Me".to_string()),
            lid_to_phone: RefCell::new(crate::ui::runtime::load_lid_phone_map()),
            all_typers: RefCell::new(HashMap::new()),
            scroll_pending: Rc::new(Cell::new(0)),
            at_bottom: Rc::new(Cell::new(true)),
            goto_latest_btn: goto_latest_btn.clone(),
            bubbles: RefCell::new(HashMap::new()),
            pending_message_jump: RefCell::new(None),
            search_texts: RefCell::new(HashMap::new()),
            id_remap: RefCell::new(HashMap::new()),
            media_items: RefCell::new(Vec::new()),
            last_msg_date: RefCell::new(None),
            reply_context: RefCell::new(None),
            reply_bar,
            reply_label,
            search_revealer,
            search_entry,
            forward_selected: RefCell::new(Vec::new()),
            forward_mode: RefCell::new(false),
            forward_bar,
            forward_count_label,
            input_bar,
            group_members: RefCell::new(Vec::new()),
            mention_popover,
            mention_list,
            pending_mentions: RefCell::new(Vec::new()),
            slash_popover,
            slash_list,
            quick_replies: RefCell::new(crate::ui::quick_replies::load()),
            pending_image_path: RefCell::new(None),
            pending_gif_url: RefCell::new(None),
            pending_gif_preview_url: RefCell::new(None),
            image_preview_bar,
            image_preview_pic,
            preview_label: preview_label.clone(),
            pending_attachments: RefCell::new(HashMap::new()),
            on_profile_open: RefCell::new(None),
            emoji_popover,
            gif_grid,
            sticker_grid,
            gif_status,
            sticker_status,
            gif_search_request: Cell::new(0),
            sticker_search_request: Cell::new(0),
            picker_page: Cell::new(0),
            gif_result_cache: RefCell::new(None),
            sticker_result_cache: RefCell::new(None),
            gif_rendered_request: Cell::new(0),
            sticker_rendered_request: Cell::new(0),
            editing_msg: RefCell::new(None),
            edit_banner,
            ai_spinner,
            ac_delay_send,
            drafts: RefCell::new(HashMap::new()),
            send_group_ids: RefCell::new(None),
            starred_msgs: RefCell::new(HashSet::new()),
        });

        // Poll AI autocorrect in-flight status to toggle the spinner.
        // 500ms is plenty — spinner is purely cosmetic feedback, no need to
        // wake the main loop 10×/sec just to watch a flag.
        {
            let spinner = inner.ai_spinner.clone();
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                let correcting = crate::ui::autocorrect::is_correcting();
                if correcting && !spinner.is_visible() {
                    spinner.set_visible(true);
                    spinner.start();
                } else if !correcting && spinner.is_visible() {
                    spinner.stop();
                    spinner.set_visible(false);
                }
                gtk4::glib::ControlFlow::Continue
            });
        }

        // Detect user scroll via EventControllerScroll (mouse wheel / touchpad).
        // This is more reliable than vadjustment signals for tracking user intent.
        {
            let at_bottom = inner.at_bottom.clone();
            let btn = inner.goto_latest_btn.clone();
            let scroll_ref = inner.scroll.clone();
            let sc = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
            sc.connect_scroll(move |_, _, _| {
                // User is actively scrolling — update at_bottom and chevron
                let adj = scroll_ref.vadjustment();
                let near = adj.value() >= adj.upper() - adj.page_size() - 60.0;
                at_bottom.set(near);
                btn.set_visible(!near && adj.upper() > adj.page_size());
                glib::Propagation::Proceed
            });
            inner.scroll.add_controller(sc);
        }

        // When content height changes (layout/image load/prepend), re-anchor
        // to the bottom in two cases:
        //   1. Force mode: scroll_pending > 0 (used by chat switch / send to
        //      override even an out-of-position scroll).
        //   2. Idle mode: at_bottom is true (the user hasn't scrolled away).
        //      This is the durable lock — it keeps us pinned to the latest
        //      message through long sequences of prepends or texture-load
        //      resizes without needing to guess a pulse count up front.
        // User-initiated scroll updates at_bottom via EventControllerScroll
        // above, so scrolling up cleanly disables the auto-snap.
        {
            let sp = inner.scroll_pending.clone();
            let at_b = inner.at_bottom.clone();
            let adj = inner.scroll.vadjustment();
            adj.connect_changed(move |a| {
                let count = sp.get();
                if count > 0 {
                    a.set_value(a.upper() - a.page_size());
                    sp.set(count - 1);
                } else if at_b.get() {
                    a.set_value(a.upper() - a.page_size());
                }
            });
        }

        // "Go to latest" button click — animate the scroll
        {
            let scroll_c = inner.scroll.clone();
            let sp = inner.scroll_pending.clone();
            let at_b = inner.at_bottom.clone();
            goto_latest_btn.connect_clicked(move |btn| {
                let adj = scroll_c.vadjustment();
                let target = adj.upper() - adj.page_size();
                let start = adj.value();
                let distance = target - start;

                // Only proceed if there's actually distance to scroll
                if distance.abs() < 1.0 {
                    return;
                }

                // Set state AFTER confirming we will actually scroll
                sp.set(0);
                btn.set_visible(false);
                at_b.set(true);

                // Animate over ~300ms in 20 steps (ease-out cubic)
                let adj_c = adj.clone();
                let step = std::cell::Cell::new(0u32);
                let at_b2 = at_b.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(15), move || {
                    let i = step.get() + 1;
                    step.set(i);
                    let t = (i as f64 / 20.0).min(1.0);
                    let eased = 1.0 - (1.0 - t).powi(3);
                    adj_c.set_value(start + distance * eased);
                    if i >= 20 {
                        adj_c.set_value(target);
                        at_b2.set(true); // Ensure at_bottom is true when animation ends
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
            });
        }

        // Send-mode toggle: cycle WhatsApp ↔ SMS for the current chat.
        {
            let inner_w = Rc::downgrade(&inner);
            send_mode_btn.connect_clicked(move |_btn| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                let Some(cid) = inner_c.current_chat_id.borrow().clone() else {
                    return;
                };
                let current = crate::bridge::send_mode::get(&cid)
                    .unwrap_or(crate::bridge::send_mode::Mode::WhatsApp);
                let next = match current {
                    crate::bridge::send_mode::Mode::WhatsApp => crate::bridge::send_mode::Mode::Sms,
                    crate::bridge::send_mode::Mode::Sms => crate::bridge::send_mode::Mode::WhatsApp,
                };
                crate::bridge::send_mode::set(&cid, next);
                ChatViewPanel::apply_send_mode_btn(&inner_c, &cid);
            });
        }

        // Search button toggles the search bar
        {
            let inner_clone = inner.clone();
            // Favourite button
            {
                let inner_c = inner.clone();
                fav_button.connect_clicked(move |_| {
                    if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                        let new_fav = !inner_c.favorite_chats.borrow().contains(&cid);
                        if new_fav {
                            inner_c.favorite_chats.borrow_mut().insert(cid.clone());
                        } else {
                            inner_c.favorite_chats.borrow_mut().remove(&cid);
                        }
                        inner_c.bridge.send_command(WaCommand::FavoriteChat {
                            chat_id: cid,
                            favorite: new_fav,
                        });
                        ChatViewPanel::apply_favorite_button(&inner_c, new_fav);
                    }
                });
            }

            // Label dropdown
            {
                let inner_c = inner.clone();
                label_button.connect_clicked(move |btn| {
                    let popover = gtk4::Popover::new();
                    popover.set_parent(btn);
                    popover.set_has_arrow(true);
                    let vbox = Box::new(Orientation::Vertical, 0);
                    vbox.set_width_request(200);
                    // WhatsApp Business default labels with their colours
                    let labels: &[(&str, &str)] = &[
                        ("New customer", "#64b5f6"),
                        ("New order", "#66bb6a"),
                        ("Pending payment", "#fdd835"),
                        ("Paid", "#ef5350"),
                        ("Order complete", "#ff7043"),
                        ("Work", "#7e57c2"),
                        ("Personal", "#26c6da"),
                        ("Important", "#ec407a"),
                        ("None", "#8696a0"),
                    ];
                    for (label, colour) in labels {
                        let item = Button::new();
                        item.add_css_class("flat");
                        item.set_has_frame(false);
                        let hbox = Box::new(Orientation::Horizontal, 8);
                        hbox.set_margin_start(8);
                        hbox.set_margin_end(8);
                        hbox.set_margin_top(4);
                        hbox.set_margin_bottom(4);
                        let dot = Label::new(Some("●"));
                        dot.set_markup(&format!("<span foreground='{colour}'>●</span>"));
                        let name = Label::new(Some(label));
                        name.set_halign(Align::Start);
                        hbox.append(&dot);
                        hbox.append(&name);
                        item.set_child(Some(&hbox));
                        let inner_cc = inner_c.clone();
                        let pop = popover.clone();
                        let label_str = if *label == "None" {
                            None
                        } else {
                            Some(label.to_string())
                        };
                        let label_btn_ref = btn.clone();
                        item.connect_clicked(move |_| {
                            if let Some(cid) = inner_cc.current_chat_id.borrow().clone() {
                                inner_cc.bridge.send_command(WaCommand::LabelChat {
                                    chat_id: cid,
                                    label: label_str.clone(),
                                });
                            }
                            // Update label icon colour
                            if label_str.is_some() {
                                label_btn_ref.set_tooltip_text(Some(&format!(
                                    "Label: {}",
                                    label_str.as_deref().unwrap_or("")
                                )));
                            }
                            pop.popdown();
                        });
                        vbox.append(&item);
                    }
                    popover.set_child(Some(&vbox));
                    popover.popup();
                });
            }

            search_button.connect_clicked(move |_| {
                let visible = inner_clone.search_revealer.reveals_child();
                inner_clone.search_revealer.set_reveal_child(!visible);
                if !visible {
                    inner_clone.search_entry.grab_focus();
                } else {
                    // Clear filter when hiding
                    inner_clone.search_entry.set_text("");
                    Self::apply_search_filter(&inner_clone, "");
                }
            });
        }

        // Voice call button
        {
            let inner_c = inner.clone();
            voice_call_btn.connect_clicked(move |_| {
                if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                    inner_c.bridge.send_command(WaCommand::InitiateCall {
                        chat_id: cid,
                        is_video: false,
                    });
                }
            });
        }

        // Video call button
        {
            let inner_c = inner.clone();
            video_call_btn.connect_clicked(move |_| {
                if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                    inner_c.bridge.send_command(WaCommand::InitiateCall {
                        chat_id: cid,
                        is_video: true,
                    });
                }
            });
        }

        // Voice note recording (press-and-hold to record, release to review)
        {
            let recording: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            let record_path: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
            let record_start: Rc<Cell<u64>> = Rc::new(Cell::new(0));
            let record_child: Rc<RefCell<Option<std::process::Child>>> =
                Rc::new(RefCell::new(None));
            let inner_c = inner.clone();

            // ── Helper: stop recording + show review bar ──
            let stop_and_review = {
                let rec = recording.clone();
                let rp = record_path.clone();
                let rs = record_start.clone();
                let rc = record_child.clone();
                let inner_c = inner_c.clone();
                let mic_btn_c = mic_btn.clone();
                Rc::new(move || {
                    if !rec.get() {
                        return;
                    }
                    rec.set(false);
                    mic_btn_c.set_icon_name("audio-input-microphone-symbolic");
                    mic_btn_c.set_tooltip_text(Some("Hold to record voice note"));

                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let dur = (now - rs.get()) as u32;
                    let dur = dur.max(1);

                    // Stop ffmpeg
                    if let Some(mut child) = rc.borrow_mut().take() {
                        unsafe {
                            libc::kill(child.id() as i32, libc::SIGINT);
                        }
                        let _ = child.wait();
                    }

                    if let (Some(cid), Some(path)) = (
                        inner_c.current_chat_id.borrow().clone(),
                        rp.borrow().clone(),
                    ) {
                        // Show review bar: play preview, send, or discard
                        let review = Box::new(Orientation::Horizontal, 8);
                        review.set_margin_start(8);
                        review.set_margin_end(8);
                        review.set_margin_top(6);
                        review.set_margin_bottom(6);

                        let dur_lbl = Label::new(Some(&format!("🎤 {dur}s")));
                        dur_lbl.add_css_class("heading");

                        let play_btn = Button::from_icon_name("media-playback-start-symbolic");
                        play_btn.add_css_class("flat");
                        play_btn.set_tooltip_text(Some("Preview"));
                        let path_play = path.clone();
                        // Preview inline via gtk4::MediaFile (same audio backend
                        // the video bubbles use) instead of shelling out to
                        // xdg-open, which stole focus to an external player and
                        // failed silently. Play/pause toggles on repeat clicks.
                        let media_slot: Rc<RefCell<Option<gtk4::MediaFile>>> =
                            Rc::new(RefCell::new(None));
                        play_btn.connect_clicked(move |btn| {
                            let mut slot = media_slot.borrow_mut();
                            if let Some(mf) = slot.as_ref() {
                                // Toggle pause/resume on an already-loaded clip.
                                if mf.is_playing() {
                                    mf.pause();
                                    btn.set_icon_name("media-playback-start-symbolic");
                                } else {
                                    // If playback already ran to the end, rewind
                                    // first — a MediaFile parked at EOS won't
                                    // replay on play() without seeking to 0.
                                    if mf.is_ended() {
                                        mf.seek(0);
                                    }
                                    mf.play();
                                    btn.set_icon_name("media-playback-pause-symbolic");
                                }
                                return;
                            }
                            let mf = gtk4::MediaFile::for_filename(&path_play);
                            // Reset the icon when playback finishes.
                            let btn_end = btn.clone();
                            mf.connect_ended_notify(move |m| {
                                if m.is_ended() {
                                    btn_end.set_icon_name("media-playback-start-symbolic");
                                }
                            });
                            mf.play();
                            btn.set_icon_name("media-playback-pause-symbolic");
                            *slot = Some(mf);
                        });

                        let send_btn = Button::from_icon_name("go-up-symbolic");
                        send_btn.add_css_class("suggested-action");
                        send_btn.add_css_class("circular");
                        send_btn.set_tooltip_text(Some("Send voice note"));

                        let discard_btn = Button::from_icon_name("edit-delete-symbolic");
                        discard_btn.add_css_class("flat");
                        discard_btn.set_tooltip_text(Some("Discard"));

                        review.append(&dur_lbl);
                        review.append(&play_btn);
                        let spacer = Box::new(Orientation::Horizontal, 0);
                        spacer.set_hexpand(true);
                        review.append(&spacer);
                        review.append(&discard_btn);
                        review.append(&send_btn);

                        // Insert review bar above the input
                        inner_c
                            .input_bar
                            .insert_child_after(&review, None::<&gtk4::Widget>);

                        // Discard — remove review bar and delete file
                        let review_d = review.clone();
                        let input_bar_d = inner_c.input_bar.clone();
                        let path_d = path.clone();
                        discard_btn.connect_clicked(move |_| {
                            input_bar_d.remove(&review_d);
                            let _ = std::fs::remove_file(&path_d);
                        });

                        // Send — create optimistic bubble and dispatch
                        let review_s = review.clone();
                        let input_bar_s = inner_c.input_bar.clone();
                        let bridge = inner_c.bridge.clone();
                        let inner_c2 = inner_c.clone();
                        send_btn.connect_clicked(move |_| {
                            input_bar_s.remove(&review_s);

                            // Small delay for file flush
                            let bridge = bridge.clone();
                            let cid = cid.clone();
                            let path = path.clone();
                            let inner_c3 = inner_c2.clone();
                            gtk4::glib::timeout_add_local_once(
                                std::time::Duration::from_millis(100),
                                move || match std::fs::metadata(&path) {
                                    Ok(meta) if meta.len() > 0 => {
                                        let tmp_id = format!(
                                            "vn_{}",
                                            std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_millis()
                                        );
                                        let now_ts = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs()
                                            as i64;
                                        let mut vn_msg = crate::bridge::IncomingMessage::outgoing(
                                            tmp_id.clone(),
                                            cid.clone(),
                                            None,
                                            now_ts,
                                        );
                                        vn_msg.media_type = Some(crate::bridge::MediaType::Audio);
                                        vn_msg.media_local_path = Some(path.clone());
                                        ChatViewPanel::append_bubble_to_inner(&inner_c3, vn_msg);
                                        ChatViewPanel::force_scroll_to_bottom(&inner_c3, 3);

                                        bridge.send_command(WaCommand::SendAudio {
                                            chat_id: ChatViewPanel::resolve_send_target(&cid),
                                            path,
                                            duration_secs: dur,
                                            is_voice_note: true,
                                            tmp_id,
                                        });
                                    }
                                    _ => log::warn!("Voice note file missing or empty"),
                                },
                            );
                        });
                    }
                })
            };

            // Start recording helper
            let start_recording = {
                let rec = recording.clone();
                let rp = record_path.clone();
                let rs = record_start.clone();
                let rc = record_child.clone();
                let mic_btn_c = mic_btn.clone();
                Rc::new(move || {
                    if rec.get() {
                        return;
                    }
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let path = format!("/tmp/wa_voice_{ts}.ogg");
                    *rp.borrow_mut() = Some(path.clone());
                    rs.set(ts);
                    rec.set(true);
                    mic_btn_c.set_icon_name("media-record-symbolic");
                    mic_btn_c.set_tooltip_text(Some("Recording... release to review"));

                    // Read audio input device from settings
                    let audio_input = {
                        let s = crate::ui::settings::AppSettings::load();
                        if s.audio_input.is_empty() {
                            "default".to_string()
                        } else {
                            s.audio_input
                        }
                    };
                    match std::process::Command::new("ffmpeg")
                        .args([
                            "-y",
                            "-f",
                            "pulse",
                            "-i",
                            &audio_input,
                            "-ac",
                            "1",
                            "-c:a",
                            "libopus",
                            "-b:a",
                            "32k",
                            "-ar",
                            "48000",
                            "-application",
                            "voip",
                            &path,
                        ])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(child) => {
                            *rc.borrow_mut() = Some(child);
                        }
                        Err(e) => {
                            log::warn!("Failed to start recording: {e}");
                            rec.set(false);
                            mic_btn_c.set_icon_name("audio-input-microphone-symbolic");
                        }
                    }
                })
            };

            // Press-and-hold: press starts recording, release stops and shows review
            let press_gesture = gtk4::GestureLongPress::new();
            // GTK requires a delay factor in the 0.5..=2.0 range. Using the
            // minimum keeps press-and-hold responsive without runtime warnings.
            press_gesture.set_delay_factor(0.5);
            let sr = start_recording.clone();
            press_gesture.connect_pressed(move |_, _, _| {
                (sr)();
            });
            mic_btn.add_controller(press_gesture);

            // Release → stop recording and show review
            let release_ctl = gtk4::GestureClick::new();
            release_ctl.set_button(1);
            let rec_for_release = recording.clone();
            let sar = stop_and_review.clone();
            release_ctl.connect_released(move |_, _, _, _| {
                if rec_for_release.get() {
                    (sar)();
                }
            });
            mic_btn.add_controller(release_ctl);

            // Also support click to toggle for accessibility
            let sr2 = start_recording.clone();
            let sar2 = stop_and_review.clone();
            let rec_for_click = recording.clone();
            mic_btn.connect_clicked(move |_| {
                if rec_for_click.get() {
                    (sar2)();
                } else {
                    (sr2)();
                }
            });
        }

        // Live search filtering
        {
            let inner_clone = inner.clone();
            inner.search_entry.connect_search_changed(move |entry| {
                let query = entry.text().to_lowercase();
                Self::apply_search_filter(&inner_clone, &query);
            });
        }

        // Wire send button
        {
            let inner_clone = inner.clone();
            inner.send_button.connect_clicked(move |_| {
                Self::do_send(&inner_clone);
            });
        }

        // Wire Enter=send, Shift+Enter or Alt+Enter=newline
        {
            let inner_clone = inner.clone();
            let key_ctrl = gtk4::EventControllerKey::new();
            let iv = inner.input_view.clone();
            key_ctrl.connect_key_pressed(move |_, key, _, modifier| {
                // If mention or slash popover is visible, intercept Enter/Up/Down/Escape
                let mention_visible = inner_clone.mention_popover.is_visible();
                let slash_visible = inner_clone.slash_popover.is_visible();
                let popover_visible = mention_visible || slash_visible;

                if popover_visible {
                    let list = if mention_visible {
                        &inner_clone.mention_list
                    } else {
                        &inner_clone.slash_list
                    };
                    match key {
                        gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::Tab => {
                            // Activate the selected (or first) row
                            if let Some(row) = list.selected_row().or_else(|| list.row_at_index(0))
                            {
                                row.activate();
                            }
                            return gtk4::glib::Propagation::Stop;
                        }
                        gtk4::gdk::Key::Escape => {
                            // Dismiss only the popover — no longer also cancels a
                            // staged attachment (that was an overloaded Escape).
                            inner_clone.mention_popover.popdown();
                            inner_clone.slash_popover.popdown();
                            return gtk4::glib::Propagation::Stop;
                        }
                        gtk4::gdk::Key::Down => {
                            // Move selection down
                            let cur = list.selected_row().map(|r| r.index()).unwrap_or(-1);
                            if let Some(next) = list.row_at_index(cur + 1) {
                                list.select_row(Some(&next));
                            }
                            return gtk4::glib::Propagation::Stop;
                        }
                        gtk4::gdk::Key::Up => {
                            let cur = list.selected_row().map(|r| r.index()).unwrap_or(1);
                            if cur > 0 {
                                if let Some(prev) = list.row_at_index(cur - 1) {
                                    list.select_row(Some(&prev));
                                }
                            }
                            return gtk4::glib::Propagation::Stop;
                        }
                        _ => {}
                    }
                }

                // Escape when no mention/slash popover is up: first close the
                // emoji/GIF/sticker popover if it's open (GTK's default only
                // closes it when the popover itself holds focus, not while focus
                // stays in the input); otherwise cancel a staged attachment
                // preview. These are now separate so one Escape never does both.
                if key == gtk4::gdk::Key::Escape {
                    if inner_clone.emoji_popover.is_visible() {
                        inner_clone.emoji_popover.popdown();
                        return gtk4::glib::Propagation::Stop;
                    }
                    if inner_clone.pending_image_path.borrow().is_some()
                        || inner_clone.pending_gif_url.borrow().is_some()
                    {
                        *inner_clone.pending_image_path.borrow_mut() = None;
                        *inner_clone.pending_gif_url.borrow_mut() = None;
                        *inner_clone.pending_gif_preview_url.borrow_mut() = None;
                        inner_clone.image_preview_bar.set_visible(false);
                        if let Some(cid) = inner_clone.current_chat_id.borrow().clone() {
                            inner_clone.pending_attachments.borrow_mut().remove(&cid);
                        }
                        update_send_button_state(&inner_clone);
                        return gtk4::glib::Propagation::Stop;
                    }
                }

                // Ctrl+A: select all text
                if key == gtk4::gdk::Key::a
                    && modifier.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                {
                    let buf = iv.buffer();
                    buf.select_range(&buf.start_iter(), &buf.end_iter());
                    return gtk4::glib::Propagation::Stop;
                }

                if key == gtk4::gdk::Key::Return || key == gtk4::gdk::Key::KP_Enter {
                    let has_modifier = modifier.contains(gtk4::gdk::ModifierType::SHIFT_MASK)
                        || modifier.contains(gtk4::gdk::ModifierType::ALT_MASK)
                        || modifier.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                    if has_modifier {
                        // Shift+Enter / Alt+Enter / Ctrl+Enter → new line
                        iv.buffer().insert_at_cursor("\n");
                        gtk4::glib::Propagation::Stop
                    } else {
                        Self::do_send(&inner_clone);
                        gtk4::glib::Propagation::Stop
                    }
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            });
            inner.input_view.add_controller(key_ctrl);
        }

        // ── Ctrl+V paste handler ──
        // We OWN Ctrl+V on the input. GTK4's native clipboard read is
        // broken under COSMIC — the app is a native Wayland client, but
        // the GTK4 ↔ cosmic-comp `wl_data_device` path drops reads (both
        // text and images, not transiently — it just doesn't work
        // reliably). `wl-paste` from wl-clipboard talks the
        // focus-independent `wlr-data-control` protocol instead and
        // reads the COSMIC clipboard correctly, so we shell out to it.
        // If wl-clipboard isn't installed we fall back to the GTK
        // native read (paste_clipboard_text / _image).
        {
            let inner_c = inner.clone();
            let iv_paste = inner.input_view.clone();
            let paste_ctrl = gtk4::EventControllerKey::new();
            paste_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
            paste_ctrl.connect_key_pressed(move |_, key, _, modifier| {
                if !(key == gtk4::gdk::Key::v
                    && modifier.contains(gtk4::gdk::ModifierType::CONTROL_MASK))
                {
                    return gtk4::glib::Propagation::Proceed;
                }
                // Leave Ctrl+V alone when a text Entry/Search field has focus —
                // its focused inner widget is a GtkText, so those keep their
                // normal text paste. The message input is a GtkTextView (not a
                // GtkText), so it — and any non-editable focus target — still
                // gets the image paste below.
                let entry_focused = inner_c
                    .root
                    .root()
                    .and_then(|r| r.downcast::<gtk4::Window>().ok())
                    .and_then(|w| gtk4::prelude::GtkWindowExt::focus(&w))
                    .map(|f| f.is::<gtk4::Text>())
                    .unwrap_or(false);
                if entry_focused {
                    return gtk4::glib::Propagation::Proceed;
                }
                log::info!("Paste: Ctrl+V — reading clipboard via wl-paste");
                paste_via_wl_clipboard(iv_paste.clone(), inner_c.clone());
                // We handled it — never let the (broken) native paste run.
                gtk4::glib::Propagation::Stop
            });
            // Attach to the chat-view ROOT (capture phase), not just the message
            // input. KDE frequently doesn't keep the input focused when you
            // switch to the app or copy from another app, so a handler bound to
            // the input alone never saw Ctrl+V (paste silently did nothing).
            // From the root, capture phase sees the keypress for any focused
            // descendant of the chat view, so paste-into-message works wherever
            // focus happens to be.
            inner.root.add_controller(paste_ctrl);
        }

        // ── Buffer changed: trigger @ mention and / quick reply popovers ──
        {
            let inner_c = inner.clone();
            inner.input_view.buffer().connect_changed(move |buf| {
                let cursor = buf.iter_at_mark(&buf.get_insert());
                let text = buf.text(&buf.start_iter(), &cursor, false).to_string();

                // Toggle the send button: active only when there's something to
                // send (non-empty text or a staged image/GIF attachment).
                update_send_button_state(&inner_c);

                // ── Outbound "typing…" indicator (throttled true + idle false) ──
                {
                    let full = buf.text(&buf.start_iter(), &buf.end_iter(), false);
                    // Skip programmatic set_text (draft/edit restore, event creator):
                    // those aren't real keystrokes and must not broadcast "typing…".
                    if inner_c.suppress_typing.get() {
                        // fall through to popover detection below
                    } else if !full.trim().is_empty()
                        && let Some(cid) = inner_c.current_chat_id.borrow().clone()
                        // Send groups have no real recipient JID — dispatching
                        // SetTyping with the virtual "sendgroup::Name" id sends a
                        // malformed JID to the bridge every 3s. Skip typing there.
                        && inner_c.send_group_ids.borrow().is_none()
                        && !cid.starts_with("sendgroup::")
                    {
                        let routed = ChatViewPanel::resolve_send_target(&cid);
                        // Monotonic clock: immune to suspend / NTP wall-clock jumps
                        // that could otherwise wedge the throttle. Microseconds → ms.
                        let now_ms = gtk4::glib::monotonic_time() / 1000;
                        // Send typing=true at most once every 3s.
                        if now_ms - inner_c.typing_last_true_ms.get() > 3000 {
                            inner_c.typing_last_true_ms.set(now_ms);
                            *inner_c.typing_target.borrow_mut() = Some(routed.clone());
                            inner_c.bridge.send_command(WaCommand::SetTyping {
                                chat_id: routed.clone(),
                                is_typing: true,
                            });
                        }
                        // Rearm the idle-stop timer (typing=false after 4s quiet).
                        if let Some(src) = inner_c.typing_stop_source.borrow_mut().take() {
                            src.remove();
                        }
                        let inner_t = inner_c.clone();
                        let src = gtk4::glib::timeout_add_local_once(
                            std::time::Duration::from_secs(4),
                            move || {
                                inner_t.typing_last_true_ms.set(0);
                                inner_t.bridge.send_command(WaCommand::SetTyping {
                                    chat_id: routed.clone(),
                                    is_typing: false,
                                });
                                *inner_t.typing_stop_source.borrow_mut() = None;
                                *inner_t.typing_target.borrow_mut() = None;
                            },
                        );
                        *inner_c.typing_stop_source.borrow_mut() = Some(src);
                    }
                }

                // --- @ Mention detection ---
                if let Some(at_pos) = text.rfind('@') {
                    // Only trigger if @ is at start or preceded by whitespace
                    let before_at = if at_pos > 0 {
                        text.as_bytes().get(at_pos - 1).copied()
                    } else {
                        Some(b' ')
                    };
                    if before_at == Some(b' ') || before_at == Some(b'\n') || at_pos == 0 {
                        let query = text[at_pos + 1..].to_lowercase();
                        let members = inner_c.group_members.borrow();
                        if !members.is_empty() {
                            let matches: Vec<_> = members
                                .iter()
                                .filter(|m| {
                                    m.name.to_lowercase().contains(&query) || query.is_empty()
                                })
                                .collect();
                            if !matches.is_empty() {
                                // Populate mention list with avatars
                                let list = &inner_c.mention_list;
                                while let Some(child) = list.first_child() {
                                    list.remove(&child);
                                }
                                for m in &matches {
                                    let row = gtk4::ListBoxRow::new();
                                    let hbox = Box::new(Orientation::Horizontal, 10);
                                    hbox.set_margin_start(10);
                                    hbox.set_margin_end(10);
                                    hbox.set_margin_top(6);
                                    hbox.set_margin_bottom(6);

                                    let av = libadwaita::Avatar::new(32, Some(&m.name), true);
                                    // Try loading cached avatar
                                    let safe = m.jid.replace(['/', '\\', '@', ':'], "_");
                                    let av_path = std::path::PathBuf::from("wa_avatars")
                                        .join(format!("{safe}.jpg"));
                                    if av_path.exists() {
                                        if let Some(tex) =
                                            crate::ui::texture_cache::texture_thumbnail(
                                                &av_path, 64,
                                            )
                                        {
                                            av.set_custom_image(Some(&tex));
                                        }
                                    }

                                    let lbl = Label::new(Some(&m.name));
                                    lbl.set_halign(Align::Start);
                                    lbl.set_hexpand(true);

                                    hbox.append(&av);
                                    hbox.append(&lbl);
                                    row.set_child(Some(&hbox));
                                    row.set_widget_name(&m.jid);
                                    list.append(&row);
                                }
                                // Match parent input width
                                let w = inner_c.input_view.width();
                                if w > 100 {
                                    inner_c.mention_popover.set_size_request(w, -1);
                                }
                                inner_c.mention_popover.popup();
                                inner_c.slash_popover.popdown();
                                return;
                            }
                        }
                    }
                }
                inner_c.mention_popover.popdown();

                // --- / Quick reply detection ---
                let full_text = buf
                    .text(&buf.start_iter(), &buf.end_iter(), false)
                    .to_string();
                if full_text.starts_with('/') && full_text.len() >= 1 {
                    let query = full_text[1..].to_lowercase();
                    let replies = inner_c.quick_replies.borrow();
                    let matches: Vec<_> = replies
                        .iter()
                        .filter(|r| {
                            r.shortcut.to_lowercase().starts_with(&query) || query.is_empty()
                        })
                        .collect();
                    if !matches.is_empty() {
                        let list = &inner_c.slash_list;
                        while let Some(child) = list.first_child() {
                            list.remove(&child);
                        }
                        for r in &matches {
                            let row = gtk4::ListBoxRow::new();
                            let vbox = Box::new(Orientation::Vertical, 2);
                            vbox.set_margin_start(12);
                            vbox.set_margin_end(12);
                            vbox.set_margin_top(6);
                            vbox.set_margin_bottom(6);
                            let shortcut = Label::new(Some(&format!("/{}", r.shortcut)));
                            shortcut.add_css_class("heading");
                            shortcut.set_halign(Align::Start);
                            let preview = Label::new(Some(&r.text));
                            preview.add_css_class("dim-label");
                            preview.set_halign(Align::Start);
                            preview.set_hexpand(true);
                            preview.set_wrap(true);
                            preview.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                            preview.set_lines(3);
                            preview.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                            vbox.append(&shortcut);
                            vbox.append(&preview);
                            row.set_child(Some(&vbox));
                            row.set_widget_name(&r.text);
                            list.append(&row);
                        }
                        let w = inner_c.input_view.width();
                        if w > 100 {
                            inner_c.slash_popover.set_size_request(w, -1);
                        }
                        inner_c.slash_popover.popup();
                        return;
                    }
                }
                inner_c.slash_popover.popdown();
            });
        }

        // Wire mention list row activation
        {
            let inner_c = inner.clone();
            inner.mention_list.connect_row_activated(move |_, row| {
                let jid = row.widget_name().to_string();
                // Get name from the Label inside the hbox (avatar is first child, label is last)
                let name = row
                    .child()
                    .and_then(|hbox| hbox.last_child())
                    .and_then(|c| c.downcast::<Label>().ok())
                    .map(|l| l.text().to_string())
                    .unwrap_or_else(|| crate::ui::runtime::display_name_from_jid(&jid));
                // Replace @query with @Name in the buffer
                let buf = inner_c.input_view.buffer();
                let cursor = buf.iter_at_mark(&buf.get_insert());
                let text_before = buf.text(&buf.start_iter(), &cursor, false).to_string();
                if let Some(at_pos) = text_before.rfind('@') {
                    let mut start = buf.iter_at_offset(at_pos as i32);
                    let mut end = buf.iter_at_mark(&buf.get_insert());
                    buf.delete(&mut start, &mut end);
                    buf.insert(&mut start, &format!("@{name} "));
                }
                // Store both JID and display name for send-time replacement
                inner_c
                    .pending_mentions
                    .borrow_mut()
                    .push(format!("{jid}\t{name}"));
                inner_c.mention_popover.popdown();
                inner_c.input_view.grab_focus();
            });
        }

        // Wire slash list row activation
        {
            let inner_c = inner.clone();
            inner.slash_list.connect_row_activated(move |_, row| {
                let text = row.widget_name().to_string();
                let buf = inner_c.input_view.buffer();
                buf.set_text(&text);
                inner_c.slash_popover.popdown();
                inner_c.input_view.grab_focus();
            });
        }

        // Cancel reply
        {
            let inner_clone = inner.clone();
            cancel_reply.connect_clicked(move |_| {
                *inner_clone.reply_context.borrow_mut() = None;
                inner_clone.reply_bar.set_visible(false);
            });
        }

        // Cancel edit
        {
            let inner_clone = inner.clone();
            cancel_edit.connect_clicked(move |_| {
                *inner_clone.editing_msg.borrow_mut() = None;
                inner_clone.edit_banner.set_reveal_child(false);
                inner_clone.input_view.buffer().set_text("");
            });
        }

        // ── Drag and drop files (single or multiple) ──
        {
            let inner_c = inner.clone();
            // Accept both single File and FileList for multi-file drops.
            // Some file managers (Files, Dolphin) advertise FileList for
            // both single and multi drops, others (Thunar in some configs)
            // only advertise gio::File for single drops. set_types accepts
            // both — without it, a single-file drop from certain file
            // managers would silently fail.
            let drop_target = gtk4::DropTarget::new(
                gtk4::gdk::FileList::static_type(),
                gtk4::gdk::DragAction::COPY,
            );
            drop_target.set_types(&[
                gtk4::gdk::FileList::static_type(),
                gtk4::gio::File::static_type(),
            ]);
            drop_target.connect_drop(move |_, value, _, _| {
                let mut paths: Vec<String> = Vec::new();

                // Try FileList first (multi-file drop)
                if let Ok(file_list) = value.get::<gtk4::gdk::FileList>() {
                    for file in file_list.files() {
                        if let Some(path) = file.path() {
                            paths.push(path.to_string_lossy().to_string());
                        }
                    }
                }
                // Fallback: single File
                if paths.is_empty() {
                    if let Ok(file) = value.get::<gtk4::gio::File>() {
                        if let Some(path) = file.path() {
                            paths.push(path.to_string_lossy().to_string());
                        }
                    }
                }

                if paths.is_empty() {
                    return false;
                }

                if paths.len() == 1 {
                    set_pending_attachment(&inner_c, &paths[0]);
                } else {
                    // Multiple files: send each one immediately. The runtime
                    // side of SendImage detects file extension and routes
                    // documents through the document_message path with the
                    // correct mime type — so SendImage works for non-images
                    // here despite the name.
                    if let Some(chat_id) = inner_c.current_chat_id.borrow().clone() {
                        for path_str in &paths {
                            let tmp_id = gen_tmp_id();
                            inner_c.bridge.send_command(WaCommand::SendImage {
                                chat_id: ChatViewPanel::resolve_send_target(&chat_id),
                                path: path_str.clone(),
                                caption: None,
                                tmp_id,
                            });
                        }
                    }
                }
                true
            });
            inner.root.add_controller(drop_target);
        }

        // Cancel image preview
        {
            let inner_clone = inner.clone();
            cancel_preview.connect_clicked(move |_| {
                *inner_clone.pending_image_path.borrow_mut() = None;
                *inner_clone.pending_gif_url.borrow_mut() = None;
                *inner_clone.pending_gif_preview_url.borrow_mut() = None;
                inner_clone.image_preview_bar.set_visible(false);
                update_send_button_state(&inner_clone);
                // Clear stale state so the next attachment starts fresh.
                inner_clone
                    .image_preview_pic
                    .set_paintable(None::<&gtk4::gdk::Paintable>);
                inner_clone
                    .preview_label
                    .set_markup("Press Enter to send, Escape to cancel");
                // Drop the per-chat saved record so the cancellation
                // sticks across chat switches (otherwise switching out
                // and back would re-stage what the user just cancelled).
                if let Some(cid) = inner_clone.current_chat_id.borrow().clone() {
                    inner_clone.pending_attachments.borrow_mut().remove(&cid);
                }
            });
        }

        // + Attachment menu
        {
            let inner_c = inner.clone();
            attach_btn.connect_clicked(move |btn| {
                let popover = gtk4::Popover::new();
                popover.set_parent(btn);
                popover.set_position(gtk4::PositionType::Top);
                popover.set_has_arrow(false);

                let vbox = Box::new(Orientation::Vertical, 0);
                vbox.set_width_request(200);

                let items = [
                    ("📷", "Photo & Video"),
                    ("🎵", "Audio"),
                    ("📄", "Document"),
                    ("📊", "Poll"),
                    ("📅", "Event"),
                ];

                for (icon, label) in &items {
                    let row = Button::new();
                    row.set_has_frame(false);
                    row.add_css_class("flat");
                    let hbox = Box::new(Orientation::Horizontal, 8);
                    hbox.set_margin_start(8);
                    hbox.set_margin_end(8);
                    hbox.set_margin_top(6);
                    hbox.set_margin_bottom(6);
                    let icon_lbl = Label::new(Some(icon));
                    let text_lbl = Label::new(Some(label));
                    text_lbl.set_halign(Align::Start);
                    hbox.append(&icon_lbl);
                    hbox.append(&text_lbl);
                    row.set_child(Some(&hbox));

                    let inner_cc = inner_c.clone();
                    let label_str = label.to_string();
                    let pop = popover.clone();
                    row.connect_clicked(move |_| {
                        pop.popdown();
                        if label_str == "Poll" {
                            show_poll_creator(&inner_cc);
                        } else if label_str == "Event" {
                            show_event_creator(&inner_cc);
                        } else if label_str == "Audio" {
                            // Pick an existing audio file → SendAudio (not a
                            // voice note). Distinct from the hold-to-record mic,
                            // which was the only audio path before.
                            let dialog = gtk4::FileDialog::new();
                            let filter = gtk4::FileFilter::new();
                            filter.add_mime_type("audio/*");
                            filter.set_name(Some("Audio"));
                            let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
                            filters.append(&filter);
                            dialog.set_filters(Some(&filters));
                            let inner_ccc = inner_cc.clone();
                            let win = inner_cc
                                .root
                                .root()
                                .and_then(|r| r.downcast::<gtk4::Window>().ok());
                            dialog.open(
                                win.as_ref(),
                                None::<&gtk4::gio::Cancellable>,
                                move |result| {
                                    if let (Ok(file), Some(chat_id)) =
                                        (result, inner_ccc.current_chat_id.borrow().clone())
                                    {
                                        if let Some(path) = file.path() {
                                            inner_ccc.bridge.send_command(WaCommand::SendAudio {
                                                chat_id: ChatViewPanel::resolve_send_target(
                                                    &chat_id,
                                                ),
                                                path: path.to_string_lossy().to_string(),
                                                duration_secs: 0,
                                                is_voice_note: false,
                                                tmp_id: gen_tmp_id(),
                                            });
                                        }
                                    }
                                },
                            );
                        } else if label_str == "Photo & Video" || label_str == "Document" {
                            // Open file chooser
                            let dialog = gtk4::FileDialog::new();
                            if label_str == "Photo & Video" {
                                let filter = gtk4::FileFilter::new();
                                filter.add_mime_type("image/*");
                                filter.add_mime_type("video/*");
                                filter.set_name(Some("Photos & Videos"));
                                let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
                                filters.append(&filter);
                                dialog.set_filters(Some(&filters));
                            } else {
                                // Document: broad non-image/video filter so the
                                // chooser doesn't silently accept an image as a
                                // "document" (which would then ride the image
                                // send path). "All files" plus common doc types.
                                let doc_filter = gtk4::FileFilter::new();
                                doc_filter.add_mime_type("application/*");
                                doc_filter.add_mime_type("text/*");
                                doc_filter.set_name(Some("Documents"));
                                let all_filter = gtk4::FileFilter::new();
                                all_filter.add_pattern("*");
                                all_filter.set_name(Some("All files"));
                                let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
                                filters.append(&doc_filter);
                                filters.append(&all_filter);
                                dialog.set_filters(Some(&filters));
                                dialog.set_default_filter(Some(&doc_filter));
                            }
                            let inner_ccc = inner_cc.clone();
                            let win = inner_cc
                                .root
                                .root()
                                .and_then(|r| r.downcast::<gtk4::Window>().ok());
                            dialog.open(
                                win.as_ref(),
                                None::<&gtk4::gio::Cancellable>,
                                move |result| {
                                    if let Ok(file) = result {
                                        if let Some(path) = file.path() {
                                            let path_str = path.to_string_lossy().to_string();
                                            set_pending_attachment(&inner_ccc, &path_str);
                                        }
                                    }
                                },
                            );
                        }
                    });
                    vbox.append(&row);
                }

                popover.set_child(Some(&vbox));
                popover.popup();
            });
        }

        // Emoji/GIF/Sticker panel — built once, toggled on click
        {
            let inner_w = Rc::downgrade(&inner);
            let emoji_search = emoji_search.clone();
            let gif_search = gif_search.clone();
            let sticker_search = sticker_search.clone();
            let notebook = notebook.clone();
            emoji_btn.connect_clicked(move |_| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                if inner_c.emoji_popover.is_visible() {
                    inner_c.emoji_popover.popdown();
                } else {
                    inner_c.emoji_popover.popup();
                    let page = notebook.current_page().unwrap_or(0);
                    inner_c.picker_page.set(page);
                    match page {
                        1 => {
                            Self::maybe_render_picker_results(&inner_c, PickerSearchKind::Gif);
                            gif_search.grab_focus();
                        }
                        2 => {
                            Self::maybe_render_picker_results(&inner_c, PickerSearchKind::Sticker);
                            sticker_search.grab_focus();
                        }
                        _ => {
                            emoji_search.grab_focus();
                        }
                    };
                }
            });
        }

        // Wire GIF search
        {
            let inner_w = Rc::downgrade(&inner);
            gif_search.connect_search_changed(move |entry| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                queue_picker_search(
                    &inner_c,
                    PickerSearchKind::Gif,
                    entry.text().to_string(),
                    true,
                );
            });
        }

        // Wire sticker search
        {
            let inner_w = Rc::downgrade(&inner);
            sticker_search.connect_search_changed(move |entry| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                queue_picker_search(
                    &inner_c,
                    PickerSearchKind::Sticker,
                    entry.text().to_string(),
                    true,
                );
            });
        }

        // GIFs and stickers are network-backed. Load each tab only when it is
        // first viewed; opening the default emoji tab should do no hidden work.
        {
            let inner_w = Rc::downgrade(&inner);
            let gif_search_c = gif_search.clone();
            let sticker_search_c = sticker_search.clone();
            notebook.connect_switch_page(move |_, _, page| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                inner_c.picker_page.set(page);
                match page {
                    1 => {
                        if inner_c.gif_search_request.get() == 0 {
                            queue_picker_search(
                                &inner_c,
                                PickerSearchKind::Gif,
                                String::new(),
                                false,
                            );
                        }
                        Self::maybe_render_picker_results(&inner_c, PickerSearchKind::Gif);
                        gif_search_c.grab_focus();
                    }
                    2 => {
                        if inner_c.sticker_search_request.get() == 0 {
                            queue_picker_search(
                                &inner_c,
                                PickerSearchKind::Sticker,
                                String::new(),
                                false,
                            );
                        }
                        Self::maybe_render_picker_results(&inner_c, PickerSearchKind::Sticker);
                        sticker_search_c.grab_focus();
                    }
                    _ => {}
                }
            });
        }

        // Forward bar: cancel
        {
            let inner_clone = inner.clone();
            forward_cancel_btn.connect_clicked(move |_| {
                Self::exit_forward_mode(&inner_clone);
            });
        }

        // Forward bar: send (opens chat picker)
        {
            let inner_clone = inner.clone();
            forward_send_btn.connect_clicked(move |_| {
                inner_clone.bridge.send_command(WaCommand::GetChatList);
            });
        }

        ChatViewPanel { inner }
    }

    fn enter_forward_mode(inner: &Rc<ChatViewInner>, initial_msg_id: &str) {
        *inner.forward_mode.borrow_mut() = true;
        inner.forward_selected.borrow_mut().clear();
        inner
            .forward_selected
            .borrow_mut()
            .push(initial_msg_id.to_string());
        inner.input_bar.set_visible(false);
        inner.forward_bar.set_visible(true);
        inner.forward_count_label.set_text("1 selected");
        inner.messages_box.set_cursor_from_name(Some("pointer"));
        inner.messages_box.add_css_class("forward-mode");
        // Visually mark the initial message
        if let Some(bubble) = inner.bubbles.borrow().get(initial_msg_id) {
            let w = bubble.widget();
            w.set_opacity(0.75);
            w.set_margin_start(w.margin_start() + 20);
            w.set_margin_end(w.margin_end() + 20);
            w.set_margin_top(w.margin_top() + 2);
            w.set_margin_bottom(w.margin_bottom() + 2);
            w.add_css_class("forward-selected");
        }
    }

    fn exit_forward_mode(inner: &Rc<ChatViewInner>) {
        // Restore visuals on all selected messages
        let selected = inner.forward_selected.borrow().clone();
        for msg_id in &selected {
            if let Some(bubble) = inner.bubbles.borrow().get(msg_id.as_str()) {
                let w = bubble.widget();
                w.set_opacity(1.0);
                w.set_margin_start((w.margin_start() - 20).max(0));
                w.set_margin_end((w.margin_end() - 20).max(0));
                w.set_margin_top((w.margin_top() - 2).max(0));
                w.set_margin_bottom((w.margin_bottom() - 2).max(0));
                w.remove_css_class("forward-selected");
            }
        }
        *inner.forward_mode.borrow_mut() = false;
        inner.forward_selected.borrow_mut().clear();
        inner.forward_bar.set_visible(false);
        inner.input_bar.set_visible(true);
        inner.messages_box.set_cursor(None);
        inner.messages_box.remove_css_class("forward-mode");
    }

    fn toggle_forward_select(inner: &Rc<ChatViewInner>, msg_id: &str) {
        let mut selected = inner.forward_selected.borrow_mut();
        let is_now_selected;
        if let Some(pos) = selected.iter().position(|id| id == msg_id) {
            selected.remove(pos);
            is_now_selected = false;
        } else {
            selected.push(msg_id.to_string());
            is_now_selected = true;
        }
        let count = selected.len();
        inner
            .forward_count_label
            .set_text(&format!("{count} selected"));
        drop(selected);

        // Visual feedback: darken + shrink when selected, restore when deselected
        if let Some(bubble) = inner.bubbles.borrow().get(msg_id) {
            let w = bubble.widget();
            if is_now_selected {
                w.set_opacity(0.75);
                // Scale down by applying CSS transform via margin
                w.set_margin_start(w.margin_start() + 20);
                w.set_margin_end(w.margin_end() + 20);
                w.set_margin_top(w.margin_top() + 2);
                w.set_margin_bottom(w.margin_bottom() + 2);
                w.add_css_class("forward-selected");
            } else {
                w.set_opacity(1.0);
                w.set_margin_start((w.margin_start() - 20).max(0));
                w.set_margin_end((w.margin_end() - 20).max(0));
                w.set_margin_top((w.margin_top() - 2).max(0));
                w.set_margin_bottom((w.margin_bottom() - 2).max(0));
                w.remove_css_class("forward-selected");
            }
        }
    }

    fn apply_search_filter(inner: &Rc<ChatViewInner>, query: &str) {
        let bubbles = inner.bubbles.borrow();
        let texts = inner.search_texts.borrow();
        for (msg_id, bubble) in bubbles.iter() {
            let visible = if query.is_empty() {
                true
            } else {
                texts
                    .get(msg_id)
                    .map(|t| t.to_lowercase().contains(query))
                    .unwrap_or(false)
            };
            bubble.widget().set_visible(visible);
        }
    }

    /// Set callback for avatar clicks that should open profiles.
    pub fn connect_profile_open(&self, cb: impl Fn(String, String) + 'static) {
        *self.inner.on_profile_open.borrow_mut() = Some(std::boxed::Box::new(cb));
    }

    /// Connect a callback triggered when the user clicks the chat header name.
    pub fn connect_header_click(&self, callback: impl Fn(String, String) + 'static) {
        let inner = self.inner.clone();
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(1);
        gesture.connect_released(move |_, _, _, _| {
            let chat_id = inner.current_chat_id.borrow().clone().unwrap_or_default();
            let name = inner.header_name.text().to_string();
            if !chat_id.is_empty() {
                callback(chat_id, name);
            }
        });
        self.inner.header_name.add_controller(gesture);
        self.inner.header_name.set_cursor_from_name(Some("pointer"));
    }

    /// Send a plain text message to the current chat WITHOUT touching the
    /// composer buffer, editing state, pending attachment, reply context or
    /// mentions. Builds its own optimistic bubble and routes through the same
    /// SendText / MultiSend paths do_send uses for a plain message. Used by the
    /// event creator so composing an event can't hijack an in-progress edit,
    /// caption a staged image, or destroy the user's draft.
    fn send_plain_text(inner: &Rc<ChatViewInner>, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let chat_id = match inner.current_chat_id.borrow().clone() {
            Some(id) => id,
            None => return,
        };
        let tmp_id = gen_tmp_id();
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let optimistic = IncomingMessage {
            id: tmp_id.clone(),
            chat_id: chat_id.clone(),
            sender_id: String::new(),
            sender_name: String::new(),
            text: Some(text.clone()),
            media_type: None,
            timestamp: now_ts,
            is_from_me: true,
            quoted_msg_id: None,
            quoted_text: None,
            quoted_sender: None,
            quoted_media_path: None,
            poll_question: None,
            poll_options: vec![],
            poll_selectable: 0,
            poll_secret: vec![],
            poll_votes: vec![],
            is_forwarded: false,
            forwarding_score: 0,
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
            receipt_status: crate::bridge::ReceiptStatus::Pending,
            is_edited: false,
            is_system_message: false,
            media_download: None,
        };
        Self::append_bubble_to_inner(inner, optimistic);
        Self::force_scroll_to_bottom(inner, 5);

        if let Some(group_ids) = inner.send_group_ids.borrow().clone() {
            let group_name = chat_id
                .strip_prefix("sendgroup::")
                .unwrap_or(&chat_id)
                .to_string();
            save_send_group_message(&group_name, &text);
            inner.bridge.send_command(WaCommand::MultiSend {
                chat_ids: group_ids,
                text,
            });
        } else {
            inner.bridge.send_command(WaCommand::SendText {
                chat_id: ChatViewPanel::resolve_send_target(&chat_id),
                text,
                tmp_id,
                mentioned_jids: vec![],
            });
        }
    }

    fn append_optimistic_picker_media(
        inner: &Rc<ChatViewInner>,
        chat_id: &str,
        tmp_id: &str,
        media_type: crate::bridge::MediaType,
    ) {
        let optimistic = IncomingMessage {
            id: tmp_id.to_string(),
            chat_id: chat_id.to_string(),
            sender_id: String::new(),
            sender_name: String::new(),
            text: None,
            media_type: Some(media_type),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            is_from_me: true,
            quoted_msg_id: None,
            quoted_text: None,
            quoted_sender: None,
            quoted_media_path: None,
            poll_question: None,
            poll_options: Vec::new(),
            poll_selectable: 0,
            poll_secret: Vec::new(),
            poll_votes: Vec::new(),
            is_forwarded: false,
            forwarding_score: 0,
            reactions: Vec::new(),
            media_local_path: None,
            media_filename: None,
            media_caption: None,
            contact_name: None,
            contact_vcard: None,
            link_title: None,
            link_description: None,
            link_url: None,
            link_thumbnail_path: None,
            receipt_status: crate::bridge::ReceiptStatus::Pending,
            is_edited: false,
            is_system_message: false,
            media_download: None,
        };
        Self::append_bubble_to_inner(inner, optimistic);
        Self::force_scroll_to_bottom(inner, 5);
    }

    fn do_send(inner: &Rc<ChatViewInner>) {
        let chat_id = match inner.current_chat_id.borrow().clone() {
            Some(id) => id,
            None => return,
        };

        // ── Image / GIF / Edit ──

        let pending_image = inner.pending_image_path.borrow_mut().take();
        if let Some(image_path) = pending_image {
            // Sending ends the typing session — flush SetTyping{false} to the
            // routed target so the recipient doesn't linger on "typing…".
            Self::cancel_typing(inner, true);
            inner.image_preview_bar.set_visible(false);
            // Drop the per-chat saved record too — once sent, the chat
            // shouldn't retain it for next-time-opened.
            inner.pending_attachments.borrow_mut().remove(&chat_id);
            let buf = inner.input_view.buffer();
            let caption_text = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .to_string();
            buf.set_text("");
            let tmp_id = gen_tmp_id();

            // If AC delay is ON and there's a caption, correct it first
            if inner.ac_delay_send.get() && !caption_text.trim().is_empty() {
                let bridge = inner.bridge.clone();
                let chat_id = chat_id.clone();
                crate::ui::autocorrect::correct_for_send(caption_text, move |corrected| {
                    let caption = if corrected.trim().is_empty() {
                        None
                    } else {
                        Some(corrected)
                    };
                    bridge.send_command(WaCommand::SendImage {
                        chat_id: ChatViewPanel::resolve_send_target(&chat_id),
                        path: image_path,
                        caption,
                        tmp_id,
                    });
                });
            } else {
                let caption = if caption_text.trim().is_empty() {
                    None
                } else {
                    Some(caption_text)
                };
                inner.bridge.send_command(WaCommand::SendImage {
                    chat_id: ChatViewPanel::resolve_send_target(&chat_id),
                    path: image_path,
                    caption,
                    tmp_id,
                });
            }
            return;
        }

        let pending_gif = inner.pending_gif_url.borrow_mut().take();
        if let Some(mp4_url) = pending_gif {
            *inner.pending_gif_preview_url.borrow_mut() = None;
            Self::cancel_typing(inner, true);
            inner.image_preview_bar.set_visible(false);
            inner.pending_attachments.borrow_mut().remove(&chat_id);
            let tmp_id = gen_tmp_id();
            Self::append_optimistic_picker_media(
                inner,
                &chat_id,
                &tmp_id,
                crate::bridge::MediaType::Gif,
            );
            inner.bridge.send_command(WaCommand::SendGif {
                chat_id: ChatViewPanel::resolve_send_target(&chat_id),
                mp4_url,
                tmp_id,
            });
            return;
        }

        // ── Text message path ──

        let buf = inner.input_view.buffer();
        let text = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        if text.trim().is_empty() {
            return;
        }

        // We are committing to a send — flush any outbound "typing…" now.
        Self::cancel_typing(inner, true);

        // Edit an existing message — send immediately, no AC delay
        let editing = inner.editing_msg.borrow_mut().take();
        if let Some((edit_chat_id, edit_msg_id)) = editing {
            buf.set_text("");
            inner.edit_banner.set_reveal_child(false);
            // Resolve tmp→real at SEND time. This also covers the case where the
            // send confirmation landed after the user opened the editor but before
            // they hit save (editing_msg would still hold the "tmp-…" id).
            let edit_msg_id = inner
                .id_remap
                .borrow()
                .get(&edit_msg_id)
                .cloned()
                .unwrap_or(edit_msg_id);
            inner.bridge.send_command(WaCommand::EditMessage {
                chat_id: edit_chat_id,
                msg_id: edit_msg_id,
                new_text: text,
            });
            return;
        }

        // Clear input immediately so it feels instant to the user
        buf.set_text("");

        let tmp_id = gen_tmp_id();
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let reply = inner.reply_context.borrow().clone();

        // Show optimistic bubble immediately (user sees their message right away)
        let optimistic = IncomingMessage {
            id: tmp_id.clone(),
            chat_id: chat_id.clone(),
            sender_id: String::new(),
            sender_name: String::new(),
            text: Some(text.clone()),
            media_type: None,
            timestamp: now_ts,
            is_from_me: true,
            quoted_msg_id: reply.as_ref().map(|(id, _, _, _)| id.clone()),
            quoted_text: reply.as_ref().map(|(_, _, t, _)| t.clone()),
            quoted_sender: reply.as_ref().map(|(_, sender, _, _)| sender.clone()),
            quoted_media_path: reply.as_ref().and_then(|(_, _, _, mp)| mp.clone()),
            poll_question: None,
            poll_options: vec![],
            poll_selectable: 0,
            poll_secret: vec![],
            poll_votes: vec![],
            is_forwarded: false,
            forwarding_score: 0,
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
            receipt_status: crate::bridge::ReceiptStatus::Pending,
            is_edited: false,
            is_system_message: false,
            media_download: None,
        };
        Self::append_bubble_to_inner(inner, optimistic);

        // Collect @mention data (jid\tname pairs → jid list + name→number mapping).
        // Reconcile against the final message text: only keep a mention whose
        // literal "@Name" is still present, so deleting an @mention before send
        // no longer pings that person. Replacements are applied longest-name-
        // first with word-boundary matching to avoid substring corruption when
        // one name is a prefix of another (or the literal appears elsewhere).
        let raw_mentions: Vec<String> = inner.pending_mentions.borrow_mut().drain(..).collect();
        let mut mentioned_jids: Vec<String> = Vec::new();
        let mut mention_replacements: Vec<(String, String)> = Vec::new(); // (@Name, @Number)
        for entry in &raw_mentions {
            if let Some((jid, name)) = entry.split_once('\t') {
                let name_pat = format!("@{name}");
                // Drop the mention if the user deleted its "@Name" from the text.
                // Word-boundary aware (same rule as replace_mention_literal) so a
                // deleted "@Ann" doesn't linger just because "@Anna" remains.
                if !mention_literal_present(&text, &name_pat) {
                    continue;
                }
                mentioned_jids.push(jid.to_string());
                let jid_number = jid.split('@').next().unwrap_or(jid);
                mention_replacements.push((name_pat, format!("@{jid_number}")));
            } else {
                mentioned_jids.push(entry.clone());
            }
        }
        // Longest literal first so a shorter name that is a prefix of a longer
        // one doesn't consume the longer one's "@Name" mid-string.
        mention_replacements.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        // Dismiss popovers
        inner.mention_popover.popdown();
        inner.slash_popover.popdown();

        // Capture reply info and clear the reply bar
        let reply_info = if let Some((quoted_msg_id, quoted_sender, _, _)) = reply {
            *inner.reply_context.borrow_mut() = None;
            inner.reply_bar.set_visible(false);
            Some((quoted_msg_id, quoted_sender))
        } else {
            None
        };

        Self::force_scroll_to_bottom(inner, 5);

        // Shared flag — send_network sets false to kill the bounce timer
        let bounce_active: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // Snapshot send group state — if Some, we'll dispatch MultiSend instead of SendText
        let send_group_snapshot = inner.send_group_ids.borrow().clone();

        // Closure that applies mention replacement and sends the network command.
        // Called immediately (AC off) or after AI correction returns (AC on).
        let bridge = inner.bridge.clone();
        let bubbles = inner.bubbles.clone();
        let original_text = text.clone();
        let tmp_id_for_pilafy = tmp_id.clone();
        let bounce_stop = bounce_active.clone();
        let send_network = move |final_text: String| {
            // Apply @Name → @Number replacement for the WA protocol.
            // Word-boundary-aware (only replaces "@Name" when the char after the
            // literal is not part of a longer name) and applied longest-first, so
            // substring names can't corrupt one another.
            let mut send_text = final_text.clone();
            for (name_pat, number_pat) in &mention_replacements {
                send_text = replace_mention_literal(&send_text, name_pat, number_pat);
            }

            // Stop bounce and settle into place
            bounce_stop.set(false);
            if let Some(bubble) = bubbles.borrow().get(&tmp_id) {
                let w = bubble.widget();
                w.remove_css_class("pilafy");
                w.add_css_class("pilafy-settle");
                // Update text if AI changed it
                if final_text != original_text {
                    log::info!(
                        "AC corrected message: {:?} → {:?}",
                        original_text,
                        final_text
                    );
                    bubble.update_text(&final_text, false);
                }
                // Remove settle class after animation completes
                let w2 = w.clone();
                gtk4::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(500),
                    move || {
                        w2.remove_css_class("pilafy-settle");
                    },
                );
            }

            if let Some(group_ids) = &send_group_snapshot {
                // Send group mode: dispatch to all member chats + save history
                let group_name = chat_id
                    .strip_prefix("sendgroup::")
                    .unwrap_or(&chat_id)
                    .to_string();
                save_send_group_message(&group_name, &send_text);
                bridge.send_command(WaCommand::MultiSend {
                    chat_ids: group_ids.clone(),
                    text: send_text,
                });
            } else if let Some((quoted_msg_id, quoted_sender)) = reply_info {
                let routed = ChatViewPanel::resolve_send_target(&chat_id);
                bridge.send_command(WaCommand::SendReply {
                    chat_id: routed,
                    text: send_text,
                    quoted_msg_id,
                    quoted_sender,
                    tmp_id,
                    mentioned_jids,
                });
            } else {
                let routed = ChatViewPanel::resolve_send_target(&chat_id);
                bridge.send_command(WaCommand::SendText {
                    chat_id: routed,
                    text: send_text,
                    tmp_id,
                    mentioned_jids,
                });
            }
        };

        if inner.ac_delay_send.get() {
            // AC delay ON — submit to AI, send corrected text when it returns.
            // The user sees their message bubble bouncing (pilafy), and the network
            // send is deferred until the AI finishes (max ~4s timeout).
            if let Some(bubble) = inner.bubbles.borrow().get(&tmp_id_for_pilafy) {
                bubble.widget().add_css_class("pilafy");
                log::info!("AC: applied pilafy class to bubble {}", tmp_id_for_pilafy);
            } else {
                log::warn!("AC: tmp bubble {} not found in HashMap", tmp_id_for_pilafy);
            }

            // Pulse animation is on the pilafy class itself — already added above.
            bounce_active.set(true);

            log::info!("AC delay: deferring send until AI correction completes");

            // Ensure the bounce is visible for at least 600ms even if AI returns fast.
            // Without this, a fast AI response would kill the bounce on the first frame.
            let min_bounce_ms = 600u64;
            let bounce_start = std::time::Instant::now();
            let original_for_guard = text.clone();
            let send_with_min_bounce = move |final_text: String| {
                // Guard: if AI returned less than half the original, it truncated — use original
                let safe_text = if final_text.len() * 2 < original_for_guard.len() {
                    log::warn!(
                        "AC guard: AI output shorter than original ({} < {}), using original",
                        final_text.len(),
                        original_for_guard.len()
                    );
                    original_for_guard
                } else {
                    final_text
                };
                let elapsed = bounce_start.elapsed().as_millis() as u64;
                if elapsed >= min_bounce_ms {
                    send_network(safe_text);
                } else {
                    let remaining = min_bounce_ms - elapsed;
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(remaining),
                        move || {
                            send_network(safe_text);
                        },
                    );
                }
            };
            crate::ui::autocorrect::correct_for_send(text, send_with_min_bounce);
        } else {
            // AC delay OFF — quick pill→bubble settle animation, then send
            if let Some(bubble) = inner.bubbles.borrow().get(&tmp_id_for_pilafy) {
                bubble.widget().add_css_class("pilafy");
            }
            // Brief pill flash then settle into normal bubble shape
            let bubbles_settle = inner.bubbles.clone();
            let tid = tmp_id_for_pilafy.clone();
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(bubble) = bubbles_settle.borrow().get(&tid) {
                    let w = bubble.widget();
                    w.remove_css_class("pilafy");
                    w.add_css_class("pilafy-settle");
                    let w2 = w.clone();
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(500),
                        move || {
                            w2.remove_css_class("pilafy-settle");
                        },
                    );
                }
            });
            send_network(text);
        }
    }

    pub fn widget(&self) -> &Box {
        &self.inner.root
    }

    /// Set the user's own JID and display name for avatar loading on sent messages.
    pub fn set_own_jid(&self, jid: String) {
        *self.inner.own_jid.borrow_mut() = Some(jid);
    }

    pub fn set_own_name(&self, name: String) {
        if !name.is_empty() {
            *self.inner.own_name.borrow_mut() = name;
        }
    }

    /// Returns the currently open chat ID, if any.
    pub fn current_chat_id(&self) -> Option<String> {
        self.inner.current_chat_id.borrow().clone()
    }

    /// Fade the selected conversation into place without changing its
    /// allocation. This is deliberately started *after* synchronous history
    /// row construction: starting it in `open_chat` meant the main thread was
    /// blocked for most or all of the old 190ms duration, so GTK had no frames
    /// to paint and the animation appeared to be missing.
    fn start_chat_switch_animation(inner: &ChatViewInner) {
        Self::settle_chat_switch_animation(inner);

        let animations_enabled = gtk4::Settings::default()
            .map(|settings| settings.is_gtk_enable_animations())
            .unwrap_or(true);
        if !animations_enabled {
            inner.conversation_surface.set_opacity(1.0);
            return;
        }

        // A longer, higher-contrast ease is visible even on a high-refresh
        // display, while remaining short enough that rapid navigation feels
        // immediate. This is paint-only: no message rows are remeasured.
        const DURATION_MS: u32 = 280;
        const START_OPACITY: f64 = 0.28;

        inner.conversation_surface.set_opacity(START_OPACITY);
        let target = adw::PropertyAnimationTarget::new(&inner.conversation_surface, "opacity");
        let animation = adw::TimedAnimation::new(
            &inner.conversation_surface,
            START_OPACITY,
            1.0,
            DURATION_MS,
            target,
        );
        animation.set_easing(adw::Easing::EaseOutCubic);
        animation.play();
        *inner.chat_switch_animation.borrow_mut() = Some(animation);
    }

    /// Finish a transition before replacing its content. In particular, a
    /// rapid A -> B -> C sequence must not leave B's in-flight opacity value on
    /// C's loading state while its history is being assembled.
    fn settle_chat_switch_animation(inner: &ChatViewInner) {
        if let Some(active) = inner.chat_switch_animation.borrow_mut().take() {
            active.skip();
        }
        inner.conversation_surface.set_opacity(1.0);
    }

    fn apply_favorite_button(inner: &ChatViewInner, is_favorite: bool) {
        if is_favorite {
            inner.favorite_button.set_icon_name("starred-symbolic");
            inner
                .favorite_button
                .set_tooltip_text(Some("Remove from favourites"));
            inner.favorite_button.add_css_class("favorite-active");
        } else {
            inner.favorite_button.set_icon_name("non-starred-symbolic");
            inner
                .favorite_button
                .set_tooltip_text(Some("Add to favourites"));
            inner.favorite_button.remove_css_class("favorite-active");
        }
    }

    /// Update the authoritative favourite state for a chat. Callers may seed
    /// this from chat-list/history data at any time; the header is refreshed
    /// immediately when the affected chat is open.
    pub fn set_chat_favorite(&self, chat_id: &str, is_favorite: bool) {
        if is_favorite {
            self.inner
                .favorite_chats
                .borrow_mut()
                .insert(chat_id.to_string());
        } else {
            self.inner.favorite_chats.borrow_mut().remove(chat_id);
        }
        if self.inner.current_chat_id.borrow().as_deref() == Some(chat_id) {
            Self::apply_favorite_button(&self.inner, is_favorite);
        }
    }

    /// Update the send-mode toggle button's icon, tooltip and visibility
    /// based on whether `chat_id` is a merged chat (contact has both
    /// WhatsApp and SMS) and what the user's current preference is.
    /// Hidden for chats that don't have a paired protocol.
    fn apply_send_mode_btn(inner: &Rc<ChatViewInner>, chat_id: &str) {
        // Determine if this chat is merged (has a sibling on the OTHER
        // protocol). If gm: chat → look for whatsapp; else look for gm.
        let is_gm_chat = chat_id.starts_with("gm:");
        let target_source = if is_gm_chat { "whatsapp" } else { "gmessages" };
        let has_pair = crate::contacts::global()
            .other_chat_id(chat_id, target_source)
            .is_some();
        if !has_pair {
            // Single-protocol chat — no choice to make.
            inner.send_mode_btn.set_visible(false);
            return;
        }
        let mode = crate::bridge::send_mode::get(chat_id).unwrap_or(
            // For a gm: row, the natural default is SMS (you opened the
            // SMS row, you probably want SMS); for a wa-jid row, default
            // is WhatsApp. The user can flip it for either.
            if is_gm_chat {
                crate::bridge::send_mode::Mode::Sms
            } else {
                crate::bridge::send_mode::Mode::WhatsApp
            },
        );
        inner.send_mode_btn.set_visible(true);
        match mode {
            crate::bridge::send_mode::Mode::WhatsApp => {
                inner.send_mode_btn.set_icon_name("user-available-symbolic");
                inner
                    .send_mode_btn
                    .set_tooltip_text(Some("Sending via WhatsApp — click to switch to SMS"));
                inner.send_mode_btn.remove_css_class("send-mode-sms");
                inner.send_mode_btn.add_css_class("send-mode-wa");
            }
            crate::bridge::send_mode::Mode::Sms => {
                inner.send_mode_btn.set_icon_name("phone-symbolic");
                inner
                    .send_mode_btn
                    .set_tooltip_text(Some("Sending via SMS — click to switch to WhatsApp"));
                inner.send_mode_btn.remove_css_class("send-mode-wa");
                inner.send_mode_btn.add_css_class("send-mode-sms");
            }
        }
    }

    /// For the currently-open chat, decide which chat_id to actually send
    /// to based on the user's send-mode preference. For merged chats this
    /// can flip between the WhatsApp JID and the gm: chat_id.
    fn resolve_send_target(chat_id: &str) -> String {
        let is_gm = chat_id.starts_with("gm:");
        let target_source = if is_gm { "whatsapp" } else { "gmessages" };
        let pair = crate::contacts::global().other_chat_id(chat_id, target_source);
        let Some(pair) = pair else {
            // Not merged — send to the chat as-is.
            return chat_id.to_string();
        };
        let mode = crate::bridge::send_mode::get(chat_id).unwrap_or(if is_gm {
            crate::bridge::send_mode::Mode::Sms
        } else {
            crate::bridge::send_mode::Mode::WhatsApp
        });
        match mode {
            crate::bridge::send_mode::Mode::WhatsApp if is_gm => pair, // gm row, send via WA
            crate::bridge::send_mode::Mode::Sms if !is_gm => pair,     // wa row, send via SMS
            _ => chat_id.to_string(),
        }
    }

    /// Tear down the outbound typing-indicator state. Removes any armed
    /// idle-stop timer, resets the throttle, and (when `flush`) sends a final
    /// SetTyping{false} to the chat we last announced typing for — using the
    /// stored routed target, NOT current_chat_id, so a chat switch / send
    /// flushes the OLD chat correctly instead of stranding it on "typing…".
    fn cancel_typing(inner: &Rc<ChatViewInner>, flush: bool) {
        if let Some(src) = inner.typing_stop_source.borrow_mut().take() {
            src.remove();
        }
        inner.typing_last_true_ms.set(0);
        let target = inner.typing_target.borrow_mut().take();
        if flush && let Some(routed) = target {
            inner.bridge.send_command(WaCommand::SetTyping {
                chat_id: routed,
                is_typing: false,
            });
        }
    }

    /// Programmatically set the composer text WITHOUT tripping the outbound
    /// typing indicator (draft/edit restore, event-creator prefill are not
    /// real keystrokes). Guards `suppress_typing` around the set_text.
    fn set_composer_text_silent(inner: &Rc<ChatViewInner>, text: &str) {
        inner.suppress_typing.set(true);
        inner.input_view.buffer().set_text(text);
        inner.suppress_typing.set(false);
    }

    /// Find the app-wide `adw::ToastOverlay` by walking up the widget tree from
    /// the chat root. Lets chat-view surface a toast without a bridge round-trip.
    fn toast_overlay(inner: &ChatViewInner) -> Option<adw::ToastOverlay> {
        let mut w = inner.root.parent();
        while let Some(cur) = w {
            if let Ok(overlay) = cur.clone().downcast::<adw::ToastOverlay>() {
                return Some(overlay);
            }
            w = cur.parent();
        }
        None
    }

    /// Force scroll-to-bottom (for chat switch, send, history load).
    /// Fires on the next N vadjustment `changed` signals.
    fn force_scroll_to_bottom(inner: &ChatViewInner, pulses: u32) {
        inner.scroll_pending.set(pulses);
        inner.at_bottom.set(true);
        inner.goto_latest_btn.set_visible(false);
        let adj = inner.scroll.vadjustment();
        adj.set_value(adj.upper() - adj.page_size());
    }

    /// Auto-scroll only if user is already at the bottom (for incoming messages).
    fn scroll_if_at_bottom(inner: &ChatViewInner) {
        if inner.at_bottom.get() {
            inner.scroll_pending.set(3);
            let adj = inner.scroll.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        }
    }

    /// Schedule a scroll after GTK has allocated the target bubble. Keeping the
    /// lookup inside the idle callback also makes this reliable for callers
    /// that request a jump during history construction.
    fn jump_to_message_inner(inner: &Rc<ChatViewInner>, msg_id: &str) -> bool {
        let resolved_id = inner
            .id_remap
            .borrow()
            .get(msg_id)
            .cloned()
            .unwrap_or_else(|| msg_id.to_string());
        if !inner.bubbles.borrow().contains_key(&resolved_id) {
            *inner.pending_message_jump.borrow_mut() = Some(resolved_id);
            return false;
        }

        *inner.pending_message_jump.borrow_mut() = None;
        let inner_w = Rc::downgrade(inner);
        glib::idle_add_local_once(move || {
            let Some(inner) = inner_w.upgrade() else {
                return;
            };
            let widget = inner
                .bubbles
                .borrow()
                .get(&resolved_id)
                .map(|bubble| bubble.widget().clone());
            let Some(widget) = widget else {
                return;
            };

            // A jump is an explicit user action, so it takes precedence over
            // the automatic "stay at bottom" lock.
            inner.at_bottom.set(false);
            inner.scroll_pending.set(0);
            let origin = gtk4::graphene::Point::new(0.0, 0.0);
            if let Some(point) = widget.compute_point(&inner.messages_box, &origin) {
                let adj = inner.scroll.vadjustment();
                let target = (point.y() as f64 - adj.page_size() * 0.25).clamp(
                    adj.lower(),
                    (adj.upper() - adj.page_size()).max(adj.lower()),
                );
                adj.set_value(target);
                inner
                    .goto_latest_btn
                    .set_visible(target < adj.upper() - adj.page_size() - 60.0);
            }

            widget.add_css_class("flash-highlight");
            glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                widget.remove_css_class("flash-highlight");
            });
        });
        true
    }

    /// Scroll to and highlight a message in the open chat. Returns `true` when
    /// the bubble is already available. If history is still loading, the jump
    /// is queued and automatically completed when the message is inserted.
    pub fn jump_to_message(&self, msg_id: &str) -> bool {
        Self::jump_to_message_inner(&self.inner, msg_id)
    }

    /// Open a chat immediately (sets current_chat_id, clears messages, shows loading).
    /// Returns false if the chat is already open (no reload needed).
    /// Restore a failed edit: put the edited text back in the composer and
    /// re-open edit mode so the user doesn't lose what they typed. Only applies
    /// if the failed edit's chat is still open.
    pub fn restore_failed_edit(&self, chat_id: &str, msg_id: &str, new_text: &str) {
        if self.inner.current_chat_id.borrow().as_deref() != Some(chat_id) {
            return;
        }
        // Only reclaim the composer if it's empty — otherwise we'd clobber a
        // fresh draft the user typed while the edit was in flight. In that case
        // surface the failed text via a toast so it isn't silently lost, and do
        // NOT re-arm editing_msg (the next Enter would fire an unintended edit).
        let buf = self.inner.input_view.buffer();
        let current = buf
            .text(&buf.start_iter(), &buf.end_iter(), false)
            .to_string();
        if !current.trim().is_empty() {
            if let Some(overlay) = Self::toast_overlay(&self.inner) {
                overlay.add_toast(adw::Toast::new(&format!(
                    "Edit failed — your text: {new_text}"
                )));
            }
            return;
        }
        // suppress_typing: this is a programmatic restore, not a keystroke.
        Self::set_composer_text_silent(&self.inner, new_text);
        *self.inner.editing_msg.borrow_mut() = Some((chat_id.to_string(), msg_id.to_string()));
        self.inner.edit_banner.set_reveal_child(true);
        self.inner.input_view.grab_focus();
    }

    /// Update the group-sender name label on any open bubbles from this sender
    /// (late name resolution — a participant that showed a raw number now shows
    /// their real name without needing to reopen the chat).
    pub fn refresh_sender_name(&self, chat_id: &str, sender_id: &str, name: &str) {
        if self.inner.current_chat_id.borrow().as_deref() != Some(chat_id) {
            return;
        }
        for bubble in self.inner.bubbles.borrow().values() {
            bubble.update_sender_name(sender_id, name);
        }
    }

    pub fn open_chat(&self, chat_id: String, chat_name: &str) -> bool {
        // A chat is now selected — enable the compose bar (disabled at startup so
        // typing/Send/attach don't silently no-op on the "Select a chat" pane).
        self.inner.input_bar.set_sensitive(true);
        // If this chat is already displayed, just update the header and skip reload
        let already_open = self.inner.current_chat_id.borrow().as_deref() == Some(&chat_id);
        if already_open {
            self.inner.header_name.set_text(chat_name);
            let is_favorite = self.inner.favorite_chats.borrow().contains(&chat_id);
            Self::apply_favorite_button(&self.inner, is_favorite);
            self.inner.input_view.grab_focus();
            return false;
        }

        // Settle any in-flight fade before the message surface is replaced.
        // The newly loaded history starts its own transition after rendering.
        Self::settle_chat_switch_animation(&self.inner);

        // ── Flush the outgoing chat's typing indicator ──
        // Before we swap current_chat_id, tell the OLD chat we've stopped
        // typing (using the stored routed target) and tear down its idle timer,
        // so the recipient doesn't get stuck on "typing…" after we leave.
        Self::cancel_typing(&self.inner, true);

        // ── Save draft for outgoing chat ──
        if let Some(old_chat_id) = self.inner.current_chat_id.borrow().clone() {
            let buf = self.inner.input_view.buffer();
            let draft = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .to_string();
            if draft.trim().is_empty() {
                self.inner.drafts.borrow_mut().remove(&old_chat_id);
            } else {
                self.inner
                    .drafts
                    .borrow_mut()
                    .insert(old_chat_id.clone(), draft);
            }

            // ── Save pending attachment for the OUTGOING chat ──
            // Without this, a paste/drop staged in chat A leaked into
            // chat B when the user switched chats — pending_image_path
            // was global, not per-chat.
            let pending = self
                .inner
                .pending_image_path
                .borrow()
                .clone()
                .map(PendingAttachment::File)
                .or_else(|| {
                    let mp4_url = self.inner.pending_gif_url.borrow().clone()?;
                    let preview_url = self.inner.pending_gif_preview_url.borrow().clone()?;
                    Some(PendingAttachment::Gif {
                        mp4_url,
                        preview_url,
                    })
                });
            if let Some(pending) = pending {
                self.inner
                    .pending_attachments
                    .borrow_mut()
                    .insert(old_chat_id, pending);
            } else {
                self.inner
                    .pending_attachments
                    .borrow_mut()
                    .remove(&old_chat_id);
            }
        }

        // ── Cancel any in-flight AI autocorrect ──
        // Invalidate pending corrections so stale responses from the old chat
        // don't overwrite text in the new chat.
        crate::ui::autocorrect::cancel_pending();

        // Clear edit mode
        *self.inner.editing_msg.borrow_mut() = None;
        self.inner.edit_banner.set_reveal_child(false);

        // Cancel any pending AI autocorrect from the previous chat
        crate::ui::autocorrect::cancel_pending();

        *self.inner.current_chat_id.borrow_mut() = Some(chat_id.clone());
        *self.inner.pending_message_jump.borrow_mut() = None;
        // Clear send group mode when switching to a real chat
        *self.inner.send_group_ids.borrow_mut() = None;
        // Update the WhatsApp/SMS toggle in the header for this chat.
        ChatViewPanel::apply_send_mode_btn(&self.inner, &chat_id);
        let is_favorite = self.inner.favorite_chats.borrow().contains(&chat_id);
        Self::apply_favorite_button(&self.inner, is_favorite);
        self.inner.header_name.set_text(chat_name);
        // Clear group subtitle — it'll be set when GroupMembers arrives
        self.inner.header_subtitle.set_text("");
        self.inner.header_subtitle.set_visible(false);

        // ── Restore draft for incoming chat ──
        let draft = self
            .inner
            .drafts
            .borrow()
            .get(&chat_id)
            .cloned()
            .unwrap_or_default();
        // Programmatic restore — suppress the outbound typing indicator so
        // merely clicking a chat with a saved draft doesn't broadcast "typing…".
        Self::set_composer_text_silent(&self.inner, &draft);

        // ── Restore pending attachment for incoming chat ──
        // Always reset the active pending state first so a stale
        // attachment from the previous chat can't bleed through if this
        // chat has none. set_pending_attachment handles paintable +
        // label setup; clearing covers the no-pending case.
        *self.inner.pending_image_path.borrow_mut() = None;
        *self.inner.pending_gif_url.borrow_mut() = None;
        *self.inner.pending_gif_preview_url.borrow_mut() = None;
        self.inner.image_preview_bar.set_visible(false);
        self.inner
            .image_preview_pic
            .set_paintable(None::<&gtk4::gdk::Paintable>);
        self.inner
            .preview_label
            .set_markup("Press Enter to send, Escape to cancel");
        let saved = self
            .inner
            .pending_attachments
            .borrow()
            .get(&chat_id)
            .cloned();
        if let Some(saved) = saved {
            match saved {
                PendingAttachment::File(path) => {
                    // Re-render the preview UI for the saved file via the same
                    // helper used by paste/drop/file-chooser.
                    set_pending_attachment(&self.inner, &path);
                }
                PendingAttachment::Gif {
                    mp4_url,
                    preview_url,
                } => stage_pending_gif(&self.inner, mp4_url, preview_url, None),
            }
        }
        // Normalise the send button for the freshly-restored draft/attachment
        // state (the connect_changed from set_text above may have observed a
        // stale pending value from the previous chat).
        update_send_button_state(&self.inner);

        // Clear message area and search/media state
        remove_all_children(&self.inner.messages_box);
        self.inner.bubbles.borrow_mut().clear();
        self.inner.search_texts.borrow_mut().clear();
        self.inner.id_remap.borrow_mut().clear();
        self.inner.media_items.borrow_mut().clear();
        *self.inner.last_msg_date.borrow_mut() = None;
        self.inner.search_entry.set_text("");
        self.inner.search_revealer.set_reveal_child(false);
        // Restore typing indicator for the newly opened chat (if anyone is typing)
        {
            let all = self.inner.all_typers.borrow();
            if let Some(typers) = all.get(&chat_id) {
                if !typers.is_empty() {
                    let label = typers.join(", ");
                    self.inner
                        .typing_name
                        .set_markup(&format!("<small><b>{label}</b> </small>"));
                    self.inner.typing_box.set_visible(true);
                } else {
                    self.inner.typing_box.set_visible(false);
                }
            } else {
                self.inner.typing_box.set_visible(false);
            }
        }
        self.inner.pin_banner.set_visible(false);

        // Show loading placeholder
        let placeholder = Label::new(Some("Loading messages…"));
        placeholder.set_widget_name("placeholder");
        placeholder.add_css_class("dim-label");
        placeholder.set_vexpand(true);
        placeholder.set_valign(Align::Center);
        self.inner.messages_box.append(&placeholder);

        // Clear mention state
        self.inner.group_members.borrow_mut().clear();
        self.inner.pending_mentions.borrow_mut().clear();

        // Fetch group members for @ mentions
        let cid = self
            .inner
            .current_chat_id
            .borrow()
            .clone()
            .unwrap_or_default();
        if cid.ends_with("@g.us") {
            self.inner
                .bridge
                .send_command(WaCommand::GetGroupMembers { chat_id: cid });
        }

        // Focus the message input so user can start typing immediately
        // (and so Ctrl+V hits the paste handler on input_view, not
        // whatever widget happened to be focused before — e.g. the
        // chat-list SearchEntry when picking from filtered results).
        self.inner.input_view.grab_focus();

        // Belt-and-braces: idle-re-grab. When the chat is selected from
        // a SearchEntry-filtered list, GTK can route focus back to the
        // SearchEntry after the row-activated callback returns (the
        // search entry was the user's last interactive widget). Without
        // this idle pass, Ctrl+V immediately after click would hit the
        // SearchEntry instead of input_view. Idle priority runs after
        // all pending focus events have been processed.
        let inp = self.inner.input_view.clone();
        glib::idle_add_local_once(move || {
            inp.grab_focus();
        });
        true
    }

    /// Open a send group as a virtual chat in the existing message pane.
    /// Messages typed here are dispatched via MultiSend to all group members.
    pub fn open_send_group(&self, group_name: &str, chat_ids: Vec<String>) {
        let virtual_id = format!("sendgroup::{}", group_name);
        let n = chat_ids.len();
        // open_chat resets the UI and clears send_group_ids, so we set AFTER.
        let switched_chat = self.open_chat(virtual_id, group_name);
        *self.inner.send_group_ids.borrow_mut() = Some(chat_ids);

        self.inner
            .header_subtitle
            .set_text(&format!("{n} chats \u{2022} Send Group"));
        self.inner.header_subtitle.set_visible(true);

        // Remove "Loading messages" placeholder and load send group history
        self.remove_placeholder();
        let history = load_send_group_history(group_name);
        if history.is_empty() {
            let label = Label::new(Some(
                "No messages yet \u{2014} type below to send to all group members.",
            ));
            label.set_widget_name("placeholder");
            label.add_css_class("dim-label");
            label.set_vexpand(true);
            label.set_valign(Align::Center);
            self.inner.messages_box.append(&label);
        } else {
            for msg in history {
                Self::append_history_bubble_to_inner(&self.inner, msg);
            }
            Self::force_scroll_to_bottom(&self.inner, 3);
        }
        if switched_chat {
            Self::start_chat_switch_animation(&self.inner);
        }
    }

    /// Replace the loading placeholder with history messages (or a "no history" notice).
    /// Only applies if `chat_id` still matches the currently open chat.
    pub fn load_history(&self, chat_id: &str, messages: Vec<IncomingMessage>) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);

        if !is_current {
            return;
        }

        // Remove placeholder
        self.remove_placeholder();

        if messages.is_empty() {
            let label = Label::new(Some(
                "No message history yet — new messages will appear here.",
            ));
            label.set_widget_name("placeholder");
            label.add_css_class("dim-label");
            label.set_vexpand(true);
            label.set_valign(Align::Center);
            self.inner.messages_box.append(&label);
            Self::start_chat_switch_animation(&self.inner);
            return;
        }

        // Render ALL messages synchronously, oldest first. We previously
        // streamed older messages via a prepend timer to make first paint
        // instant, but GTK4's GL renderer didn't reliably paint widgets
        // inserted at the top via insert_child_after — they remained
        // invisible until a mouse-hover damaged the surface. Switching to
        // pure append (oldest → newest) keeps the renderer's render-tree
        // invalidation working correctly.
        //
        // Cost: ~100-300ms hitch on chat switch for 50 messages. Acceptable
        // tradeoff vs. the "messages don't paint until hover" bug.
        for msg in messages {
            Self::append_history_bubble_to_inner(&self.inner, msg);
        }
        self.inner.at_bottom.set(true);
        self.inner.goto_latest_btn.set_visible(false);

        // Mark as at-bottom and seed a few pulses for the initial layout
        // and any async resizes (avatar texture loads). The
        // connect_changed handler will keep the scroll anchored to the
        // bottom while at_bottom remains true.
        Self::force_scroll_to_bottom(&self.inner, 4);

        // History rows are constructed synchronously above. Starting the
        // frame-clock animation only now guarantees that its first frame is
        // the real replacement conversation rather than an unpainted loading
        // placeholder, and that row construction cannot consume its duration.
        Self::start_chat_switch_animation(&self.inner);

        // BULLETPROOF PAINT FIX:
        // GTK4's GL renderer occasionally fails to paint newly-rendered
        // bubbles until something damages the surface. History bubbles no
        // longer animate, so one short delayed invalidation is sufficient and
        // avoids holding the chat in an animated/layout-heavy state for 700ms.
        {
            let inner_w = Rc::downgrade(&self.inner);
            glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
                let Some(inner) = inner_w.upgrade() else {
                    return;
                };
                inner.messages_box.set_visible(false);
                inner.messages_box.set_visible(true);
                let adj = inner.scroll.vadjustment();
                adj.set_value(adj.upper() - adj.page_size());
                inner.scroll_pending.set(2);
            });
        }
    }

    fn remove_placeholder(&self) {
        if let Some(child) = self.inner.messages_box.first_child() {
            if child.widget_name() == "placeholder" {
                self.inner.messages_box.remove(&child);
            }
        }
    }

    pub fn append_message(&self, msg: IncomingMessage) {
        let inner = &self.inner;

        // Match the current chat — tolerant of LID/phone JID aliases so
        // that self-messages from phone (which may arrive as @lid when the
        // open chat is @s.whatsapp.net, or vice versa) still render.
        let is_current = {
            let current = inner.current_chat_id.borrow();
            match current.as_deref() {
                Some(cid) => {
                    if cid == msg.chat_id {
                        true
                    } else {
                        // Check LID↔phone alias via the shared lid_to_phone map.
                        let map = inner.lid_to_phone.borrow();
                        let cid_phone = map.get(cid).map(|s| s.as_str());
                        let cid_lid = map
                            .iter()
                            .find(|(_, v)| v.as_str() == cid)
                            .map(|(k, _)| k.as_str());
                        let msg_phone = map.get(&msg.chat_id).map(|s| s.as_str());
                        let msg_lid = map
                            .iter()
                            .find(|(_, v)| v.as_str() == msg.chat_id.as_str())
                            .map(|(k, _)| k.as_str());
                        cid_phone == Some(msg.chat_id.as_str())
                            || cid_lid == Some(msg.chat_id.as_str())
                            || msg_phone == Some(cid)
                            || msg_lid == Some(cid)
                    }
                }
                None => false,
            }
        };

        // If no match AND this is a self-message, try refreshing the LID map
        // from disk (runtime may have learned new mappings after we loaded).
        let is_current = if !is_current && msg.is_from_me {
            let fresh = crate::ui::runtime::load_lid_phone_map();
            let current = inner.current_chat_id.borrow();
            let matched = match current.as_deref() {
                Some(cid) => {
                    let cid_phone = fresh.get(cid).map(|s| s.as_str());
                    let cid_lid = fresh
                        .iter()
                        .find(|(_, v)| v.as_str() == cid)
                        .map(|(k, _)| k.as_str());
                    let msg_phone = fresh.get(&msg.chat_id).map(|s| s.as_str());
                    let msg_lid = fresh
                        .iter()
                        .find(|(_, v)| v.as_str() == msg.chat_id.as_str())
                        .map(|(k, _)| k.as_str());
                    cid_phone == Some(msg.chat_id.as_str())
                        || cid_lid == Some(msg.chat_id.as_str())
                        || msg_phone == Some(cid)
                        || msg_lid == Some(cid)
                }
                None => false,
            };
            if matched {
                *inner.lid_to_phone.borrow_mut() = fresh;
            }
            matched
        } else {
            is_current
        };

        if !is_current {
            // Bubble doesn't go in the wrong chat view. The chat list still
            // updates via update_last_message() on the same WaEvent, and the
            // message is persisted to s.history — opening the chat shows it.
            // Demoted to debug: this log line previously fired on every
            // self-message-not-in-current-chat and made it look broken.
            if msg.is_from_me {
                log::debug!(
                    "append_message: self-message for chat={} not current ({:?}) — chat list still updates",
                    msg.chat_id,
                    inner.current_chat_id.borrow().as_deref()
                );
            }
            return;
        }

        // Remove loading/empty placeholder on first real message
        self.remove_placeholder();

        self.append_message_inner(msg);
    }

    fn append_message_inner(&self, msg: IncomingMessage) {
        Self::append_bubble_to_inner(&self.inner, msg);
    }

    pub fn confirm_bubble(&self, tmp_id: &str, real_id: &str) {
        // Record tmp→real so bubble-menu closures (which froze the tmp id at
        // creation time) can resolve to the real server id for edit/star/etc.
        if tmp_id != real_id {
            self.inner
                .id_remap
                .borrow_mut()
                .insert(tmp_id.to_string(), real_id.to_string());
        }
        let mut bubbles = self.inner.bubbles.borrow_mut();
        if let Some(bubble) = bubbles.remove(tmp_id) {
            if bubbles.contains_key(real_id) {
                // Residual race: the server echo already rendered a bubble under
                // real_id (it won the race against this confirm). Drop the
                // optimistic widget instead of stacking a second one.
                self.inner.messages_box.remove(bubble.widget());
            } else {
                bubble.update_receipt(&ReceiptStatus::Sent);
                bubbles.insert(real_id.to_string(), bubble);
            }
        }
        let mut texts = self.inner.search_texts.borrow_mut();
        if let Some(text) = texts.remove(tmp_id) {
            texts.entry(real_id.to_string()).or_insert(text);
        }
    }

    fn append_bubble_to_inner(inner: &Rc<ChatViewInner>, msg: IncomingMessage) {
        Self::append_bubble_to_inner_at(inner, msg, true);
    }

    fn append_history_bubble_to_inner(inner: &Rc<ChatViewInner>, msg: IncomingMessage) {
        Self::append_bubble_to_inner_at(inner, msg, false);
    }

    fn append_bubble_to_inner_at(inner: &Rc<ChatViewInner>, msg: IncomingMessage, animate: bool) {
        // Dedup: if a bubble already exists for this msg id (e.g. optimistic bubble
        // was already confirmed via MessageConfirmed), skip creating a new one.
        if inner.bubbles.borrow().contains_key(&msg.id) {
            return;
        }

        maybe_insert_date_separator(inner, msg.timestamp);

        let own_name = inner.own_name.borrow().clone();
        let bubble = MessageBubble::new(&msg, &own_name, &inner.bridge);

        // A quoted reply is a real navigation control: clicking or activating
        // it from the keyboard scrolls to and highlights the original message.
        if let Some(quoted_id) = bubble.quoted_msg_id.clone() {
            let inner_c = inner.clone();
            bubble.set_quoted_click_handler(move || {
                ChatViewPanel::jump_to_message_inner(&inner_c, &quoted_id);
            });
        }

        // Right-click context menu — anchor to the bubble widget
        let gesture = GestureClick::new();
        gesture.set_button(3);
        let inner_clone = inner.clone();
        let msg_clone = msg.clone();
        // NOTE: do NOT capture bubble.widget() in the closure — that creates a
        // ref cycle (widget → controller → closure → widget) and leaks memory.
        // Instead, get the widget from the gesture at callback time.
        gesture.connect_pressed(move |g, _, x, y| {
            if let Some(w) = g.widget() {
                show_message_menu(&inner_clone, &msg_clone, w.upcast_ref(), x, y);
            }
        });
        bubble.widget().add_controller(gesture);

        // Long-press (touch / single-tap) → same context menu, so touchscreen
        // users aren't limited to the faint chevron. Anchored at the press
        // point. As above, fetch the widget from the gesture to avoid a ref
        // cycle through the closure.
        let long_press = gtk4::GestureLongPress::new();
        let inner_lp = inner.clone();
        let msg_lp = msg.clone();
        long_press.connect_pressed(move |g, x, y| {
            if let Some(w) = g.widget() {
                show_message_menu(&inner_lp, &msg_lp, w.upcast_ref(), x, y);
            }
        });
        bubble.widget().add_controller(long_press);

        // Left-click in forward mode toggles message selection.
        // Uses CAPTURE phase so it intercepts before text selection handlers.
        let fwd_click = GestureClick::new();
        fwd_click.set_button(1);
        fwd_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let inner_fwd = inner.clone();
        let msg_id_fwd = msg.id.clone();
        fwd_click.connect_pressed(move |gesture, _, _, _| {
            if *inner_fwd.forward_mode.borrow() {
                ChatViewPanel::toggle_forward_select(&inner_fwd, &msg_id_fwd);
                // Stop propagation so text selection doesn't activate
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            }
        });
        bubble.widget().add_controller(fwd_click);

        // Wire chevron → full dropdown menu anchored to chevron
        {
            let inner_c = inner.clone();
            let msg_c = msg.clone();
            bubble.chevron_button().connect_clicked(move |btn| {
                show_message_menu(&inner_c, &msg_c, btn.upcast_ref(), 0.0, 0.0);
            });
        }

        // Wire quick action buttons: React, Reply, Forward
        // System-message bubbles have no hover actions — skip wiring
        if let Some((btn_react, btn_reply, btn_forward)) = bubble.quick_action_buttons() {
            let inner_c = inner.clone();
            let msg_c = msg.clone();
            btn_react.connect_clicked(move |btn| {
                show_react_picker(&inner_c, &msg_c, btn.upcast_ref());
            });

            let inner_c = inner.clone();
            let msg_c = msg.clone();
            btn_reply.connect_clicked(move |_| {
                do_reply(&inner_c, &msg_c);
            });

            let inner_c = inner.clone();
            let msg_id = msg.id.clone();
            let is_poll = msg.poll_question.is_some() && !msg.poll_options.is_empty();
            let poll_q = msg.poll_question.clone().unwrap_or_default();
            let poll_opts = msg.poll_options.clone();
            let poll_sel = if msg.poll_selectable == 0 {
                1
            } else {
                msg.poll_selectable
            };
            btn_forward.connect_clicked(move |_| {
                if is_poll {
                    let parent = inner_c
                        .root
                        .root()
                        .and_then(|r| r.downcast::<gtk4::Window>().ok());
                    let bridge = inner_c.bridge.clone();
                    let q = poll_q.clone();
                    let opts = poll_opts.clone();
                    crate::ui::chat_picker::show_chat_picker(
                        "Send poll to…",
                        true,
                        parent.as_ref(),
                        move |selected| {
                            for to_chat_id in &selected {
                                bridge.send_command(crate::bridge::WaCommand::SendPoll {
                                    chat_id: to_chat_id.clone(),
                                    question: q.clone(),
                                    options: opts.clone(),
                                    selectable_count: poll_sel,
                                });
                            }
                        },
                    );
                } else {
                    ChatViewPanel::enter_forward_mode(&inner_c, &msg_id);
                }
            });
        }

        // Build searchable text for this message
        let search_text = msg
            .text
            .clone()
            .or_else(|| msg.media_caption.clone())
            .or_else(|| msg.media_filename.clone())
            .unwrap_or_default();

        // Wire avatar click → open profile panel
        if !msg.is_from_me {
            let chat_id_for_profile = if msg.chat_id.ends_with("@g.us") {
                msg.sender_id.clone()
            } else {
                msg.chat_id.clone()
            };
            let name_for_profile = if msg.sender_name.is_empty() {
                crate::ui::runtime::display_name_from_jid(&chat_id_for_profile)
            } else {
                msg.sender_name.clone()
            };
            let inner_c = inner.clone();
            let activate_profile: Rc<dyn Fn()> = Rc::new(move || {
                if let Some(cb) = inner_c.on_profile_open.borrow().as_ref()
                    && !chat_id_for_profile.is_empty()
                {
                    cb(chat_id_for_profile.clone(), name_for_profile.clone());
                }
            });
            let av_gesture = GestureClick::new();
            av_gesture.set_button(1);
            let activate_click = activate_profile.clone();
            av_gesture.connect_released(move |g, _, _, _| {
                g.set_state(gtk4::EventSequenceState::Claimed);
                activate_click();
            });
            bubble.avatar_widget().add_controller(av_gesture);
            bubble.avatar_widget().set_cursor_from_name(Some("pointer"));
            bubble.avatar_widget().set_focusable(true);
            bubble
                .avatar_widget()
                .set_accessible_role(gtk4::AccessibleRole::Button);
            bubble
                .avatar_widget()
                .update_property(&[gtk4::accessible::Property::Label("Open contact profile")]);
            let key = gtk4::EventControllerKey::new();
            key.connect_key_pressed(move |_, key, _, _| {
                if matches!(
                    key,
                    gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::space
                ) {
                    activate_profile();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            bubble.avatar_widget().add_controller(key);
        }

        // Wire the contact card's native Button directly. It is keyboard
        // focusable by GTK and avoids recursively walking every bubble's widget
        // tree to rediscover a control MessageBubble already owns.
        if let (Some(jid), Some(msg_btn)) = (
            bubble.contact_jid.clone(),
            bubble.contact_message_button().cloned(),
        ) {
            let name = msg.contact_name.clone().unwrap_or_default();
            let inner_c = inner.clone();
            msg_btn.connect_clicked(move |_| {
                let j = jid.clone();
                ChatViewPanel::cancel_typing(&inner_c, true);
                *inner_c.current_chat_id.borrow_mut() = Some(j.clone());
                *inner_c.send_group_ids.borrow_mut() = None;
                *inner_c.reply_context.borrow_mut() = None;
                inner_c.reply_bar.set_visible(false);
                *inner_c.editing_msg.borrow_mut() = None;
                inner_c.edit_banner.set_reveal_child(false);
                *inner_c.pending_message_jump.borrow_mut() = None;
                ChatViewPanel::apply_send_mode_btn(&inner_c, &j);
                let is_favorite = inner_c.favorite_chats.borrow().contains(&j);
                ChatViewPanel::apply_favorite_button(&inner_c, is_favorite);
                inner_c.header_name.set_text(&name);
                while let Some(child) = inner_c.messages_box.first_child() {
                    inner_c.messages_box.remove(&child);
                }
                inner_c.bubbles.borrow_mut().clear();
                inner_c.search_texts.borrow_mut().clear();
                inner_c.id_remap.borrow_mut().clear();
                inner_c.media_items.borrow_mut().clear();
                *inner_c.last_msg_date.borrow_mut() = None;
                inner_c.search_entry.set_text("");
                inner_c.search_revealer.set_reveal_child(false);
                inner_c.pin_banner.set_visible(false);
                inner_c
                    .bridge
                    .send_command(WaCommand::StartNewChat { jid: j.clone() });
                inner_c.bridge.send_command(WaCommand::SetActiveChat {
                    chat_id: Some(j.clone()),
                });
                inner_c.bridge.send_command(WaCommand::LoadChat {
                    chat_id: j.clone(),
                    chat_name: name.clone(),
                });
                inner_c.input_view.grab_focus();
            });
        }

        // If this message already has a local media path, register it and wire the carousel
        if bubble.is_visual_media() {
            if let Some(path) = &msg.media_local_path {
                let entry = (msg.id.clone(), path.clone());
                inner.media_items.borrow_mut().push(entry);
                wire_image_click(inner, &bubble, &msg.id);
            }
        }

        // Load cached avatar for the sender.
        {
            let own = inner.own_jid.borrow();
            let own_ref = own.as_deref().unwrap_or("");
            let is_group = msg.chat_id.ends_with("@g.us");
            let mut jids_to_try: Vec<&str> = if msg.is_from_me {
                if own_ref.is_empty() {
                    vec![]
                } else {
                    vec![own_ref]
                }
            } else if !msg.sender_id.is_empty() {
                // In groups: only use sender_id (never fall back to group JID hero image)
                if is_group {
                    vec![&msg.sender_id]
                } else {
                    vec![&msg.sender_id, &msg.chat_id]
                }
            } else if !is_group {
                vec![&msg.chat_id] // DM: chat_id IS the contact's JID
            } else {
                vec![] // Group with no sender_id: no avatar (show initials)
            };
            // Always try "me" as last resort for own messages
            if msg.is_from_me {
                jids_to_try.push("me");
            }
            let lid_map = inner.lid_to_phone.borrow();
            'avatar: for jid in jids_to_try {
                // Strip device suffix (e.g., "126095978418213:19@lid" → "126095978418213@lid")
                let stripped = if let (Some(colon), Some(at)) = (jid.find(':'), jid.find('@')) {
                    if colon < at {
                        format!("{}{}", &jid[..colon], &jid[at..])
                    } else {
                        jid.to_string()
                    }
                } else {
                    jid.to_string()
                };

                // Try the JID itself, then its phone counterpart via LID→phone mapping
                let mut candidates = vec![stripped.clone()];
                if stripped.ends_with("@lid") {
                    if let Some(phone) = lid_map.get(&stripped) {
                        candidates.push(phone.clone());
                    }
                }

                for candidate in &candidates {
                    let safe = candidate.replace(['/', '\\', '@', ':'], "_");
                    let path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
                    if path.exists() {
                        if let Ok(abs) = path.canonicalize() {
                            bubble.set_avatar_image(&abs.to_string_lossy());
                        }
                        break 'avatar;
                    }
                }
            }
            drop(lid_map);
        }

        // Deduplicate — skip if this message ID is already displayed
        if inner.bubbles.borrow().contains_key(&msg.id) {
            // If the new message has link preview data that the old one doesn't,
            // replace the old bubble entirely (optimistic → enriched)
            let has_new_data = msg.link_title.is_some()
                || msg.link_thumbnail_path.is_some()
                || msg.contact_name.is_some();
            if has_new_data {
                // Remove old bubble and fall through to create a new one
                let mut bubbles = inner.bubbles.borrow_mut();
                if let Some(old) = bubbles.remove(&msg.id) {
                    inner.messages_box.remove(old.widget());
                }
                drop(bubbles);
            } else {
                Self::scroll_if_at_bottom(inner);
                return;
            }
        }

        // Tag the widget with the msg_id. Useful for runtime debugging
        // (e.g. inspecting widget tree via GTK Inspector to find a bubble
        // by message id) and harmless to leave in.
        bubble
            .widget()
            .set_widget_name(&format!("bubble-{}", msg.id));

        // Only genuinely live messages animate. Animating every history row
        // simultaneously caused hundreds of layout-changing margin updates and
        // negative-width GTK warnings on chat open. Honour the desktop reduced-
        // motion setting for live messages too.
        let animations_enabled = gtk4::Settings::default()
            .map(|settings| settings.is_gtk_enable_animations())
            .unwrap_or(true);
        if animate && animations_enabled {
            bubble.widget().add_css_class("bubble-enter");
            let bubble_w = bubble.widget().clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                bubble_w.remove_css_class("bubble-enter");
            });
        }

        inner.messages_box.append(bubble.widget());
        inner.bubbles.borrow_mut().insert(msg.id.clone(), bubble);
        inner
            .search_texts
            .borrow_mut()
            .insert(msg.id.clone(), search_text);

        let should_complete_jump = inner.pending_message_jump.borrow().as_deref() == Some(&msg.id);
        if should_complete_jump {
            Self::jump_to_message_inner(inner, &msg.id);
        }

        // Apply current search filter to the new bubble
        let query = inner.search_entry.text().to_lowercase();
        if !query.is_empty() {
            Self::apply_search_filter(inner, &query);
        }

        Self::scroll_if_at_bottom(inner);
    }

    pub fn set_media_loaded(
        &self,
        msg_id: &str,
        chat_id: &str,
        path: &str,
        _media_type: &crate::bridge::MediaType,
    ) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }

        let bubbles = self.inner.bubbles.borrow();
        let Some(bubble) = bubbles.get(msg_id) else {
            return;
        };

        // Register in media_items before calling set_media_loaded so the
        // click handler (wired below) can find itself in the list.
        if bubble.is_visual_media() {
            let mut items = self.inner.media_items.borrow_mut();
            if !items.iter().any(|(id, _)| id == msg_id) {
                items.push((msg_id.to_string(), path.to_string()));
            }
            drop(items);
            wire_image_click(&self.inner, bubble, msg_id);
        }

        bubble.set_media_loaded(path);
    }

    pub fn update_receipt(&self, msg_id: &str, status: ReceiptStatus) {
        if let Some(bubble) = self.inner.bubbles.borrow().get(msg_id) {
            bubble.update_receipt(&status);
        }
    }

    pub fn clear_chat(&self, chat_id: &str) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }
        remove_all_children(&self.inner.messages_box);
        self.inner.bubbles.borrow_mut().clear();
        let label = gtk4::Label::new(Some(
            "No message history yet — new messages will appear here.",
        ));
        label.set_widget_name("placeholder");
        label.add_css_class("dim-label");
        label.set_vexpand(true);
        label.set_valign(gtk4::Align::Center);
        self.inner.messages_box.append(&label);
    }

    pub fn remove_message(&self, chat_id: &str, msg_id: &str) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        let current_id = self.inner.current_chat_id.borrow().clone();
        log::info!(
            "remove_message: chat_id={chat_id} msg_id={msg_id} is_current={is_current} current={:?}",
            current_id
        );
        if !is_current {
            return;
        }
        let mut bubbles = self.inner.bubbles.borrow_mut();
        log::info!(
            "remove_message: {} bubbles in cache, looking for {msg_id}",
            bubbles.len()
        );
        if let Some(bubble) = bubbles.remove(msg_id) {
            // Replace bubble content with "deleted" placeholder (like WhatsApp)
            let widget = bubble.widget();
            remove_all_children(widget);
            let deleted_label = Label::new(Some("🚫 This message was deleted"));
            deleted_label.add_css_class("dim-label");
            deleted_label.add_css_class("caption");
            deleted_label.set_margin_top(8);
            deleted_label.set_margin_bottom(8);
            deleted_label.set_margin_start(12);
            deleted_label.set_margin_end(12);
            widget.append(&deleted_label);
        }
    }

    /// Update the text of an existing message bubble (used for edits).
    pub fn update_message_text(
        &self,
        chat_id: &str,
        msg_id: &str,
        new_text: &str,
        is_edited: bool,
    ) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }
        let bubbles = self.inner.bubbles.borrow();
        if let Some(bubble) = bubbles.get(msg_id) {
            bubble.update_text(new_text, is_edited);
        }
        // Update search index
        self.inner
            .search_texts
            .borrow_mut()
            .insert(msg_id.to_string(), new_text.to_lowercase());
    }

    pub fn show_forward_picker(&self, _chats: Vec<crate::bridge::ChatSummary>) {
        let msg_ids: Vec<String> = self.inner.forward_selected.borrow().clone();
        if msg_ids.is_empty() {
            return;
        }

        let msg_count = msg_ids.len();
        let parent = self
            .inner
            .root
            .root()
            .and_then(|r| r.downcast::<gtk4::Window>().ok());
        let bridge = self.inner.bridge.clone();
        let inner_ref = self.inner.clone();

        crate::ui::chat_picker::show_chat_picker(
            &format!(
                "Forward {msg_count} message{}",
                if msg_count > 1 { "s" } else { "" }
            ),
            true,
            parent.as_ref(),
            move |selected_chat_ids| {
                for to_chat_id in &selected_chat_ids {
                    bridge.send_command(crate::bridge::WaCommand::ForwardMessages {
                        to_chat_id: to_chat_id.clone(),
                        msg_ids: msg_ids.clone(),
                    });
                }
                ChatViewPanel::exit_forward_mode(&inner_ref);
            },
        );
    }

    pub fn set_quick_replies(&self, replies: Vec<crate::bridge::QuickReplyData>) {
        let mut qr = self.inner.quick_replies.borrow_mut();
        if replies.is_empty() {
            // Empty list = signal to clear defaults (full sync starting)
            qr.clear();
        } else {
            // Merge incoming replies
            for r in &replies {
                if let Some(existing) = qr.iter_mut().find(|q| q.shortcut == r.shortcut) {
                    existing.text = r.message.clone();
                } else {
                    qr.push(crate::ui::quick_replies::QuickReply {
                        shortcut: r.shortcut.clone(),
                        text: r.message.clone(),
                    });
                }
            }
        }
        crate::ui::quick_replies::save(&qr);
    }

    pub fn show_reaction(&self, chat_id: &str, msg_id: &str, reactions: &[(String, String)]) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }
        // Rebuild the whole reaction row from the full deduped set — groups by
        // emoji, no duplicate rows, and an empty set clears it (removal).
        if let Some(bubble) = self.inner.bubbles.borrow().get(msg_id) {
            bubble.rebuild_reactions(reactions);
        }
    }

    fn maybe_render_picker_results(inner: &Rc<ChatViewInner>, kind: PickerSearchKind) {
        let expected_page = match kind {
            PickerSearchKind::Gif => 1,
            PickerSearchKind::Sticker => 2,
        };
        if !inner.emoji_popover.is_visible() || inner.picker_page.get() != expected_page {
            return;
        }

        let (cached, current_request, rendered_request) = match kind {
            PickerSearchKind::Gif => (
                inner.gif_result_cache.borrow().clone(),
                inner.gif_search_request.get(),
                &inner.gif_rendered_request,
            ),
            PickerSearchKind::Sticker => (
                inner.sticker_result_cache.borrow().clone(),
                inner.sticker_search_request.get(),
                &inner.sticker_rendered_request,
            ),
        };
        let Some(cached) = cached else {
            return;
        };
        if cached.request_id != current_request || rendered_request.get() == cached.request_id {
            return;
        }

        match kind {
            PickerSearchKind::Gif => Self::render_gif_results(inner, &cached),
            PickerSearchKind::Sticker => Self::render_sticker_results(inner, &cached),
        }
        rendered_request.set(cached.request_id);
    }

    fn render_gif_results(inner: &Rc<ChatViewInner>, result: &PickerResultSet) {
        let grid = &inner.gif_grid;
        grid.remove_all();
        grid.set_sensitive(true);
        inner.gif_status.remove_css_class("error");
        if let Some(error) = result.error.as_deref() {
            inner.gif_status.add_css_class("error");
            inner.gif_status.set_text(error);
            return;
        }
        if result.results.is_empty() {
            let label = if result.query.eq_ignore_ascii_case("trending") {
                "No trending GIFs are available right now".to_string()
            } else {
                format!("No GIFs found for “{}”", result.query)
            };
            inner.gif_status.set_text(&label);
            return;
        }
        inner.gif_status.set_text(&format!(
            "{} results · Powered by Tenor",
            result.results.len()
        ));

        for gif in &result.results {
            let frame = Box::new(Orientation::Vertical, 2);
            frame.set_size_request(170, 130);

            // Download and show preview image asynchronously
            let pic = gtk4::Picture::new();
            pic.set_size_request(170, 110);
            pic.set_content_fit(gtk4::ContentFit::Cover);
            pic.set_can_shrink(true);
            frame.append(&pic);

            let title = Label::new(Some(&gif.title));
            title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            title.set_max_width_chars(18);
            title.add_css_class("caption");
            title.add_css_class("dim-label");
            title.set_halign(Align::Center);
            frame.append(&title);

            load_picker_preview(&pic, gif.preview_url.clone(), None, None);

            // A real Button supplies focus, Enter/Space activation and an
            // accessible role without custom key controllers.
            let tile = Button::new();
            tile.set_has_frame(false);
            tile.add_css_class("flat");
            tile.set_focusable(true);
            tile.set_cursor_from_name(Some("pointer"));
            tile.set_child(Some(&frame));
            let accessible_label = format!("Choose GIF: {}", gif.title);
            tile.set_tooltip_text(Some(&accessible_label));
            tile.update_property(&[gtk4::accessible::Property::Label(&accessible_label)]);

            let mp4_url = gif.mp4_url.clone();
            let preview_url_c = gif.preview_url.clone();
            let result_pic = pic.clone();
            let inner_w = Rc::downgrade(inner);
            tile.connect_clicked(move |_| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                stage_pending_gif(
                    &inner_c,
                    mp4_url.clone(),
                    preview_url_c.clone(),
                    result_pic.paintable(),
                );
                inner_c.emoji_popover.popdown();
            });
            grid.append(&tile);
        }
    }

    fn render_sticker_results(inner: &Rc<ChatViewInner>, result: &PickerResultSet) {
        let grid = &inner.sticker_grid;
        grid.remove_all();
        grid.set_sensitive(true);
        inner.sticker_status.remove_css_class("error");
        if let Some(error) = result.error.as_deref() {
            inner.sticker_status.add_css_class("error");
            inner.sticker_status.set_text(error);
            return;
        }
        if result.results.is_empty() {
            let label = if result.query.eq_ignore_ascii_case("trending") {
                "No trending stickers are available right now".to_string()
            } else {
                format!("No stickers found for “{}”", result.query)
            };
            inner.sticker_status.set_text(&label);
            return;
        }
        inner.sticker_status.set_text(&format!(
            "{} results · Powered by Tenor",
            result.results.len()
        ));

        for sticker in &result.results {
            let pic = gtk4::Picture::new();
            pic.set_size_request(90, 90);
            pic.set_content_fit(gtk4::ContentFit::Contain);
            pic.set_can_shrink(true);

            load_picker_preview(&pic, sticker.preview_url.clone(), None, None);

            let tile = Button::new();
            tile.set_has_frame(false);
            tile.add_css_class("flat");
            tile.set_focusable(true);
            tile.set_cursor_from_name(Some("pointer"));
            tile.set_child(Some(&pic));
            let accessible_label = format!("Send sticker: {}", sticker.title);
            tile.set_tooltip_text(Some(&accessible_label));
            tile.update_property(&[gtk4::accessible::Property::Label(&accessible_label)]);

            let webp_url = sticker.mp4_url.clone(); // reused field for webp URL
            let inner_w = Rc::downgrade(inner);
            tile.connect_clicked(move |_| {
                let Some(inner_c) = inner_w.upgrade() else {
                    return;
                };
                let bridge = inner_c.bridge.clone();
                let chat_id = inner_c.current_chat_id.borrow().clone();
                inner_c.emoji_popover.popdown();
                if let Some(chat_id) = chat_id {
                    let tmp_id = gen_tmp_id();
                    Self::append_optimistic_picker_media(
                        &inner_c,
                        &chat_id,
                        &tmp_id,
                        crate::bridge::MediaType::Sticker,
                    );
                    bridge.send_command(WaCommand::SendSticker {
                        chat_id: ChatViewPanel::resolve_send_target(&chat_id),
                        webp_url: webp_url.clone(),
                        tmp_id,
                    });
                }
            });
            grid.append(&tile);
        }
    }

    pub fn show_gif_results(
        &self,
        request_id: u64,
        query: &str,
        gifs: Vec<crate::bridge::GifResult>,
        error: Option<&str>,
    ) {
        if self.inner.gif_search_request.get() != request_id {
            return;
        }
        *self.inner.gif_result_cache.borrow_mut() = Some(PickerResultSet {
            request_id,
            query: query.to_string(),
            results: gifs,
            error: error.map(str::to_string),
        });
        Self::maybe_render_picker_results(&self.inner, PickerSearchKind::Gif);
    }

    pub fn show_sticker_results(
        &self,
        request_id: u64,
        query: &str,
        stickers: Vec<crate::bridge::GifResult>,
        error: Option<&str>,
    ) {
        if self.inner.sticker_search_request.get() != request_id {
            return;
        }
        *self.inner.sticker_result_cache.borrow_mut() = Some(PickerResultSet {
            request_id,
            query: query.to_string(),
            results: stickers,
            error: error.map(str::to_string),
        });
        Self::maybe_render_picker_results(&self.inner, PickerSearchKind::Sticker);
    }

    pub fn set_group_members(&self, chat_id: &str, members: Vec<crate::bridge::GroupMember>) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if is_current {
            // Show participant names in header subtitle (like WhatsApp Web)
            let names: Vec<&str> = members
                .iter()
                .filter(|m| !m.name.is_empty() && !m.name.contains('@'))
                .map(|m| m.name.as_str())
                .take(8) // Limit to avoid overflow
                .collect();
            if !names.is_empty() {
                self.inner.header_subtitle.set_text(&names.join(", "));
                self.inner.header_subtitle.set_visible(true);
            }
            *self.inner.group_members.borrow_mut() = members;
        }
    }

    /// Update the header name if this chat is currently open.
    ///
    /// Also forces a re-realization of messages_box because changing the
    /// header text invalidates header layout, which can shrink the message
    /// pane viewport AFTER bubbles are rendered — that triggered the GTK4
    /// GL renderer paint bug where bubbles became invisible until hover.
    /// Most visible on contacts whose name resolves slowly (phone number
    /// → real name swap fires ChatNameUpdated post-load).
    pub fn update_chat_name(&self, chat_id: &str, name: &str) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if is_current && self.inner.header_name.text() != name {
            self.inner.header_name.set_text(name);
            // Hide/show messages_box on the next idle tick to force the
            // GL renderer to rebuild its render tree after the header
            // layout shift. This is the same workaround used at the end
            // of load_history.
            let inner_w = Rc::downgrade(&self.inner);
            glib::idle_add_local_once(move || {
                let Some(inner) = inner_w.upgrade() else {
                    return;
                };
                inner.messages_box.set_visible(false);
                inner.messages_box.set_visible(true);
                let adj = inner.scroll.vadjustment();
                if inner.at_bottom.get() {
                    adj.set_value(adj.upper() - adj.page_size());
                }
            });
        }
    }

    /// Show a pinned message banner at the top of the chat (fixed position, not scrollable)
    /// Update poll vote display with all accumulated votes
    pub fn update_poll_votes(
        &self,
        chat_id: &str,
        poll_msg_id: &str,
        all_votes: &[(String, Vec<String>)],
    ) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        log::info!(
            "update_poll_votes: chat={chat_id} poll={poll_msg_id} voters={} is_current={is_current}",
            all_votes.len()
        );
        if !is_current {
            return;
        }

        let bubbles = self.inner.bubbles.borrow();
        let found = bubbles.get(poll_msg_id).is_some();
        log::info!(
            "  bubble lookup: found={found} total_bubbles={}",
            bubbles.len()
        );
        if let Some(bubble) = bubbles.get(poll_msg_id) {
            bubble.update_poll_votes(all_votes);
        }
    }

    pub fn show_pinned_banner(&self, chat_id: &str, msg_id: &str) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }

        // Find the message text
        let text = self
            .inner
            .bubbles
            .borrow()
            .get(msg_id)
            .and_then(|b| b.text.clone())
            .unwrap_or_else(|| "Pinned message".to_string());

        // Clear and rebuild the pin banner
        let banner = &self.inner.pin_banner;
        remove_all_children(banner);

        let pin_icon = Label::new(Some("📌"));

        // Clickable text area — scrolls to the pinned message
        let pin_btn = Button::new();
        pin_btn.add_css_class("flat");
        pin_btn.set_hexpand(true);
        let pin_text = Label::new(Some(&text));
        pin_text.set_wrap(true);
        pin_text.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        pin_text.set_lines(2);
        pin_text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        pin_text.set_halign(Align::Start);
        pin_btn.set_child(Some(&pin_text));

        let inner_c = self.inner.clone();
        let msg_id_c = msg_id.to_string();
        pin_btn.connect_clicked(move |_| {
            ChatViewPanel::jump_to_message_inner(&inner_c, &msg_id_c);
        });

        let close_btn = Button::from_icon_name("window-close-symbolic");
        close_btn.add_css_class("flat");
        let banner_c = banner.clone();
        close_btn.connect_clicked(move |_| {
            banner_c.set_visible(false);
        });

        banner.append(&pin_icon);
        banner.append(&pin_btn);
        banner.append(&close_btn);
        banner.set_visible(true);
    }

    pub fn set_typing_indicator(&self, chat_id: &str, sender_name: &str, is_typing: bool) {
        // Track per-chat so state survives chat switches.
        // Hide raw JIDs (unresolved LIDs, phone numbers) behind "Someone".
        let looks_like_jid = sender_name.contains('@')
            || (sender_name.len() > 8 && sender_name.chars().all(|c| c.is_ascii_digit()));
        let display = if sender_name.is_empty() || looks_like_jid {
            "Someone".to_string()
        } else {
            sender_name.to_string()
        };
        let mut all = self.inner.all_typers.borrow_mut();
        let chat_typers = all.entry(chat_id.to_string()).or_default();
        if is_typing {
            if !chat_typers.contains(&display) {
                chat_typers.push(display.clone());
            }
        } else {
            chat_typers.retain(|t| *t != display);
        }

        // Only update UI if this is the currently open chat
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if !is_current {
            return;
        }

        let typers = all.get(chat_id).cloned().unwrap_or_default();
        drop(all);
        if typers.is_empty() {
            self.inner.typing_box.set_visible(false);
        } else {
            let label = typers.join(", ");
            self.inner
                .typing_name
                .set_markup(&format!("<small><b>{label}</b> </small>"));
            self.inner.typing_box.set_visible(true);
        }

        // Auto-expire typing indicator after 15 seconds.
        // WhatsApp clients re-send Composing every ~10s while still typing,
        // so if we don't receive a refresh within 15s, the person stopped.
        if is_typing {
            let inner_w = Rc::downgrade(&self.inner);
            let cid = chat_id.to_string();
            let name = display;
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_secs(15), move || {
                if let Some(inner) = inner_w.upgrade() {
                    let mut all = inner.all_typers.borrow_mut();
                    if let Some(typers) = all.get_mut(&cid) {
                        typers.retain(|t| *t != name);
                    }
                    let is_current = inner
                        .current_chat_id
                        .borrow()
                        .as_deref()
                        .map(|id| id == cid)
                        .unwrap_or(false);
                    if is_current {
                        let remaining = all.get(&cid).cloned().unwrap_or_default();
                        drop(all);
                        if remaining.is_empty() {
                            inner.typing_box.set_visible(false);
                        } else {
                            let label = remaining.join(", ");
                            inner
                                .typing_name
                                .set_markup(&format!("<small><b>{label}</b> </small>"));
                        }
                    }
                }
            });
        }
    }
}

/// Enable the send button only when there's actually something to send:
/// non-whitespace text in the compose buffer, or a staged image/GIF
/// attachment. Called from buffer changes and whenever the pending
/// attachment is set/cleared.
fn update_send_button_state(inner: &Rc<ChatViewInner>) {
    let buf = inner.input_view.buffer();
    let has_text = !buf
        .text(&buf.start_iter(), &buf.end_iter(), false)
        .trim()
        .is_empty();
    let has_attachment =
        inner.pending_image_path.borrow().is_some() || inner.pending_gif_url.borrow().is_some();
    inner.send_button.set_sensitive(has_text || has_attachment);
}

/// Replace every occurrence of a mention literal (`@Name`) with `@Number`,
/// but only when the literal is not immediately followed by another name
/// character (letter/digit/underscore). This stops a shorter name from
/// matching inside a longer one and keeps the substitution word-boundary safe.
/// True if `needle` (an "@Name" literal) appears in `haystack` as a WHOLE
/// mention — i.e. the char immediately after it is not a name char. Mirrors the
/// word-boundary rule in `replace_mention_literal` so the keep-decision for a
/// mention doesn't spuriously match "@Ann" inside "@Anna".
fn mention_literal_present(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut rest = haystack;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        let next_is_name_char = after
            .chars()
            .next()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false);
        if !next_is_name_char {
            return true;
        }
        rest = after;
    }
    false
}

fn replace_mention_literal(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(pos) = rest.find(needle) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + needle.len()..];
        // Only treat this as a whole mention if the next char isn't a name char.
        let next_is_name_char = after
            .chars()
            .next()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false);
        if next_is_name_char {
            // Not a boundary — emit the matched literal verbatim and continue.
            out.push_str(&rest[pos..pos + needle.len()]);
        } else {
            out.push_str(replacement);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn gen_tmp_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    // Monotonic per-session counter appended to the full-nanosecond timestamp so
    // two calls in the same nanosecond (e.g. a synchronous multi-file drop loop)
    // never collide. A bare subsec_nanos value could repeat, dropping a bubble
    // and mis-keying its receipt.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("tmp-{nanos:x}-{seq:x}")
}

/// Max retries for a flaky clipboard read on COSMIC. Each retry waits
/// `60ms * (attempt+1)`, so total worst-case wait is ~360ms across 4
/// attempts — fast enough to feel instant, slow enough to ride out the
/// compositor dropping the first offer read.
const PASTE_MAX_RETRIES: u32 = 3;

/// Outcome of a `wl-paste` clipboard read.
enum WlPasteResult {
    /// Plain text — insert at cursor.
    Text(String),
    /// Image written to this tmp path — stage as pending attachment.
    ImageFile(String),
    /// wl-paste ran fine but the clipboard had nothing usable.
    Empty,
    /// wl-paste is not installed / failed to run — caller should fall
    /// back to the GTK-native clipboard path.
    Unavailable,
}

/// Read the Wayland clipboard by shelling out to `wl-paste` (wl-clipboard).
/// This uses the `wlr-data-control` protocol, which — unlike GTK4's
/// `wl_data_device` path — is focus-independent and works reliably under
/// cosmic-comp. Blocking; MUST be called off the GTK main thread.
fn run_wl_paste() -> WlPasteResult {
    use std::process::Command;
    // 1. Enumerate offered MIME types.
    let types_out = Command::new("wl-paste").arg("--list-types").output();
    let types = match types_out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => {
            // Non-zero exit usually means "clipboard empty".
            log::info!(
                "Paste: wl-paste --list-types exited {} (clipboard likely empty)",
                o.status
            );
            return WlPasteResult::Empty;
        }
        Err(e) => {
            log::warn!("Paste: wl-paste not runnable ({e}) — falling back to GTK");
            return WlPasteResult::Unavailable;
        }
    };
    let type_set: Vec<&str> = types.lines().map(|l| l.trim()).collect();
    log::info!("Paste: wl-paste offered types = {type_set:?}");

    // 2. Prefer image when present. Match generously — different source
    //    apps advertise wildly different MIME sets. Some only offer
    //    common variants (image/png), others toss in less standard ones
    //    (image/x-MS-bmp, image/heif, application/x-qt-image with the
    //    bytes actually being PNG). Rule: take the FIRST preferred
    //    well-supported format if present, else any `image/*` MIME
    //    (skipping svg+xml — that's vector XML, not a bitmap we can
    //    send), else fall back to the Qt-internal blob if that's all
    //    the source offers (its bytes are usually a real PNG).
    const PREFERRED_IMAGE_MIMES: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/webp",
        "image/gif",
        "image/bmp",
        "image/x-MS-bmp",
        "image/heif",
        "image/heic",
        "image/tiff",
    ];
    let chosen_mime: Option<String> = PREFERRED_IMAGE_MIMES
        .iter()
        .find(|m| type_set.contains(m))
        .map(|m| (*m).to_string())
        .or_else(|| {
            type_set
                .iter()
                .find(|t| t.starts_with("image/") && **t != "image/svg+xml")
                .map(|t| (*t).to_string())
        })
        .or_else(|| {
            type_set
                .iter()
                .find(|t| **t == "application/x-qt-image")
                .map(|t| (*t).to_string())
        });

    if let Some(mime) = chosen_mime {
        log::info!("Paste: choosing image MIME {mime}");
        // Filename extension hint. Don't worry about being wrong — the
        // send path inspects the bytes for the real format.
        let ext = match mime.as_str() {
            "image/png" | "application/x-qt-image" => "png",
            "image/jpeg" | "image/jpg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/bmp" | "image/x-MS-bmp" => "bmp",
            "image/heif" | "image/heic" => "heic",
            "image/tiff" => "tiff",
            other => other.rsplit('/').next().unwrap_or("bin"),
        };
        match Command::new("wl-paste").arg("--type").arg(&mime).output() {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                let path = format!(
                    "/tmp/wa_paste_{}.{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                    ext
                );
                match std::fs::write(&path, &o.stdout) {
                    Ok(_) => {
                        log::info!(
                            "Paste: staged image via wl-paste ({} bytes): {path}",
                            o.stdout.len()
                        );
                        return WlPasteResult::ImageFile(path);
                    }
                    Err(e) => {
                        log::warn!("Paste: writing pasted image failed: {e}");
                        return WlPasteResult::Empty;
                    }
                }
            }
            Ok(o) => {
                log::warn!(
                    "Paste: wl-paste image read returned exit={} stdout_len={}",
                    o.status,
                    o.stdout.len()
                );
                return WlPasteResult::Empty;
            }
            Err(e) => {
                log::warn!("Paste: wl-paste image read failed: {e}");
                return WlPasteResult::Empty;
            }
        }
    }

    // 3. Otherwise read text. `--no-newline` drops the trailing newline
    //    wl-paste would otherwise append.
    let has_text = type_set.iter().any(|t| {
        *t == "text/plain;charset=utf-8"
            || *t == "text/plain"
            || *t == "UTF8_STRING"
            || *t == "STRING"
            || *t == "TEXT"
    });
    if has_text {
        match Command::new("wl-paste")
            .arg("--no-newline")
            .arg("--type")
            .arg("text/plain;charset=utf-8")
            .output()
        {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.is_empty() {
                    return WlPasteResult::Empty;
                }
                return WlPasteResult::Text(text);
            }
            // Fall through to a typeless read if the explicit type failed.
            _ => {
                if let Ok(o) = Command::new("wl-paste").arg("--no-newline").output()
                    && o.status.success()
                {
                    let text = String::from_utf8_lossy(&o.stdout).to_string();
                    if !text.is_empty() {
                        return WlPasteResult::Text(text);
                    }
                }
            }
        }
    }
    WlPasteResult::Empty
}

/// Ctrl+V entry point. Runs `wl-paste` on a worker thread, then applies
/// the result on the GTK main loop. Falls back to the GTK-native
/// clipboard path if wl-clipboard isn't installed.
fn paste_via_wl_clipboard(iv: gtk4::TextView, inner: Rc<ChatViewInner>) {
    let (tx, rx) = async_channel::bounded::<WlPasteResult>(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(run_wl_paste());
    });
    glib::MainContext::default().spawn_local(async move {
        let Ok(result) = rx.recv().await else {
            return;
        };
        match result {
            WlPasteResult::Text(text) => {
                let buffer = iv.buffer();
                if let Some((mut s, mut e)) = buffer.selection_bounds() {
                    buffer.delete(&mut s, &mut e);
                }
                buffer.insert_at_cursor(&text);
                log::info!("Paste: inserted {} chars via wl-paste", text.len());
            }
            WlPasteResult::ImageFile(path) => {
                log::info!("Paste: staged image via wl-paste: {path}");
                set_pending_attachment(&inner, &path);
            }
            WlPasteResult::Empty => {
                log::info!("Paste: wl-paste — clipboard had nothing usable");
            }
            WlPasteResult::Unavailable => {
                // wl-clipboard missing — use the GTK-native path.
                log::info!("Paste: falling back to GTK-native clipboard read");
                if let Some(display) = gtk4::gdk::Display::default() {
                    let clipboard = display.clipboard();
                    let formats = clipboard.formats();
                    let has_image = formats.contain_mime_type("image/png")
                        || formats.contain_mime_type("image/jpeg")
                        || formats.contain_mime_type("image/gif")
                        || formats.contain_mime_type("image/webp")
                        || formats.contain_mime_type("image/bmp");
                    if has_image {
                        paste_clipboard_image(clipboard, inner, 0);
                    } else {
                        paste_clipboard_text(clipboard, iv, inner, 0);
                    }
                }
            }
        }
    });
}

/// Read clipboard TEXT and insert it at the input cursor (replacing any
/// selection — standard paste semantics). Retries on empty/error because
/// COSMIC's clipboard intermittently drops the first read of an offer.
///
/// On total failure for text, falls back ONCE to an image read — handles
/// the case where `formats()` returned a stale/empty view and the
/// clipboard actually held an image.
fn paste_clipboard_text(
    clipboard: gtk4::gdk::Clipboard,
    iv: gtk4::TextView,
    inner: Rc<ChatViewInner>,
    attempt: u32,
) {
    let cb_retry = clipboard.clone();
    clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |res| {
        match res {
            Ok(Some(text)) if !text.is_empty() => {
                let buffer = iv.buffer();
                if let Some((mut s, mut e)) = buffer.selection_bounds() {
                    buffer.delete(&mut s, &mut e);
                }
                buffer.insert_at_cursor(text.as_str());
                log::info!("Paste: inserted {} chars (attempt {attempt})", text.len());
            }
            _ if attempt < PASTE_MAX_RETRIES => {
                log::info!("Paste: text read empty/failed (attempt {attempt}) — retrying");
                let delay = std::time::Duration::from_millis(60 * (attempt as u64 + 1));
                gtk4::glib::timeout_add_local_once(delay, move || {
                    paste_clipboard_text(cb_retry, iv, inner, attempt + 1);
                });
            }
            _ => {
                // Text genuinely failed after all retries. Last resort:
                // the clipboard may actually hold an image that
                // formats() didn't report. Try one image read.
                log::warn!("Paste: text read exhausted retries — trying image as last resort");
                paste_clipboard_image(cb_retry, inner, 0);
            }
        }
    });
}

/// Read clipboard IMAGE content and stage it as a pending attachment.
/// Tries a GDK texture read first, then a raw-bytes read for MIME types
/// GDK can't decode directly. Retries the whole thing on COSMIC's
/// intermittent dropped reads.
fn paste_clipboard_image(clipboard: gtk4::gdk::Clipboard, inner: Rc<ChatViewInner>, attempt: u32) {
    let cb_retry = clipboard.clone();
    clipboard.read_texture_async(None::<&gtk4::gio::Cancellable>, move |result| {
        match result {
            Ok(Some(texture)) => {
                let tmp_path = format!(
                    "/tmp/wa_paste_{}.png",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                );
                match texture.save_to_png(&tmp_path) {
                    Ok(_) => {
                        log::info!("Paste: image staged from texture (attempt {attempt})");
                        set_pending_attachment(&inner, &tmp_path);
                    }
                    Err(e) => log::warn!("Paste: save_to_png failed: {e}"),
                }
            }
            Ok(None) | Err(_) => {
                // Texture decode unavailable — fall back to a raw-bytes
                // read of the common image MIME types.
                use gtk4::gio::prelude::*;
                let inner_fb = inner.clone();
                let cb_for_bytes = cb_retry.clone();
                cb_retry.read_async(
                    &["image/png", "image/jpeg", "image/webp", "image/gif"],
                    gtk4::glib::Priority::DEFAULT,
                    None::<&gtk4::gio::Cancellable>,
                    move |res| {
                        let Ok((stream, mime)) = res else {
                            // Raw read also failed — retry the whole
                            // image paste if we have attempts left.
                            if attempt < PASTE_MAX_RETRIES {
                                log::info!(
                                    "Paste: image read failed (attempt {attempt}) — retrying"
                                );
                                let delay =
                                    std::time::Duration::from_millis(60 * (attempt as u64 + 1));
                                gtk4::glib::timeout_add_local_once(delay, move || {
                                    paste_clipboard_image(cb_for_bytes, inner_fb, attempt + 1);
                                });
                            } else {
                                log::warn!("Paste: image read exhausted retries");
                            }
                            return;
                        };
                        let ext = mime.split('/').nth(1).unwrap_or("png");
                        let tmp_path = format!(
                            "/tmp/wa_paste_{}.{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis(),
                            ext
                        );
                        let inner_done = inner_fb.clone();
                        stream.read_bytes_async(
                            10 * 1024 * 1024,
                            gtk4::glib::Priority::DEFAULT,
                            None::<&gtk4::gio::Cancellable>,
                            move |read_res| match read_res {
                                Ok(bytes) => {
                                    if let Err(e) = std::fs::write(&tmp_path, &bytes) {
                                        log::warn!("Paste: write failed: {e}");
                                        return;
                                    }
                                    log::info!("Paste: image staged from raw {mime}");
                                    set_pending_attachment(&inner_done, &tmp_path);
                                }
                                Err(e) => {
                                    log::warn!("Paste: read_bytes failed: {e}")
                                }
                            },
                        );
                    },
                );
            }
        }
    });
}

/// Stage a single file as the pending attachment in the preview bar.
/// Shared by drag-drop and file-chooser paths so both render the same
/// preview UI (image thumbnail OR filename + icon) and clear stale
/// state consistently.
fn set_pending_attachment(inner: &Rc<ChatViewInner>, path_str: &str) {
    let lower = path_str.to_lowercase();
    let is_image = lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".webp")
        || lower.ends_with(".gif");
    let is_pdf = lower.ends_with(".pdf");

    // Always clear the old paintable first so a stale paste-screenshot
    // can't remain visible behind a non-image attachment.
    inner
        .image_preview_pic
        .set_paintable(None::<&gtk4::gdk::Paintable>);

    let filename = std::path::Path::new(path_str)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path_str)
        .to_string();

    if is_image {
        if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(path_str, 720) {
            inner.image_preview_pic.set_paintable(Some(&tex));
        }
        inner
            .preview_label
            .set_markup("Press Enter to send, Escape to cancel");
    } else if is_pdf {
        // Render the first page via pdftocairo (same approach the inline
        // bubble uses for received PDFs), but never wait for the external
        // process on GTK's main thread. The filename appears immediately and
        // the thumbnail is filled in when ready.
        let escaped = gtk4::glib::markup_escape_text(&filename);
        inner.preview_label.set_markup(&format!(
            "<b>📄 {escaped}</b>\n<small>Press Enter to send, Escape to cancel</small>"
        ));
        let inner_w = Rc::downgrade(inner);
        let staged_path = path_str.to_string();
        crate::ui::message_bubble::render_pdf_thumbnail_async(path_str, move |thumb_path| {
            let Some(inner) = inner_w.upgrade() else {
                return;
            };
            // The user may have selected a different attachment while the PDF
            // was rendering. Never paint an old thumbnail over the new preview.
            if inner.pending_image_path.borrow().as_deref() != Some(&staged_path) {
                return;
            }
            if let Some(thumb_path) = thumb_path
                && let Some(tex) = crate::ui::texture_cache::texture_thumbnail(&thumb_path, 480)
            {
                inner.image_preview_pic.set_paintable(Some(&tex));
            }
        });
    } else {
        let icon = if lower.ends_with(".pdf") {
            "📄"
        } else if lower.ends_with(".doc") || lower.ends_with(".docx") {
            "📝"
        } else if lower.ends_with(".xls") || lower.ends_with(".xlsx") || lower.ends_with(".csv") {
            "📊"
        } else if lower.ends_with(".ppt") || lower.ends_with(".pptx") {
            "📽"
        } else if lower.ends_with(".zip")
            || lower.ends_with(".rar")
            || lower.ends_with(".7z")
            || lower.ends_with(".tar")
            || lower.ends_with(".gz")
        {
            "🗜"
        } else if lower.ends_with(".mp4")
            || lower.ends_with(".mov")
            || lower.ends_with(".avi")
            || lower.ends_with(".mkv")
        {
            "🎬"
        } else if lower.ends_with(".mp3")
            || lower.ends_with(".m4a")
            || lower.ends_with(".wav")
            || lower.ends_with(".ogg")
        {
            "🎵"
        } else {
            "📎"
        };
        let escaped = gtk4::glib::markup_escape_text(&filename);
        inner.preview_label.set_markup(&format!(
            "<b>{icon} {escaped}</b>\n<small>Press Enter to send, Escape to cancel</small>"
        ));
    }
    *inner.pending_image_path.borrow_mut() = Some(path_str.to_string());
    *inner.pending_gif_url.borrow_mut() = None;
    *inner.pending_gif_preview_url.borrow_mut() = None;
    inner.image_preview_bar.set_visible(true);
    // A staged attachment is sendable even with empty text.
    update_send_button_state(inner);
    inner.input_view.grab_focus();
    // Mirror to the per-chat record so the attachment survives chat
    // switches (back-and-forth restores the same file in preview).
    // The same record is removed on send / cancel.
    if let Some(cid) = inner.current_chat_id.borrow().clone() {
        inner
            .pending_attachments
            .borrow_mut()
            .insert(cid, PendingAttachment::File(path_str.to_string()));
    }
}

fn maybe_insert_date_separator(inner: &Rc<ChatViewInner>, timestamp: i64) {
    use chrono::{DateTime, Local, Utc};
    let dt: DateTime<Local> =
        DateTime::from(DateTime::<Utc>::from_timestamp(timestamp, 0).unwrap_or_default());
    let date = dt.date_naive();
    let mut last = inner.last_msg_date.borrow_mut();
    if last.as_ref() == Some(&date) {
        return;
    }
    *last = Some(date);
    drop(last);

    let now = Local::now().date_naive();
    let label_text = if date == now {
        "Today".to_string()
    } else if (now - date).num_days() == 1 {
        "Yesterday".to_string()
    } else if (now - date).num_days() < 7 {
        dt.format("%A").to_string() // Monday, Tuesday, …
    } else {
        dt.format("%B %-d, %Y").to_string() // March 15, 2025
    };

    let sep = Label::new(Some(&label_text));
    sep.add_css_class("date-separator");
    inner.messages_box.append(&sep);
}

/// Show the full dropdown context menu for a message (triggered by chevron or right-click).
fn show_message_menu(
    inner: &Rc<ChatViewInner>,
    msg: &IncomingMessage,
    anchor: &gtk4::Widget,
    x: f64,
    y: f64,
) {
    // The menu closures captured this message at bubble-creation time, when a
    // just-sent message was still keyed by its optimistic "tmp-…" id. Resolve to
    // the real server id (recorded by confirm_bubble) so every menu action —
    // edit, star, pin, react, delete, reply-quote — targets the message the
    // server actually knows, not the dead optimistic id.
    let mut resolved = msg.clone();
    if let Some(real) = inner.id_remap.borrow().get(&resolved.id) {
        resolved.id = real.clone();
    }
    let msg = &resolved;

    let is_failed = inner
        .bubbles
        .borrow()
        .get(&msg.id)
        .map(|b| b.is_failed())
        .unwrap_or(false);

    let popover = gtk4::Popover::new();
    popover.set_parent(anchor);
    popover.add_css_class("menu");
    popover.set_has_arrow(false);
    if x > 0.0 || y > 0.0 {
        let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
        popover.set_pointing_to(Some(&rect));
    }

    let vbox = Box::new(Orientation::Vertical, 0);
    vbox.set_width_request(200);

    macro_rules! menu_btn {
        ($label:expr) => {{
            let btn = Button::with_label($label);
            btn.set_has_frame(false);
            btn.add_css_class("flat");
            if let Some(child) = btn.child() {
                if let Ok(lbl) = child.downcast::<Label>() {
                    lbl.set_halign(Align::Start);
                    lbl.set_margin_start(4);
                }
            }
            btn
        }};
    }

    if is_failed {
        // ResendMessage can reconstruct text only. Offering it for an
        // optimistic media bubble (which deliberately stores no remote URL or
        // local payload) sent an empty text message instead of the GIF/sticker.
        let msg_id = msg.id.clone();
        let resend_text = (msg.media_type.is_none())
            .then(|| {
                inner
                    .bubbles
                    .borrow()
                    .get(&msg_id)
                    .and_then(|bubble| bubble.text.clone())
            })
            .flatten()
            .filter(|text| !text.trim().is_empty());
        if let Some(text) = resend_text {
            let btn = menu_btn!("Resend");
            let inner_c = inner.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                    inner_c.bridge.send_command(WaCommand::ResendMessage {
                        chat_id: cid,
                        msg_id: msg_id.clone(),
                        text: text.clone(),
                    });
                }
                pop.popdown();
            });
            vbox.append(&btn);
        } else {
            let unavailable = menu_btn!("Resend unavailable");
            unavailable.set_sensitive(false);
            unavailable.set_tooltip_text(Some("Choose the media again to retry"));
            vbox.append(&unavailable);
        }
    } else {
        // Reply
        let btn = menu_btn!("Reply");
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            do_reply(&inner_c, &msg_c);
            pop.popdown();
        });
        vbox.append(&btn);

        // Group-only: Reply privately + Message user
        let is_group = inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id.ends_with("@g.us"))
            .unwrap_or(false);
        if is_group && !msg.is_from_me && !msg.sender_id.is_empty() {
            // Reply privately — opens DM with quoted message
            // Reply privately — open DM with reply context
            let btn = menu_btn!("Reply privately");
            let inner_c = inner.clone();
            let msg_c = msg.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                // Resolve LID → canonical phone JID. Without this we'd open
                // the DM under the participant's LID, which becomes a
                // SECOND chat row separate from any existing thread under
                // their phone JID. Server fanout of the outgoing message
                // lands in the phone-JID chat anyway — so the LID row
                // ends up half-populated and the user sees the same
                // person under two rows. Symptom reported as
                // "from a group chat I click message privately... it made
                // 2 chats in 1".
                let raw_jid = msg_c.sender_id.clone();
                let dm_jid =
                    crate::ui::runtime::lid_to_canonical_phone_jid(&raw_jid).unwrap_or(raw_jid);
                let sender_name = if msg_c.sender_name.is_empty() {
                    crate::ui::runtime::display_name_from_jid(&dm_jid)
                } else {
                    msg_c.sender_name.clone()
                };
                // Fully switch to the DM chat (clear old messages, set new ID)
                *inner_c.current_chat_id.borrow_mut() = Some(dm_jid.clone());
                inner_c.header_name.set_text(&sender_name);
                // Clear the group-participant subtitle that was left over
                // from the previous chat — otherwise the private DM shows
                // the original group's member list under the contact name.
                inner_c.header_subtitle.set_text("");
                inner_c.header_subtitle.set_visible(false);
                remove_all_children(&inner_c.messages_box);
                inner_c.bubbles.borrow_mut().clear();
                inner_c.search_texts.borrow_mut().clear();
                inner_c.media_items.borrow_mut().clear();
                *inner_c.last_msg_date.borrow_mut() = None;
                // Set reply context with the original group message
                let text_preview = msg_c
                    .text
                    .clone()
                    .or(msg_c.media_caption.clone())
                    .unwrap_or_else(|| match &msg_c.media_type {
                        Some(crate::bridge::MediaType::Image) => "📷 Photo".to_string(),
                        Some(crate::bridge::MediaType::Video) => "🎥 Video".to_string(),
                        Some(crate::bridge::MediaType::Audio) => "🎵 Audio".to_string(),
                        Some(crate::bridge::MediaType::Document) => "📄 Document".to_string(),
                        Some(crate::bridge::MediaType::Sticker) => "🎭 Sticker".to_string(),
                        Some(crate::bridge::MediaType::Gif) => "🎞 GIF".to_string(),
                        None => "📎 Message".to_string(),
                    });
                inner_c
                    .reply_label
                    .set_text(&format!("{}: {}", sender_name, text_preview));
                inner_c.reply_bar.set_visible(true);
                *inner_c.reply_context.borrow_mut() = Some((
                    msg_c.id.clone(),
                    msg_c.sender_id.clone(),
                    text_preview,
                    msg_c.media_local_path.clone(),
                ));
                inner_c.input_view.grab_focus();
                inner_c.bridge.send_command(WaCommand::StartNewChat {
                    jid: dm_jid.clone(),
                });
                inner_c.bridge.send_command(WaCommand::LoadChat {
                    chat_id: dm_jid,
                    chat_name: sender_name,
                });
                inner_c.input_view.grab_focus();
                pop.popdown();
            });
            vbox.append(&btn);

            // Message user — open DM
            let sender_display = if msg.sender_name.is_empty() {
                crate::ui::runtime::display_name_from_jid(&msg.sender_id)
            } else {
                msg.sender_name.clone()
            };
            let btn = menu_btn!(&format!("Message {sender_display}"));
            let inner_c = inner.clone();
            let sender_id = msg.sender_id.clone();
            let sender_name_c = sender_display.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                // Same LID → phone JID resolution as "Reply privately"
                // above. Prevents the duplicate-chat-row bug.
                let sid = crate::ui::runtime::lid_to_canonical_phone_jid(&sender_id)
                    .unwrap_or_else(|| sender_id.clone());
                *inner_c.current_chat_id.borrow_mut() = Some(sid.clone());
                inner_c.header_name.set_text(&sender_name_c);
                // Clear leftover group-participant subtitle from prev chat.
                inner_c.header_subtitle.set_text("");
                inner_c.header_subtitle.set_visible(false);
                remove_all_children(&inner_c.messages_box);
                inner_c.bubbles.borrow_mut().clear();
                inner_c
                    .bridge
                    .send_command(WaCommand::StartNewChat { jid: sid.clone() });
                inner_c.bridge.send_command(WaCommand::LoadChat {
                    chat_id: sid,
                    chat_name: sender_name_c.clone(),
                });
                inner_c.input_view.grab_focus();
                pop.popdown();
            });
            vbox.append(&btn);
        }

        // Copy — fall back to the media caption so captioned image/video
        // messages (whose text lives in media_caption) still get a Copy item.
        if let Some(text) = msg.text.as_ref().or(msg.media_caption.as_ref()) {
            let btn = menu_btn!("Copy");
            let text_c = text.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                if let Some(display) = gtk4::gdk::Display::default() {
                    display.clipboard().set_text(&text_c);
                }
                pop.popdown();
            });
            vbox.append(&btn);
        }

        // Save any downloaded media — voice notes, images, video, documents —
        // to ~/Downloads under a clean name. Before this, only the image viewer
        // had a save affordance, so audio/documents were unreachable without
        // digging into ~/.local/share/whatsapp-desktop/wa_media.
        if let Some(path) = msg
            .media_local_path
            .as_ref()
            .filter(|p| std::path::Path::new(p.as_str()).exists())
        {
            let btn = menu_btn!("Save to Downloads");
            let path_c = path.clone();
            let inner_c = inner.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                let saved = crate::ui::message_bubble::save_to_downloads(
                    std::path::Path::new(&path_c),
                );
                if let Some(overlay) = ChatViewPanel::toast_overlay(&inner_c) {
                    let text = match &saved {
                        Some(p) => format!("Saved to {}", p.display()),
                        None => "Could not save file".to_string(),
                    };
                    overlay.add_toast(adw::Toast::new(&text));
                }
                pop.popdown();
            });
            vbox.append(&btn);
        }

        // React
        let btn = menu_btn!("React");
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |btn| {
            pop.popdown();
            show_react_picker(&inner_c, &msg_c, btn.upcast_ref());
        });
        vbox.append(&btn);

        // Forward
        let is_poll = msg.poll_question.is_some() && !msg.poll_options.is_empty();
        if is_poll {
            // Poll forward: pick chats, send immediately (no edit)
            let btn = menu_btn!("Forward poll");
            let inner_c = inner.clone();
            let pop = popover.clone();
            let poll_q = msg.poll_question.clone().unwrap_or_default();
            let poll_opts = msg.poll_options.clone();
            let poll_sel = if msg.poll_selectable == 0 {
                1
            } else {
                msg.poll_selectable
            };
            btn.connect_clicked(move |_| {
                pop.popdown();
                let parent = inner_c
                    .root
                    .root()
                    .and_then(|r| r.downcast::<gtk4::Window>().ok());
                let bridge = inner_c.bridge.clone();
                let q = poll_q.clone();
                let opts = poll_opts.clone();
                crate::ui::chat_picker::show_chat_picker(
                    "Send poll to…",
                    true,
                    parent.as_ref(),
                    move |selected| {
                        for to_chat_id in &selected {
                            bridge.send_command(crate::bridge::WaCommand::SendPoll {
                                chat_id: to_chat_id.clone(),
                                question: q.clone(),
                                options: opts.clone(),
                                selectable_count: poll_sel,
                            });
                        }
                    },
                );
            });
            vbox.append(&btn);

            // Poll forward & edit — open creator first, then pick chats
            let btn = menu_btn!("Forward & edit poll");
            let inner_c = inner.clone();
            let pop = popover.clone();
            let poll_q = msg.poll_question.clone().unwrap_or_default();
            let poll_opts = msg.poll_options.clone();
            let poll_sel = if msg.poll_selectable == 0 {
                1
            } else {
                msg.poll_selectable
            };
            btn.connect_clicked(move |_| {
                pop.popdown();
                show_poll_creator_for_forward(&inner_c, &poll_q, &poll_opts, poll_sel);
            });
            vbox.append(&btn);
        } else {
            let btn = menu_btn!("Forward");
            let inner_c = inner.clone();
            let msg_id = msg.id.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                ChatViewPanel::enter_forward_mode(&inner_c, &msg_id);
                pop.popdown();
            });
            vbox.append(&btn);
        }

        vbox.append(&Separator::new(Orientation::Horizontal));

        // Pin / Unpin
        // Pin with duration submenu
        let pin_box = Box::new(Orientation::Vertical, 0);
        let pin_header = menu_btn!("📌 Pin message…");
        pin_header.set_sensitive(false);
        pin_box.append(&pin_header);
        for (label, _duration_hint) in [("24 hours", "24h"), ("7 days", "7d"), ("30 days", "30d")] {
            let btn = menu_btn!(&format!("    {label}"));
            let inner_c = inner.clone();
            let msg_id = msg.id.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                pop.popdown();
                if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                    inner_c.bridge.send_command(WaCommand::PinMessage {
                        chat_id: cid,
                        msg_id: msg_id.clone(),
                    });
                }
            });
            pin_box.append(&btn);
        }
        vbox.append(&pin_box);

        // Star / Unstar — toggle based on this session's known star state.
        let currently_starred = inner.starred_msgs.borrow().contains(&msg.id);
        let btn = menu_btn!(if currently_starred { "Unstar" } else { "Star" });
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                // Flip the desired state and record it so a re-open of the
                // menu offers the opposite action.
                let new_starred = !inner_c.starred_msgs.borrow().contains(&msg_c.id);
                if new_starred {
                    inner_c.starred_msgs.borrow_mut().insert(msg_c.id.clone());
                } else {
                    inner_c.starred_msgs.borrow_mut().remove(&msg_c.id);
                }
                inner_c.bridge.send_command(WaCommand::StarMessage {
                    chat_id: cid,
                    msg_id: msg_c.id.clone(),
                    starred: new_starred,
                    sender_jid: msg_c.sender_id.clone(),
                    is_from_me: msg_c.is_from_me,
                });
            }
            pop.popdown();
        });
        vbox.append(&btn);

        // Edit (own text messages only, and only within WhatsApp's ~15-minute
        // edit window — offering Edit on older messages just leads users into
        // a server-side rejection).
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        const EDIT_WINDOW_SECS: i64 = 15 * 60;
        let within_edit_window = now_secs - msg.timestamp <= EDIT_WINDOW_SECS;
        if msg.is_from_me && msg.text.is_some() && msg.media_type.is_none() && within_edit_window {
            let btn = menu_btn!("Edit");
            let inner_c = inner.clone();
            let msg_c = msg.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                pop.popdown();
                // Pre-fill input with current text and set edit mode
                if let Some(text) = &msg_c.text {
                    // Programmatic prefill — suppress the outbound typing
                    // indicator so opening "Edit" doesn't broadcast "typing…".
                    ChatViewPanel::set_composer_text_silent(&inner_c, text);
                    // Store edit state: (chat_id, msg_id)
                    *inner_c.editing_msg.borrow_mut() =
                        Some((msg_c.chat_id.clone(), msg_c.id.clone()));
                    // Show "Editing" banner
                    inner_c.edit_banner.set_reveal_child(true);
                    inner_c.input_view.grab_focus();
                }
            });
            vbox.append(&btn);
        }

        // Save as quick reply
        if let Some(text) = &msg.text {
            let btn = menu_btn!("Save as quick reply");
            let text_c = text.clone();
            let bridge = inner.bridge.clone();
            let pop = popover.clone();
            let inner_c = inner.clone();
            btn.connect_clicked(move |_| {
                pop.popdown();
                // Styled adw::AlertDialog (Cancel + Save, Escape closes, Enter
                // in the entry submits) — matches the app's dialog convention
                // instead of the previous bare, non-transient gtk4::Window.
                let dialog = adw::AlertDialog::builder()
                    .heading("Save quick reply")
                    .body("Choose a shortcut for this message (used as /shortcut).")
                    .build();
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("save", "Save");
                dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("save"));
                dialog.set_close_response("cancel");

                let entry = gtk4::Entry::new();
                entry.set_placeholder_text(Some("e.g. greeting"));
                entry.set_margin_top(8);
                entry.set_activates_default(true);
                dialog.set_extra_child(Some(&entry));

                // Save is meaningless with an empty shortcut — keep it disabled
                // until the entry has content so a stray Save doesn't silently
                // eat the dialog with nothing saved.
                dialog.set_response_enabled("save", false);
                {
                    let dialog_e = dialog.clone();
                    entry.connect_changed(move |e| {
                        dialog_e.set_response_enabled("save", !e.text().trim().is_empty());
                    });
                }

                let parent_window = inner_c
                    .root
                    .root()
                    .and_then(|r| r.downcast::<gtk4::Window>().ok());
                dialog.present(parent_window.as_ref());

                let b = bridge.clone();
                let t = text_c.clone();
                dialog.connect_response(None, move |_, response| {
                    if response != "save" {
                        return;
                    }
                    let shortcut = entry.text().to_string().trim().to_string();
                    if shortcut.is_empty() {
                        return;
                    }
                    b.send_command(WaCommand::SaveQuickReply {
                        shortcut,
                        message: t.clone(),
                    });
                });
            });
            vbox.append(&btn);
        }

        // Save to notes
        if let Some(text) = &msg.text {
            let btn = menu_btn!("Add to notes");
            let text_c = text.clone();
            let bridge = inner.bridge.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                bridge.send_command(WaCommand::SaveNote {
                    text: text_c.clone(),
                });
                pop.popdown();
            });
            vbox.append(&btn);
        }

        vbox.append(&Separator::new(Orientation::Horizontal));

        // Delete for me
        let btn = menu_btn!("Delete for me");
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                inner_c.bridge.send_command(WaCommand::DeleteForMe {
                    chat_id: cid,
                    msg_id: msg_c.id.clone(),
                    sender_jid: msg_c.sender_id.clone(),
                    is_from_me: msg_c.is_from_me,
                });
            }
            pop.popdown();
        });
        vbox.append(&btn);

        // Delete for everyone (own messages only)
        if msg.is_from_me {
            let btn = menu_btn!("Delete for everyone");
            let inner_c = inner.clone();
            let msg_id = msg.id.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                    inner_c.bridge.send_command(WaCommand::DeleteForEveryone {
                        chat_id: cid,
                        msg_id: msg_id.clone(),
                    });
                }
                pop.popdown();
            });
            vbox.append(&btn);
        }
    }

    popover.set_child(Some(&vbox));
    popover.popup();
}

/// Simple event composer: collects name / date / time / location, formats a
/// tidy event message, drops it into the compose box and sends it through the
/// normal send path (optimistic bubble + channel routing).
fn show_event_creator(inner: &Rc<ChatViewInner>) {
    use gtk4::{Align, Entry, Label, Orientation};

    if inner.current_chat_id.borrow().is_none() {
        return;
    }

    let window = gtk4::Window::builder()
        .title("Create Event")
        .default_width(400)
        .modal(true)
        .build();
    if let Some(root) = inner
        .root
        .root()
        .and_then(|r| r.downcast::<gtk4::Window>().ok())
    {
        window.set_transient_for(Some(&root));
    }

    let content = Box::new(Orientation::Vertical, 12);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(16);
    content.set_margin_bottom(16);

    let heading = Label::new(Some("New event"));
    heading.add_css_class("title-3");
    heading.set_halign(Align::Start);
    content.append(&heading);

    let name = Entry::builder().placeholder_text("Event name").build();
    let date = Entry::builder()
        .placeholder_text("Date (e.g. Sat 12 Jul)")
        .build();
    let time = Entry::builder()
        .placeholder_text("Time (e.g. 7:00 PM)")
        .build();
    let location = Entry::builder()
        .placeholder_text("Location (optional)")
        .build();
    content.append(&name);
    content.append(&date);
    content.append(&time);
    content.append(&location);

    let btn_row = Box::new(Orientation::Horizontal, 8);
    btn_row.set_halign(Align::End);
    let cancel = gtk4::Button::with_label("Cancel");
    let create = gtk4::Button::with_label("Create");
    create.add_css_class("suggested-action");
    btn_row.append(&cancel);
    btn_row.append(&create);
    content.append(&btn_row);
    window.set_child(Some(&content));

    // Weak window refs in the closures: a strong clone captured by a signal
    // handler ON the window forms a reference cycle that leaks the whole event
    // dialog on every open. Downgrade + upgrade-on-use breaks it.
    let win_cancel = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(w) = win_cancel.upgrade() {
            w.close();
        }
    });

    // Escape closes.
    let key = gtk4::EventControllerKey::new();
    let win_key = window.downgrade();
    key.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk4::gdk::Key::Escape {
            if let Some(w) = win_key.upgrade() {
                w.close();
            }
            gtk4::glib::Propagation::Stop
        } else {
            gtk4::glib::Propagation::Proceed
        }
    });
    window.add_controller(key);

    let inner_c = inner.clone();
    let win_create = window.downgrade();
    create.connect_clicked(move |_| {
        let n = name.text().trim().to_string();
        if n.is_empty() {
            name.grab_focus();
            return;
        }
        let d = date.text().trim().to_string();
        let t = time.text().trim().to_string();
        let l = location.text().trim().to_string();
        let mut msg = format!("📅 Event: {n}");
        let when = format!("{d} {t}");
        let when = when.trim();
        if !when.is_empty() {
            msg.push_str(&format!("\n🗓 {when}"));
        }
        if !l.is_empty() {
            msg.push_str(&format!("\n📍 {l}"));
        }
        // Send the event text directly through normal text routing WITHOUT
        // touching the composer buffer, editing_msg, pending attachment or
        // draft — so composing an event never captions a staged image, turns an
        // active edit into an edit-of-old-message, or destroys the user's draft.
        ChatViewPanel::send_plain_text(&inner_c, msg);
        if let Some(w) = win_create.upgrade() {
            w.close();
        }
    });

    window.present();
}

fn show_poll_creator(inner: &Rc<ChatViewInner>) {
    show_poll_creator_impl(inner, None, None, 1);
}

fn show_poll_creator_impl(
    inner: &Rc<ChatViewInner>,
    prefill_q: Option<&str>,
    prefill_opts: Option<&[String]>,
    prefill_sel: u32,
) {
    use gtk4::{Align, CheckButton, Entry, Label, Orientation};

    let chat_id = match inner.current_chat_id.borrow().clone() {
        Some(id) => id,
        None => return,
    };

    let window = gtk4::Window::builder()
        .title("Create Poll")
        .default_width(400)
        .default_height(500)
        .modal(true)
        .build();

    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_vexpand(true);

    let content = Box::new(Orientation::Vertical, 12);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(16);
    content.set_margin_bottom(16);

    // Question
    let q_label = Label::new(Some("Question"));
    q_label.add_css_class("heading");
    q_label.set_halign(Align::Start);
    content.append(&q_label);

    let question = Entry::new();
    question.set_placeholder_text(Some("Ask a question"));
    if let Some(q) = prefill_q {
        question.set_text(q);
    }
    content.append(&question);

    // Options
    let opts_label = Label::new(Some("Options"));
    opts_label.add_css_class("heading");
    opts_label.set_halign(Align::Start);
    content.append(&opts_label);

    let options_box = Box::new(Orientation::Vertical, 6);
    let initial_opts = if let Some(opts) = prefill_opts {
        opts.len().max(2)
    } else {
        2
    };

    // Helper: count children in options_box
    fn count_entries(container: &Box) -> usize {
        let mut n = 0;
        let mut child = container.first_child();
        while let Some(c) = child {
            n += 1;
            child = c.next_sibling();
        }
        n
    }

    // Helper: attach auto-add-row behavior to an entry
    fn wire_auto_add(entry: &Entry, opts_box: &Box) {
        let ob = opts_box.clone();
        entry.connect_changed(move |e| {
            if e.text().is_empty() {
                return;
            }
            // Check if this entry is the last child
            if e.next_sibling().is_none() {
                let count = count_entries(&ob);
                if count < 12 {
                    let new_entry = Entry::new();
                    new_entry.set_placeholder_text(Some(&format!("Option {}", count + 1)));
                    wire_auto_add(&new_entry, &ob);
                    ob.append(&new_entry);
                }
            }
        });
    }

    for i in 1..=initial_opts {
        let entry = Entry::new();
        entry.set_placeholder_text(Some(&format!("Option {i}")));
        if let Some(opts) = prefill_opts {
            if i <= opts.len() {
                entry.set_text(&opts[i - 1]);
            }
        }
        wire_auto_add(&entry, &options_box);
        options_box.append(&entry);
    }
    content.append(&options_box);

    // Allow multiple votes
    let multi_check = CheckButton::with_label("Allow multiple answers");
    if prefill_sel > 1 {
        multi_check.set_active(true);
    }
    content.append(&multi_check);

    // Create button
    let create_btn = Button::with_label("Send Poll");
    create_btn.add_css_class("suggested-action");
    create_btn.set_halign(Align::Center);
    create_btn.set_margin_top(12);

    let bridge = inner.bridge.clone();
    let q_c = question.clone();
    let opts_c = options_box.clone();
    let multi_c = multi_check.clone();
    let win_c = window.downgrade();
    create_btn.connect_clicked(move |_| {
        let q = q_c.text().to_string();
        if q.trim().is_empty() {
            return;
        }

        let mut options: Vec<String> = Vec::new();
        let mut child = opts_c.first_child();
        while let Some(c) = child {
            let next = c.next_sibling();
            if let Some(e) = c.downcast_ref::<Entry>() {
                let text = e.text().to_string();
                if !text.trim().is_empty() {
                    options.push(text.trim().to_string());
                }
            }
            child = next;
        }

        if options.len() < 2 {
            return;
        }

        let selectable = if multi_c.is_active() {
            options.len() as u32
        } else {
            1
        };

        bridge.send_command(crate::bridge::WaCommand::SendPoll {
            chat_id: chat_id.clone(),
            question: q,
            options,
            selectable_count: selectable,
        });
        if let Some(window) = win_c.upgrade() {
            window.close();
        }
    });
    content.append(&create_btn);

    scroll.set_child(Some(&content));
    window.set_child(Some(&scroll));
    window.present();
}

/// Poll creator that, on "Send", opens a multi-select chat picker and sends to all chosen chats.
fn show_poll_creator_for_forward(
    inner: &Rc<ChatViewInner>,
    question: &str,
    options: &[String],
    selectable: u32,
) {
    use gtk4::{Align, CheckButton, Entry, Label, Orientation};

    let window = gtk4::Window::builder()
        .title("Edit & Forward Poll")
        .default_width(400)
        .default_height(500)
        .modal(true)
        .build();

    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_vexpand(true);

    let content = Box::new(Orientation::Vertical, 12);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(16);
    content.set_margin_bottom(16);

    let q_label = Label::new(Some("Question"));
    q_label.add_css_class("heading");
    q_label.set_halign(Align::Start);
    content.append(&q_label);

    let question_entry = Entry::new();
    question_entry.set_placeholder_text(Some("Ask a question"));
    question_entry.set_text(question);
    content.append(&question_entry);

    let opts_label = Label::new(Some("Options"));
    opts_label.add_css_class("heading");
    opts_label.set_halign(Align::Start);
    content.append(&opts_label);

    let options_box = Box::new(Orientation::Vertical, 6);

    fn count_entries_fwd(container: &Box) -> usize {
        let mut n = 0;
        let mut child = container.first_child();
        while let Some(c) = child {
            n += 1;
            child = c.next_sibling();
        }
        n
    }
    fn wire_auto_add_fwd(entry: &Entry, opts_box: &Box) {
        let ob = opts_box.clone();
        entry.connect_changed(move |e| {
            if e.text().is_empty() {
                return;
            }
            if e.next_sibling().is_none() {
                let count = count_entries_fwd(&ob);
                if count < 12 {
                    let new_entry = Entry::new();
                    new_entry.set_placeholder_text(Some(&format!("Option {}", count + 1)));
                    wire_auto_add_fwd(&new_entry, &ob);
                    ob.append(&new_entry);
                }
            }
        });
    }

    for (i, opt) in options.iter().enumerate() {
        let entry = Entry::new();
        entry.set_placeholder_text(Some(&format!("Option {}", i + 1)));
        entry.set_text(opt);
        wire_auto_add_fwd(&entry, &options_box);
        options_box.append(&entry);
    }
    // Ensure at least 2 rows
    while count_entries_fwd(&options_box) < 2 {
        let n = count_entries_fwd(&options_box);
        let entry = Entry::new();
        entry.set_placeholder_text(Some(&format!("Option {}", n + 1)));
        wire_auto_add_fwd(&entry, &options_box);
        options_box.append(&entry);
    }
    content.append(&options_box);

    let multi_check = CheckButton::with_label("Allow multiple answers");
    if selectable > 1 {
        multi_check.set_active(true);
    }
    content.append(&multi_check);

    let create_btn = Button::with_label("Choose chats & send");
    create_btn.add_css_class("suggested-action");
    create_btn.set_halign(Align::Center);
    create_btn.set_margin_top(12);

    let bridge = inner.bridge.clone();
    let inner_c = inner.clone();
    let q_c = question_entry.clone();
    let opts_c = options_box.clone();
    let multi_c = multi_check.clone();
    let win_c = window.downgrade();
    create_btn.connect_clicked(move |_| {
        let q = q_c.text().to_string();
        if q.trim().is_empty() {
            return;
        }

        let mut opts: Vec<String> = Vec::new();
        let mut child = opts_c.first_child();
        while let Some(c) = child {
            let next = c.next_sibling();
            if let Some(e) = c.downcast_ref::<Entry>() {
                let text = e.text().to_string();
                if !text.trim().is_empty() {
                    opts.push(text.trim().to_string());
                }
            }
            child = next;
        }
        if opts.len() < 2 {
            return;
        }

        let sel_count = if multi_c.is_active() {
            opts.len() as u32
        } else {
            1
        };
        let parent = inner_c
            .root
            .root()
            .and_then(|r| r.downcast::<gtk4::Window>().ok());
        let bridge_c = bridge.clone();

        if let Some(window) = win_c.upgrade() {
            window.close();
        }

        crate::ui::chat_picker::show_chat_picker(
            "Send poll to…",
            true,
            parent.as_ref(),
            move |selected| {
                for to_chat_id in &selected {
                    bridge_c.send_command(crate::bridge::WaCommand::SendPoll {
                        chat_id: to_chat_id.clone(),
                        question: q.clone(),
                        options: opts.clone(),
                        selectable_count: sel_count,
                    });
                }
            },
        );
    });
    content.append(&create_btn);

    scroll.set_child(Some(&content));
    window.set_child(Some(&scroll));
    window.present();
}

fn do_reply(inner: &Rc<ChatViewInner>, msg: &IncomingMessage) {
    let text_preview = msg
        .text
        .clone()
        .or(msg.media_caption.clone())
        .unwrap_or_else(|| match &msg.media_type {
            Some(crate::bridge::MediaType::Image) => "📷 Photo".to_string(),
            Some(crate::bridge::MediaType::Video) => "🎥 Video".to_string(),
            Some(crate::bridge::MediaType::Audio) => "🎵 Audio".to_string(),
            Some(crate::bridge::MediaType::Document) => "📄 Document".to_string(),
            Some(crate::bridge::MediaType::Sticker) => "🎭 Sticker".to_string(),
            Some(crate::bridge::MediaType::Gif) => "🎞 GIF".to_string(),
            None => "📎 Message".to_string(),
        });

    // Resolve media path FIRST — used for both reply bar thumbnail and stored context
    let is_visual = matches!(
        &msg.media_type,
        Some(crate::bridge::MediaType::Image)
            | Some(crate::bridge::MediaType::Video)
            | Some(crate::bridge::MediaType::Gif)
            | Some(crate::bridge::MediaType::Sticker)
    );
    let media_path: Option<String> = if is_visual {
        msg.media_local_path
            .clone()
            .or_else(|| {
                // Check media_items (updated when image was displayed)
                inner
                    .media_items
                    .borrow()
                    .iter()
                    .find(|(id, _)| *id == msg.id)
                    .map(|(_, p)| p.clone())
            })
            .or_else(|| {
                // Scan wa_media/ by full ID then prefix
                let media_dir = std::path::PathBuf::from("wa_media");
                if !media_dir.exists() {
                    return None;
                }
                let full = media_dir.join(format!("{}.jpeg", msg.id));
                if full.exists() {
                    return Some(full.to_string_lossy().to_string());
                }
                let prefix = &msg.id[..8.min(msg.id.len())];
                for entry in std::fs::read_dir(&media_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let fname = entry.file_name().to_string_lossy().to_string();
                    if fname.starts_with(prefix)
                        && (fname.ends_with(".jpeg")
                            || fname.ends_with(".jpg")
                            || fname.ends_with(".png")
                            || fname.ends_with(".webp"))
                    {
                        return Some(entry.path().to_string_lossy().to_string());
                    }
                }
                None
            })
    } else {
        None
    };

    // Build reply bar content — clear old children first
    remove_all_children(&inner.reply_bar);

    let icon = gtk4::Label::new(Some("↩"));
    icon.add_css_class("dim-label");
    inner.reply_bar.append(&icon);

    // Text column: sender name + preview
    let sender = if msg.is_from_me {
        "You"
    } else {
        &msg.sender_name
    };
    let text_col = Box::new(Orientation::Vertical, 0);
    let sender_lbl = gtk4::Label::new(Some(sender));
    sender_lbl.add_css_class("caption");
    sender_lbl.add_css_class("accent");
    sender_lbl.set_halign(gtk4::Align::Start);
    let preview_lbl = gtk4::Label::new(Some(&text_preview));
    preview_lbl.add_css_class("dim-label");
    preview_lbl.add_css_class("caption");
    preview_lbl.set_halign(gtk4::Align::Start);
    preview_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    text_col.append(&sender_lbl);
    text_col.append(&preview_lbl);
    text_col.set_hexpand(true);
    inner.reply_bar.append(&text_col);

    // Media thumbnail in reply bar (uses pre-resolved media_path)
    if let Some(ref path) = media_path {
        if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(path, 64) {
            let thumb = gtk4::Picture::new();
            thumb.set_paintable(Some(&tex));
            thumb.set_size_request(42, 42);
            thumb.set_can_shrink(true);
            thumb.set_content_fit(gtk4::ContentFit::Cover);
            inner.reply_bar.append(&thumb);
        }
    }

    // Close button
    let close_btn = Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    let inner_c = inner.clone();
    close_btn.connect_clicked(move |_| {
        inner_c.reply_bar.set_visible(false);
        *inner_c.reply_context.borrow_mut() = None;
    });
    inner.reply_bar.append(&close_btn);

    inner.reply_bar.set_visible(true);
    *inner.reply_context.borrow_mut() = Some((
        msg.id.clone(),
        msg.sender_id.clone(),
        text_preview,
        media_path, // Resolved path from three-source lookup above
    ));

    // Focus cursor to input — delay to ensure reply bar is laid out first
    let input = inner.input_view.clone();
    glib::idle_add_local_once(move || {
        input.grab_focus();
    });
}

/// Show a quick-react emoji picker popover.
fn show_react_picker(inner: &Rc<ChatViewInner>, msg: &IncomingMessage, _anchor: &gtk4::Widget) {
    // Parent to the bubble widget (not the hover button which may hide)
    let parent = inner
        .bubbles
        .borrow()
        .get(&msg.id)
        .map(|b| b.widget().clone());
    let popover = gtk4::Popover::new();
    if let Some(ref p) = parent {
        popover.set_parent(p);
    } else {
        // Fallback: parent to the messages_box
        popover.set_parent(&inner.messages_box);
    }
    popover.set_has_arrow(true);
    popover.set_position(gtk4::PositionType::Top);

    let hbox = Box::new(Orientation::Horizontal, 4);
    hbox.set_margin_start(4);
    hbox.set_margin_end(4);
    hbox.set_margin_top(4);
    hbox.set_margin_bottom(4);

    // The user's own current reaction (if any) so we can highlight it and let
    // a second tap clear it. Own reactions are stored with the user's own JID
    // as the sender (runtime persists own_lid/own_phone); match on that.
    let own_reaction: Option<String> = {
        let own = inner.own_jid.borrow();
        own.as_deref().and_then(|own_jid| {
            msg.reactions
                .iter()
                .find(|(sender, _)| sender.as_str() == own_jid)
                .map(|(_, emoji)| emoji.clone())
        })
    };

    let quick_emojis = ["👍", "❤️", "😂", "😮", "😢", "🙏"];
    for emoji in &quick_emojis {
        let btn = Button::with_label(emoji);
        btn.add_css_class("flat");
        // Visually mark the emoji the user has already reacted with.
        let is_current = own_reaction.as_deref() == Some(*emoji);
        if is_current {
            btn.add_css_class("suggested-action");
        }
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let emoji_c = emoji.to_string();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                // Tapping the already-selected reaction clears it (empty emoji),
                // otherwise set/replace with the tapped one.
                let emoji_to_send = if is_current {
                    String::new()
                } else {
                    emoji_c.clone()
                };
                inner_c.bridge.send_command(WaCommand::SendReaction {
                    chat_id: cid,
                    msg_id: msg_c.id.clone(),
                    emoji: emoji_to_send,
                    sender_jid: msg_c.sender_id.clone(),
                    is_from_me: msg_c.is_from_me,
                });
            }
            pop.popdown();
        });
        hbox.append(&btn);
    }

    popover.set_child(Some(&hbox));
    popover.popup();
}

// ── Carousel helpers ──────────────────────────────────────────────────────────

/// Set the click handler on a visual-media bubble so it opens the carousel at
/// the right index. Safe to call multiple times (overwrites previous handler).
fn wire_image_click(inner: &Rc<ChatViewInner>, bubble: &MessageBubble, msg_id: &str) {
    let inner_clone = inner.clone();
    let id_owned = msg_id.to_string();
    bubble.set_image_click_handler(move || {
        let items = inner_clone.media_items.borrow().clone();
        let idx = items
            .iter()
            .position(|(id, _)| id == &id_owned)
            .unwrap_or(0);
        open_carousel(items, idx);
    });
}

/// Open a fullscreen-style carousel window showing all visual media in the conversation.
fn open_carousel(items: Vec<(String, String)>, start_idx: usize) {
    if items.is_empty() {
        return;
    }

    let window = gtk4::Window::builder()
        .default_width(900)
        .default_height(700)
        .decorated(false)
        .build();
    window.add_css_class("lightbox");
    // Suppress GNOME's default titlebar entirely
    let empty_header = gtk4::Box::new(Orientation::Horizontal, 0);
    empty_header.set_visible(false);
    window.set_titlebar(Some(&empty_header));

    let overlay = gtk4::Overlay::new();

    // ── Carousel ──
    let carousel = adw::Carousel::new();
    carousel.set_allow_scroll_wheel(false); // We use scroll wheel for zoom
    carousel.set_hexpand(true);
    carousel.set_vexpand(true);

    let zoom_level = Rc::new(Cell::new(1.0f64));

    let carousel_pictures: Rc<Vec<gtk4::Picture>> = Rc::new(
        items
            .iter()
            .map(|_| {
                let pic = gtk4::Picture::new();
                pic
            })
            .collect(),
    );
    for pic in carousel_pictures.iter() {
        pic.set_can_shrink(true);
        pic.set_content_fit(gtk4::ContentFit::Contain);
        pic.set_hexpand(true);
        pic.set_vexpand(true);
        carousel.append(pic);
    }

    // Keep only the current full-resolution carousel image decoded. The old
    // implementation eagerly decoded every image in the conversation; a dozen
    // modern phone photos could consume hundreds of megabytes before the user
    // even navigated to them.
    let carousel_items = Rc::new(items.clone());
    let load_carousel_page: Rc<dyn Fn(usize)> = {
        let pictures = carousel_pictures.clone();
        let paths = carousel_items.clone();
        Rc::new(move |active| {
            for (idx, picture) in pictures.iter().enumerate() {
                if idx != active {
                    picture.set_paintable(None::<&gtk4::gdk::Paintable>);
                    continue;
                }
                if picture.paintable().is_none()
                    && let Some((_, path)) = paths.get(idx)
                    && let Some(texture) = crate::ui::texture_cache::texture_from_filename(path)
                {
                    picture.set_paintable(Some(&texture));
                }
            }
        })
    };
    load_carousel_page(start_idx);

    // Mouse wheel zoom on the carousel
    {
        let zl = zoom_level.clone();
        let car = carousel.clone();
        let sc = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        sc.connect_scroll(move |_, _, dy| {
            let mut z = zl.get();
            z *= if dy < 0.0 { 1.15 } else { 0.87 }; // Scroll up = zoom in
            z = z.clamp(0.5, 5.0);
            zl.set(z);
            // Apply zoom to current page's picture
            let idx = car.position().round() as u32;
            let page = car.nth_page(idx);
            // Use CSS transform for zoom
            page.set_size_request((900.0 * z) as i32, (700.0 * z) as i32);
            glib::Propagation::Stop
        });
        carousel.add_controller(sc);
    }

    // Jump to the clicked image without animation
    carousel.scroll_to(&carousel.nth_page(start_idx as u32), false);

    overlay.set_child(Some(&carousel));

    // ── Top bar: counter label + close button ──
    let top_bar = Box::new(Orientation::Horizontal, 0);
    top_bar.set_valign(Align::Start);
    top_bar.set_halign(Align::Fill);
    top_bar.add_css_class("osd");
    top_bar.set_margin_top(0);

    let counter = Label::new(Some(&format!("{} / {}", start_idx + 1, items.len())));
    counter.set_hexpand(true);
    counter.set_halign(Align::Center);
    counter.set_margin_start(48); // balance the close button

    // Save button — copies the currently-displayed media file to ~/Downloads
    // (or user-chosen path via FileChooserDialog). Positioned before Close.
    let save_btn = Button::from_icon_name("document-save-symbolic");
    save_btn.add_css_class("flat");
    save_btn.set_tooltip_text(Some("Save to disk"));
    {
        let items_ref = items.clone();
        let car = carousel.clone();
        let win_ref = window.downgrade();
        save_btn.connect_clicked(move |_| {
            let Some(win_ref) = win_ref.upgrade() else {
                return;
            };
            let idx = car.position().round() as usize;
            let Some((_, src_path)) = items_ref.get(idx).cloned() else {
                return;
            };
            let src = std::path::PathBuf::from(&src_path);
            let default_name = src
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("download")
                .to_string();
            // Strip the 8-char msg-id prefix that downloader prepends (e.g.
            // "AB12CD34_image.jpg" → "image.jpg") so the user sees a clean name.
            let clean_name = default_name
                .splitn(2, '_')
                .nth(1)
                .unwrap_or(&default_name)
                .to_string();
            let dialog = gtk4::FileDialog::builder().title("Save media").build();
            dialog.set_initial_name(Some(&clean_name));
            if let Some(home) = std::env::var_os("HOME") {
                let downloads = std::path::PathBuf::from(&home).join("Downloads");
                if downloads.exists() {
                    dialog.set_initial_folder(Some(&gtk4::gio::File::for_path(&downloads)));
                }
            }
            let src_owned = src.clone();
            dialog.save(
                Some(&win_ref),
                gtk4::gio::Cancellable::NONE,
                move |result| {
                    let Some(target) = result.ok().and_then(|file| file.path()) else {
                        return;
                    };
                    std::thread::spawn(move || {
                        if let Err(e) = std::fs::copy(&src_owned, &target) {
                            log::warn!("Save media failed: {e}");
                        } else {
                            log::info!("Saved media to {}", target.display());
                        }
                    });
                },
            );
        });
    }

    let close_btn = Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.set_halign(Align::End);
    let win_clone = window.downgrade();
    close_btn.connect_clicked(move |_| {
        if let Some(window) = win_clone.upgrade() {
            window.close();
        }
    });

    top_bar.append(&counter);
    top_bar.append(&save_btn);
    top_bar.append(&close_btn);
    overlay.add_overlay(&top_bar);

    // ── Prev / Next navigation buttons ──
    let prev_btn = Button::from_icon_name("go-previous-symbolic");
    prev_btn.add_css_class("osd");
    prev_btn.add_css_class("circular");
    prev_btn.set_valign(Align::Center);
    prev_btn.set_halign(Align::Start);
    prev_btn.set_margin_start(12);

    let next_btn = Button::from_icon_name("go-next-symbolic");
    next_btn.add_css_class("osd");
    next_btn.add_css_class("circular");
    next_btn.set_valign(Align::Center);
    next_btn.set_halign(Align::End);
    next_btn.set_margin_end(12);

    {
        let c = carousel.clone();
        let total = items.len() as u32;
        prev_btn.connect_clicked(move |_| {
            let cur = c.position().round() as u32;
            if cur > 0 {
                c.scroll_to(&c.nth_page(cur - 1), true);
            }
        });
        let c = carousel.clone();
        next_btn.connect_clicked(move |_| {
            let cur = c.position().round() as u32;
            if cur + 1 < total {
                c.scroll_to(&c.nth_page(cur + 1), true);
            }
        });
    }

    overlay.add_overlay(&prev_btn);
    overlay.add_overlay(&next_btn);

    // Update counter as user swipes
    {
        let total = items.len();
        let load_page = load_carousel_page.clone();
        carousel.connect_page_changed(move |_, idx| {
            counter.set_text(&format!("{} / {}", idx + 1, total));
            load_page(idx as usize);
        });
    }

    // ── Keyboard navigation ──
    let key_ctrl = gtk4::EventControllerKey::new();
    {
        let c = carousel.clone();
        let total = items.len() as u32;
        let win = window.downgrade();
        key_ctrl.connect_key_pressed(move |_, key, _, _| match key {
            gtk4::gdk::Key::Left | gtk4::gdk::Key::bracketleft => {
                let cur = c.position().round() as u32;
                if cur > 0 {
                    c.scroll_to(&c.nth_page(cur - 1), true);
                }
                gtk4::glib::Propagation::Stop
            }
            gtk4::gdk::Key::Right | gtk4::gdk::Key::bracketright => {
                let cur = c.position().round() as u32;
                if cur + 1 < total {
                    c.scroll_to(&c.nth_page(cur + 1), true);
                }
                gtk4::glib::Propagation::Stop
            }
            gtk4::gdk::Key::Escape => {
                if let Some(window) = win.upgrade() {
                    window.close();
                }
                gtk4::glib::Propagation::Stop
            }
            _ => gtk4::glib::Propagation::Proceed,
        });
    }
    window.add_controller(key_ctrl);

    // Double-click or click outside image area to close
    let bg_click = GestureClick::new();
    bg_click.set_button(1);
    let win_bg = window.downgrade();
    bg_click.connect_released(move |_, n_press, _, _| {
        // Double-click to close (single click navigates carousel)
        if n_press >= 2 {
            if let Some(window) = win_bg.upgrade() {
                window.close();
            }
        }
    });
    carousel.add_controller(bg_click);

    // Release the active full-resolution texture immediately on close. Weak
    // window captures above ensure the viewer itself can then be destroyed.
    let pictures_on_close = carousel_pictures.clone();
    window.connect_close_request(move |_| {
        for picture in pictures_on_close.iter() {
            picture.set_paintable(None::<&gtk4::gdk::Paintable>);
        }
        gtk4::glib::Propagation::Proceed
    });

    window.set_child(Some(&overlay));
    window.fullscreen();
    window.present();
}

// ── Send Group history persistence ──────────────────────────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SendGroupHistoryEntry {
    text: String,
    timestamp: i64,
}

fn send_group_history_path(group_name: &str) -> String {
    let safe = group_name.replace(|c: char| !c.is_alphanumeric() && c != '-', "_");
    format!("wa_sendgroup_{safe}.json")
}

fn load_send_group_history(group_name: &str) -> Vec<crate::bridge::IncomingMessage> {
    let path = send_group_history_path(group_name);
    let entries: Vec<SendGroupHistoryEntry> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default();
    let virtual_chat = format!("sendgroup::{group_name}");
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut msg = crate::bridge::IncomingMessage::outgoing(
                format!("sg_{i}_{}", e.timestamp),
                virtual_chat.clone(),
                Some(e.text.clone()),
                e.timestamp,
            );
            msg.receipt_status = crate::bridge::ReceiptStatus::Delivered;
            msg
        })
        .collect()
}

fn save_send_group_message(group_name: &str, text: &str) {
    let path = send_group_history_path(group_name);
    let mut entries: Vec<SendGroupHistoryEntry> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default();
    entries.push(SendGroupHistoryEntry {
        text: text.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    });
    if let Ok(data) = serde_json::to_string_pretty(&entries) {
        let _ = std::fs::write(path, data);
    }
}
