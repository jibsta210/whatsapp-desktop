use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, ListBox, Orientation, Revealer,
    RevealerTransitionType, ScrolledWindow, SearchEntry, Separator, TextView, Widget,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, IncomingMessage, ReceiptStatus, WaCommand};
use crate::ui::message_bubble::MessageBubble;

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
    messages_box: Box,
    scroll: ScrolledWindow,
    input_view: TextView,
    send_button: Button,
    typing_box: Box,
    typing_name: Label,
    header_name: Label,
    header_subtitle: Label,
    pin_banner: Box,
    bridge: Arc<Bridge>,
    current_chat_id: RefCell<Option<String>>,
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
    // msg_id → searchable text (for the in-chat search filter)
    search_texts: RefCell<HashMap<String, String>>,
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
    image_preview_bar: Box,
    image_preview_pic: gtk4::Picture,
    // Profile open callback (set by window)
    on_profile_open: RefCell<Option<std::boxed::Box<dyn Fn(String, String)>>>,
    // Emoji/GIF/Sticker panel
    emoji_popover: gtk4::Popover,
    gif_grid: gtk4::FlowBox,
    sticker_grid: gtk4::FlowBox,
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

        // Favourite button
        let fav_button = Button::from_icon_name("starred-symbolic");
        fav_button.add_css_class("flat");
        fav_button.set_tooltip_text(Some("Favourite"));
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

        // Voice call button
        let voice_call_btn = Button::from_icon_name("call-start-symbolic");
        voice_call_btn.add_css_class("flat");
        voice_call_btn.set_tooltip_text(Some("Voice call"));
        header.pack_end(&voice_call_btn);

        // Video call button
        let video_call_btn = Button::from_icon_name("camera-video-symbolic");
        video_call_btn.add_css_class("flat");
        video_call_btn.set_tooltip_text(Some("Video call"));
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

        // Send button — same visual size as emoji/attach
        let send_button = Button::from_icon_name("go-up-symbolic");
        send_button.add_css_class("suggested-action");
        send_button.add_css_class("circular");
        send_button.set_size_request(42, 42);
        send_button.set_valign(Align::Center);
        send_button.set_margin_end(4);
        send_button.set_tooltip_text(Some("Send"));

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
        input_frame.append(&input_scroll);
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

        // Tab 1: Emoji grid
        let emoji_tab = Box::new(Orientation::Vertical, 0);
        let emoji_grid = gtk4::FlowBox::new();
        emoji_grid.set_max_children_per_line(8);
        emoji_grid.set_min_children_per_line(8);
        emoji_grid.set_selection_mode(gtk4::SelectionMode::None);
        emoji_grid.set_homogeneous(true);
        let common_emojis = [
            "😀", "😂", "😍", "🥰", "😢", "😡", "👍", "👎", "❤️", "🔥", "🎉", "💯", "🙏", "😊",
            "🤔", "😎", "👋", "✨", "💪", "🤝", "😭", "🥺", "😤", "🫡", "🎊", "💀", "😱", "🤩",
            "😘", "💕", "👀", "🫶", "🤣", "😇", "🥳", "🤯", "💔", "🫠", "😏", "🙄", "😒", "🤗",
            "🤭", "🫣", "💅", "🦋", "🌈", "⭐",
        ];
        // Will wire clicks after inner is created
        for emoji in &common_emojis {
            let btn = Button::with_label(emoji);
            btn.add_css_class("flat");
            btn.set_widget_name(emoji);
            btn.set_size_request(46, 42);
            emoji_grid.append(&btn);
        }
        let emoji_scroll = ScrolledWindow::new();
        emoji_scroll.set_child(Some(&emoji_grid));
        emoji_scroll.set_vexpand(true);
        emoji_tab.append(&emoji_scroll);
        notebook.append_page(&emoji_tab, Some(&Label::new(Some("😀 Emoji"))));

        // Tab 2: GIF search
        let gif_tab = Box::new(Orientation::Vertical, 4);
        let gif_search = SearchEntry::new();
        gif_search.set_placeholder_text(Some("Search GIFs"));
        gif_search.set_margin_start(4);
        gif_search.set_margin_end(4);
        gif_search.set_margin_top(4);

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
        gif_tab.append(&gif_scroll);
        notebook.append_page(&gif_tab, Some(&Label::new(Some("GIF"))));

        // Tab 3: Sticker search
        let sticker_tab = Box::new(Orientation::Vertical, 4);
        let sticker_search = SearchEntry::new();
        sticker_search.set_placeholder_text(Some("Search stickers"));
        sticker_search.set_margin_start(4);
        sticker_search.set_margin_end(4);
        sticker_search.set_margin_top(4);

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
        sticker_tab.append(&sticker_scroll);
        notebook.append_page(&sticker_tab, Some(&Label::new(Some("🎭 Stickers"))));

        emoji_popover.set_child(Some(&notebook));

        // Pinned message banner (between header and scroll, hidden by default)
        let pin_banner = Box::new(Orientation::Horizontal, 8);
        pin_banner.add_css_class("pin-banner");
        pin_banner.set_visible(false);

        root.append(&header);
        root.append(&search_revealer);
        root.append(&pin_banner);
        root.append(&scroll_overlay);
        root.append(&typing_box);
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
        root.append(&input_bar);
        root.append(&forward_bar);

        let inner = Rc::new(ChatViewInner {
            root,
            messages_box,
            scroll,
            input_view,
            send_button,
            typing_box,
            typing_name,
            header_name,
            header_subtitle,
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
            search_texts: RefCell::new(HashMap::new()),
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
            image_preview_bar,
            image_preview_pic,
            on_profile_open: RefCell::new(None),
            emoji_popover,
            gif_grid,
            sticker_grid,
            editing_msg: RefCell::new(None),
            edit_banner,
            ai_spinner,
            ac_delay_send,
            drafts: RefCell::new(HashMap::new()),
            send_group_ids: RefCell::new(None),
        });

        // Poll AI autocorrect in-flight status to toggle the spinner
        {
            let spinner = inner.ai_spinner.clone();
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
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

        // When content height changes (layout/image load), auto-scroll if pending
        {
            let sp = inner.scroll_pending.clone();
            let adj = inner.scroll.vadjustment();
            adj.connect_changed(move |a| {
                let count = sp.get();
                if count > 0 {
                    a.set_value(a.upper() - a.page_size());
                    sp.set(count - 1);
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

        // Search button toggles the search bar
        {
            let inner_clone = inner.clone();
            // Favourite button
            {
                let inner_c = inner.clone();
                fav_button.connect_clicked(move |btn| {
                    if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                        // Toggle: check current icon to determine state
                        let is_fav = btn.icon_name().as_deref() == Some("starred-symbolic");
                        let new_fav = !is_fav;
                        inner_c.bridge.send_command(WaCommand::FavoriteChat {
                            chat_id: cid,
                            favorite: new_fav,
                        });
                        if new_fav {
                            btn.set_icon_name("starred-symbolic");
                            btn.set_tooltip_text(Some("Remove from favourites"));
                        } else {
                            btn.set_icon_name("non-starred-symbolic");
                            btn.set_tooltip_text(Some("Add to favourites"));
                        }
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
                        let colour_str = colour.to_string();
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
            let record_child: Rc<RefCell<Option<std::process::Child>>> = Rc::new(RefCell::new(None));
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
                    if !rec.get() { return; }
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
                        unsafe { libc::kill(child.id() as i32, libc::SIGINT); }
                        let _ = child.wait();
                    }

                    if let (Some(cid), Some(path)) =
                        (inner_c.current_chat_id.borrow().clone(), rp.borrow().clone())
                    {
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
                        play_btn.connect_clicked(move |_| {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(&path_play)
                                .spawn();
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
                        inner_c.input_bar.insert_child_after(&review, None::<&gtk4::Widget>);

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
                                move || {
                                    match std::fs::metadata(&path) {
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
                                                .as_secs() as i64;
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
                                                chat_id: cid,
                                                path,
                                                duration_secs: dur,
                                                is_voice_note: true,
                                                tmp_id,
                                            });
                                        }
                                        _ => log::warn!("Voice note file missing or empty"),
                                    }
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
                    if rec.get() { return; }
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
                        if s.audio_input.is_empty() { "default".to_string() } else { s.audio_input }
                    };
                    match std::process::Command::new("ffmpeg")
                        .args([
                            "-y", "-f", "pulse", "-i", &audio_input,
                            "-ac", "1",
                            "-c:a", "libopus", "-b:a", "32k", "-ar", "48000",
                            "-application", "voip",
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
            press_gesture.set_delay_factor(0.3); // ~150ms to trigger
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
                            inner_clone.mention_popover.popdown();
                            inner_clone.slash_popover.popdown();
                            // Also cancel image/gif preview
                            *inner_clone.pending_image_path.borrow_mut() = None;
                            *inner_clone.pending_gif_url.borrow_mut() = None;
                            inner_clone.image_preview_bar.set_visible(false);
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

        // ── Ctrl+V paste handler for images — shows preview, Enter to send ──
        {
            let inner_c = inner.clone();
            let paste_ctrl = gtk4::EventControllerKey::new();
            paste_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
            paste_ctrl.connect_key_pressed(move |_, key, _, modifier| {
                if key == gtk4::gdk::Key::v
                    && modifier.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
                {
                    log::info!("Paste handler: Ctrl+V detected, checking clipboard");
                    let Some(display) = gtk4::gdk::Display::default() else {
                        log::warn!("Paste: no default display");
                        return gtk4::glib::Propagation::Proceed;
                    };
                    let clipboard = display.clipboard();
                    let inner_cc = inner_c.clone();
                    // Primary path: read as GDK texture. Works for images
                    // copied from most apps on X11 and Wayland.
                    clipboard.read_texture_async(None::<&gtk4::gio::Cancellable>, move |result| {
                        match result {
                            Ok(Some(texture)) => {
                                log::info!("Paste: got texture from clipboard");
                                let tmp_path = format!(
                                    "/tmp/wa_paste_{}.png",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis()
                                );
                                match texture.save_to_png(&tmp_path) {
                                    Ok(_) => {
                                        inner_cc.image_preview_pic.set_paintable(Some(&texture));
                                        *inner_cc.pending_image_path.borrow_mut() =
                                            Some(tmp_path);
                                        inner_cc.image_preview_bar.set_visible(true);
                                        inner_cc.input_view.grab_focus();
                                    }
                                    Err(e) => log::warn!("Paste: save_to_png failed: {e}"),
                                }
                            }
                            Ok(None) => {
                                log::info!("Paste: no texture on clipboard (likely text paste)");
                            }
                            Err(e) => {
                                // Common when clipboard has image in MIME type GDK
                                // can't decode directly (e.g. image/jpeg on some
                                // Wayland compositors). Try reading raw bytes.
                                log::info!(
                                    "Paste: read_texture_async failed ({e}), trying raw bytes fallback"
                                );
                                let display_fb = gtk4::gdk::Display::default();
                                if let Some(d) = display_fb {
                                    let cb = d.clipboard();
                                    let inner_fb = inner_cc.clone();
                                    cb.read_async(
                                        &["image/png", "image/jpeg", "image/webp", "image/gif"],
                                        gtk4::glib::Priority::DEFAULT,
                                        None::<&gtk4::gio::Cancellable>,
                                        move |res| {
                                            let Ok((stream, mime)) = res else {
                                                log::warn!("Paste fallback: no image MIME on clipboard");
                                                return;
                                            };
                                            log::info!("Paste fallback: got {mime} stream");
                                            use gtk4::gio::prelude::*;
                                            let ext = mime.split('/').nth(1).unwrap_or("png");
                                            let tmp_path = format!(
                                                "/tmp/wa_paste_{}.{}",
                                                std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_millis(),
                                                ext
                                            );
                                            let path_for_close = tmp_path.clone();
                                            let inner_done = inner_fb.clone();
                                            stream.read_bytes_async(
                                                10 * 1024 * 1024, // 10MB cap
                                                gtk4::glib::Priority::DEFAULT,
                                                None::<&gtk4::gio::Cancellable>,
                                                move |read_res| {
                                                    match read_res {
                                                        Ok(bytes) => {
                                                            if let Err(e) =
                                                                std::fs::write(&path_for_close, &bytes)
                                                            {
                                                                log::warn!(
                                                                    "Paste fallback: write failed: {e}"
                                                                );
                                                                return;
                                                            }
                                                            if let Ok(tex) =
                                                                gtk4::gdk::Texture::from_filename(
                                                                    &path_for_close,
                                                                )
                                                            {
                                                                inner_done
                                                                    .image_preview_pic
                                                                    .set_paintable(Some(&tex));
                                                            }
                                                            *inner_done
                                                                .pending_image_path
                                                                .borrow_mut() =
                                                                Some(path_for_close);
                                                            inner_done
                                                                .image_preview_bar
                                                                .set_visible(true);
                                                            inner_done.input_view.grab_focus();
                                                        }
                                                        Err(e) => log::warn!(
                                                            "Paste fallback: read_bytes failed: {e}"
                                                        ),
                                                    }
                                                },
                                            );
                                        },
                                    );
                                }
                            }
                        }
                    });
                }
                gtk4::glib::Propagation::Proceed
            });
            inner.input_view.add_controller(paste_ctrl);
        }

        // ── Buffer changed: trigger @ mention and / quick reply popovers ──
        {
            let inner_c = inner.clone();
            inner.input_view.buffer().connect_changed(move |buf| {
                let cursor = buf.iter_at_mark(&buf.get_insert());
                let text = buf.text(&buf.start_iter(), &cursor, false).to_string();

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
                                        if let Ok(tex) = gtk4::gdk::Texture::from_filename(&av_path)
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
            // Accept both single File and FileList for multi-file drops
            let drop_target = gtk4::DropTarget::new(
                gtk4::gdk::FileList::static_type(),
                gtk4::gdk::DragAction::COPY,
            );
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
                    // Single file: show preview like before
                    let path_str = &paths[0];
                    if let Ok(tex) = gtk4::gdk::Texture::from_filename(path_str) {
                        inner_c.image_preview_pic.set_paintable(Some(&tex));
                    }
                    *inner_c.pending_image_path.borrow_mut() = Some(path_str.clone());
                    *inner_c.pending_gif_url.borrow_mut() = None;
                    inner_c.image_preview_bar.set_visible(true);
                    inner_c.input_view.grab_focus();
                } else {
                    // Multiple files: send each one immediately
                    if let Some(chat_id) = inner_c.current_chat_id.borrow().clone() {
                        for path_str in &paths {
                            let tmp_id = gen_tmp_id();
                            inner_c.bridge.send_command(WaCommand::SendImage {
                                chat_id: chat_id.clone(),
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
                inner_clone.image_preview_bar.set_visible(false);
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
                                            // Show preview — try image first, fall back to file icon
                                            if let Ok(tex) =
                                                gtk4::gdk::Texture::from_filename(&path_str)
                                            {
                                                inner_ccc
                                                    .image_preview_pic
                                                    .set_paintable(Some(&tex));
                                            } else {
                                                // Non-image file — show filename as preview
                                                inner_ccc
                                                    .image_preview_pic
                                                    .set_paintable(None::<&gtk4::gdk::Paintable>);
                                            }
                                            *inner_ccc.pending_image_path.borrow_mut() =
                                                Some(path_str);
                                            inner_ccc.image_preview_bar.set_visible(true);
                                            inner_ccc.input_view.grab_focus();
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
            let inner_c = inner.clone();
            emoji_btn.connect_clicked(move |_| {
                if inner_c.emoji_popover.is_visible() {
                    inner_c.emoji_popover.popdown();
                } else {
                    // Load trending GIFs and stickers on open
                    inner_c.bridge.send_command(WaCommand::SearchGifs {
                        query: "trending".to_string(),
                    });
                    inner_c.bridge.send_command(WaCommand::SearchStickers {
                        query: "trending".to_string(),
                    });
                    inner_c.emoji_popover.popup();
                }
            });
        }

        // Wire emoji grid button clicks
        {
            let inner_c = inner.clone();
            // Iterate all children of emoji_grid (they're FlowBoxChild wrappers)
            let mut child = emoji_grid.first_child();
            while let Some(c) = child {
                let next = c.next_sibling();
                if let Ok(flow_child) = c.clone().downcast::<gtk4::FlowBoxChild>() {
                    if let Some(btn) = flow_child.child().and_then(|c| c.downcast::<Button>().ok())
                    {
                        let emoji = btn.widget_name().to_string();
                        let inner_cc = inner_c.clone();
                        btn.connect_clicked(move |_| {
                            inner_cc.input_view.buffer().insert_at_cursor(&emoji);
                            inner_cc.emoji_popover.popdown();
                        });
                    }
                }
                child = next;
            }
        }

        // Wire GIF search
        {
            let inner_c = inner.clone();
            gif_search.connect_search_changed(move |entry| {
                let q = entry.text().to_string();
                if q.len() >= 2 {
                    inner_c
                        .bridge
                        .send_command(WaCommand::SearchGifs { query: q });
                } else if q.is_empty() {
                    inner_c.bridge.send_command(WaCommand::SearchGifs {
                        query: "trending".to_string(),
                    });
                }
            });
        }

        // Wire sticker search
        {
            let inner_c = inner.clone();
            sticker_search.connect_search_changed(move |entry| {
                let q = entry.text().to_string();
                if q.len() >= 2 {
                    inner_c
                        .bridge
                        .send_command(WaCommand::SearchStickers { query: q });
                } else if q.is_empty() {
                    inner_c.bridge.send_command(WaCommand::SearchStickers {
                        query: "trending".to_string(),
                    });
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

    fn do_send(inner: &Rc<ChatViewInner>) {
        let chat_id = match inner.current_chat_id.borrow().clone() {
            Some(id) => id,
            None => return,
        };

        // ── Image / GIF / Edit ──

        let pending_image = inner.pending_image_path.borrow_mut().take();
        if let Some(image_path) = pending_image {
            inner.image_preview_bar.set_visible(false);
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
                        chat_id,
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
                    chat_id,
                    path: image_path,
                    caption,
                    tmp_id,
                });
            }
            return;
        }

        let pending_gif = inner.pending_gif_url.borrow_mut().take();
        if let Some(mp4_url) = pending_gif {
            inner.image_preview_bar.set_visible(false);
            inner.input_view.buffer().set_text("");
            let tmp_id = gen_tmp_id();
            inner.bridge.send_command(WaCommand::SendGif {
                chat_id,
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

        // Edit an existing message — send immediately, no AC delay
        let editing = inner.editing_msg.borrow_mut().take();
        if let Some((edit_chat_id, edit_msg_id)) = editing {
            buf.set_text("");
            inner.edit_banner.set_reveal_child(false);
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
        };
        Self::append_bubble_to_inner(inner, optimistic);

        // Collect @mention data (jid\tname pairs → jid list + name→number mapping)
        let raw_mentions: Vec<String> = inner.pending_mentions.borrow_mut().drain(..).collect();
        let mut mentioned_jids: Vec<String> = Vec::new();
        let mut mention_replacements: Vec<(String, String)> = Vec::new(); // (@Name, @Number)
        for entry in &raw_mentions {
            if let Some((jid, name)) = entry.split_once('\t') {
                mentioned_jids.push(jid.to_string());
                let jid_number = jid.split('@').next().unwrap_or(jid);
                mention_replacements.push((format!("@{name}"), format!("@{jid_number}")));
            } else {
                mentioned_jids.push(entry.clone());
            }
        }

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
        let msgs_box_for_send = inner.messages_box.clone();
        let send_network = move |final_text: String| {
            // Apply @Name → @Number replacement for the WA protocol
            let mut send_text = final_text.clone();
            for (name_pat, number_pat) in &mention_replacements {
                send_text = send_text.replace(name_pat.as_str(), number_pat.as_str());
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
                bridge.send_command(WaCommand::SendReply {
                    chat_id,
                    text: send_text,
                    quoted_msg_id,
                    quoted_sender,
                    tmp_id,
                    mentioned_jids,
                });
            } else {
                bridge.send_command(WaCommand::SendText {
                    chat_id,
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
                        final_text.len(), original_for_guard.len()
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
                        move || { send_network(safe_text); },
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
            gtk4::glib::timeout_add_local_once(
                std::time::Duration::from_millis(150),
                move || {
                    if let Some(bubble) = bubbles_settle.borrow().get(&tid) {
                        let w = bubble.widget();
                        w.remove_css_class("pilafy");
                        w.add_css_class("pilafy-settle");
                        let w2 = w.clone();
                        gtk4::glib::timeout_add_local_once(
                            std::time::Duration::from_millis(500),
                            move || { w2.remove_css_class("pilafy-settle"); },
                        );
                    }
                },
            );
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

    /// Open a chat immediately (sets current_chat_id, clears messages, shows loading).
    /// Returns false if the chat is already open (no reload needed).
    pub fn open_chat(&self, chat_id: String, chat_name: &str) -> bool {
        // If this chat is already displayed, just update the header and skip reload
        let already_open = self.inner.current_chat_id.borrow().as_deref() == Some(&chat_id);
        if already_open {
            self.inner.header_name.set_text(chat_name);
            self.inner.input_view.grab_focus();
            return false;
        }

        // ── Save draft for outgoing chat ──
        if let Some(old_chat_id) = self.inner.current_chat_id.borrow().clone() {
            let buf = self.inner.input_view.buffer();
            let draft = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .to_string();
            if draft.trim().is_empty() {
                self.inner.drafts.borrow_mut().remove(&old_chat_id);
            } else {
                self.inner.drafts.borrow_mut().insert(old_chat_id, draft);
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
        // Clear send group mode when switching to a real chat
        *self.inner.send_group_ids.borrow_mut() = None;
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
        self.inner.input_view.buffer().set_text(&draft);

        // Clear message area and search/media state
        remove_all_children(&self.inner.messages_box);
        self.inner.bubbles.borrow_mut().clear();
        self.inner.search_texts.borrow_mut().clear();
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
                    self.inner.typing_name.set_markup(
                        &format!("<small><b>{label}</b> </small>")
                    );
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
        self.inner.input_view.grab_focus();
        true
    }

    /// Open a send group as a virtual chat in the existing message pane.
    /// Messages typed here are dispatched via MultiSend to all group members.
    pub fn open_send_group(&self, group_name: &str, chat_ids: Vec<String>) {
        let virtual_id = format!("sendgroup::{}", group_name);
        let n = chat_ids.len();
        // open_chat resets the UI and clears send_group_ids, so we set AFTER.
        self.open_chat(virtual_id, group_name);
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
                self.append_message_inner(msg);
            }
            Self::force_scroll_to_bottom(&self.inner, 3);
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
        } else {
            // Hide content, append all messages, scroll to bottom, then fade in.
            // This avoids the jarring piece-by-piece rendering.
            let msgs_box = &self.inner.messages_box;
            msgs_box.set_opacity(0.0);
            for msg in messages {
                self.append_message_inner(msg);
            }
            self.inner.at_bottom.set(true);
            self.inner.goto_latest_btn.set_visible(false);
            self.inner.scroll_pending.set(0);

            // After layout settles, snap to bottom and fade in
            let scroll = self.inner.scroll.clone();
            let box_c = msgs_box.clone();
            let sp = self.inner.scroll_pending.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(30), move || {
                let adj = scroll.vadjustment();
                adj.set_value(adj.upper() - adj.page_size());
                // Fade in over ~80ms using 4 steps
                let box_c2 = box_c.clone();
                let step = Rc::new(Cell::new(0u32));
                glib::timeout_add_local(std::time::Duration::from_millis(20), move || {
                    let s = step.get() + 1;
                    step.set(s);
                    let opacity = (s as f64) * 0.25;
                    box_c2.set_opacity(opacity.min(1.0));
                    if s >= 4 {
                        box_c2.set_opacity(1.0);
                        return glib::ControlFlow::Break;
                    }
                    // Also keep scroll at bottom during fade
                    if let Some(parent) = box_c2.parent() {
                        if let Some(sw) = parent.downcast_ref::<gtk4::ScrolledWindow>() {
                            let adj = sw.vadjustment();
                            adj.set_value(adj.upper() - adj.page_size());
                        }
                    }
                    glib::ControlFlow::Continue
                });
                sp.set(3);
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
            if msg.is_from_me {
                log::info!(
                    "append_message: self-message for chat={} doesn't match current={:?} \
                     — message will be in history but not rendered now",
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
        let mut bubbles = self.inner.bubbles.borrow_mut();
        if let Some(bubble) = bubbles.remove(tmp_id) {
            bubble.update_receipt(&ReceiptStatus::Sent);
            bubbles.insert(real_id.to_string(), bubble);
        }
        let mut texts = self.inner.search_texts.borrow_mut();
        if let Some(text) = texts.remove(tmp_id) {
            texts.insert(real_id.to_string(), text);
        }
    }

    fn append_bubble_to_inner(inner: &Rc<ChatViewInner>, msg: IncomingMessage) {
        // Dedup: if a bubble already exists for this msg id (e.g. optimistic bubble
        // was already confirmed via MessageConfirmed), skip creating a new one.
        if inner.bubbles.borrow().contains_key(&msg.id) {
            return;
        }

        // Insert a date separator when the day changes
        maybe_insert_date_separator(inner, msg.timestamp);

        let own_name = inner.own_name.borrow().clone();
        let bubble = MessageBubble::new(&msg, &own_name, &inner.bridge);

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
            let chevron = bubble.chevron_button().clone();
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
            let av_gesture = GestureClick::new();
            av_gesture.set_button(1);
            av_gesture.connect_released(move |g, _, _, _| {
                g.set_state(gtk4::EventSequenceState::Claimed);
                if let Some(cb) = inner_c.on_profile_open.borrow().as_ref() {
                    if !chat_id_for_profile.is_empty() {
                        cb(chat_id_for_profile.clone(), name_for_profile.clone());
                    }
                }
            });
            bubble.avatar_widget().add_controller(av_gesture);
            bubble.avatar_widget().set_cursor_from_name(Some("pointer"));
        }

        // Wire contact card "Message" button — find by widget_name containing JID
        if bubble.contact_jid.is_some() {
            let name = msg.contact_name.clone().unwrap_or_default();
            let inner_c = inner.clone();
            fn find_btn_by_name(widget: &gtk4::Widget) -> Option<(Button, String)> {
                if let Ok(btn) = widget.clone().downcast::<Button>() {
                    let wn = btn.widget_name().to_string();
                    if wn.contains("@s.whatsapp.net") {
                        return Some((btn, wn));
                    }
                }
                let mut child = widget.first_child();
                while let Some(c) = child {
                    if let Some(found) = find_btn_by_name(&c) {
                        return Some(found);
                    }
                    child = c.next_sibling();
                }
                None
            }
            if let Some((msg_btn, jid)) = find_btn_by_name(bubble.widget().upcast_ref()) {
                msg_btn.connect_clicked(move |_| {
                    let j = jid.clone();
                    *inner_c.current_chat_id.borrow_mut() = Some(j.clone());
                    inner_c.header_name.set_text(&name);
                    while let Some(child) = inner_c.messages_box.first_child() {
                        inner_c.messages_box.remove(&child);
                    }
                    inner_c.bubbles.borrow_mut().clear();
                    inner_c
                        .bridge
                        .send_command(WaCommand::StartNewChat { jid: j.clone() });
                    inner_c.bridge.send_command(WaCommand::LoadChat {
                        chat_id: j.clone(),
                        chat_name: name.clone(),
                    });
                    inner_c.input_view.grab_focus();
                });
            }
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

        inner.messages_box.append(bubble.widget());
        inner.bubbles.borrow_mut().insert(msg.id.clone(), bubble);
        inner
            .search_texts
            .borrow_mut()
            .insert(msg.id.clone(), search_text);

        // Apply current search filter to the new bubble
        let query = inner.search_entry.text().to_lowercase();
        if !query.is_empty() {
            Self::apply_search_filter(inner, &query);
        }

        // Only auto-scroll if user is already at the bottom — never override their scroll position
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

    pub fn show_reaction(&self, chat_id: &str, msg_id: &str, emoji: &str) {
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
            // Check if there's already a reaction row after this bubble
            let root = bubble.widget();
            // The root is a vertical Box — check if last child is a reaction row
            let existing = root
                .last_child()
                .filter(|c| c.widget_name() == "reaction-row");

            if let Some(row) = existing {
                // Append to existing row
                let pill = Label::new(Some(emoji));
                pill.add_css_class("caption");
                row.downcast_ref::<Box>().map(|b| b.append(&pill));
            } else {
                // Create new reaction row
                let reaction_row = Box::new(Orientation::Horizontal, 4);
                reaction_row.set_widget_name("reaction-row");
                reaction_row.set_margin_top(-6);
                reaction_row.set_margin_start(if bubble.is_from_me { 60 } else { 44 });
                reaction_row.set_halign(if bubble.is_from_me {
                    Align::End
                } else {
                    Align::Start
                });

                let pill = Label::new(Some(emoji));
                pill.add_css_class("caption");
                reaction_row.append(&pill);
                root.append(&reaction_row);
            }
        }
    }

    pub fn show_gif_results(&self, gifs: Vec<crate::bridge::GifResult>) {
        let grid = &self.inner.gif_grid;
        grid.remove_all();

        for gif in &gifs {
            let frame = Box::new(Orientation::Vertical, 2);
            frame.set_size_request(170, 130);
            frame.set_cursor_from_name(Some("pointer"));

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

            // Download preview on background thread, update via async_channel
            let preview_url = gif.preview_url.clone();
            let pic_clone = pic.clone();
            let (tx_img, rx_img) = async_channel::bounded::<Vec<u8>>(1);
            glib::MainContext::default().spawn_local(async move {
                if let Ok(bytes) = rx_img.recv().await {
                    let gbytes = glib::Bytes::from(&bytes);
                    if let Ok(tex) = gtk4::gdk::Texture::from_bytes(&gbytes) {
                        pic_clone.set_paintable(Some(&tex));
                    }
                }
            });
            std::thread::spawn(move || {
                use std::io::Read;
                if let Ok(resp) = ureq::get(&preview_url).call() {
                    let mut bytes = Vec::new();
                    if resp.into_reader().read_to_end(&mut bytes).is_ok() {
                        let _ = tx_img.send_blocking(bytes);
                    }
                }
            });

            // Click GIF → show preview, user presses Enter to send
            let mp4_url = gif.mp4_url.clone();
            let preview_url_c = gif.preview_url.clone();
            let inner_c = self.inner.clone();
            let gesture = gtk4::GestureClick::new();
            gesture.set_button(1);
            gesture.connect_released(move |_, _, _, _| {
                // Set the preview image from the already-loaded thumbnail
                // and store the MP4 URL for sending on Enter
                *inner_c.pending_gif_url.borrow_mut() = Some(mp4_url.clone());
                *inner_c.pending_image_path.borrow_mut() = None;
                inner_c.image_preview_bar.set_visible(true);
                inner_c.emoji_popover.popdown();
                // Load preview into the preview bar
                let url = preview_url_c.clone();
                let pic = inner_c.image_preview_pic.clone();
                let (tx, rx) = async_channel::bounded::<Vec<u8>>(1);
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(bytes) = rx.recv().await {
                        let gb = glib::Bytes::from(&bytes);
                        if let Ok(tex) = gtk4::gdk::Texture::from_bytes(&gb) {
                            pic.set_paintable(Some(&tex));
                        }
                    }
                });
                std::thread::spawn(move || {
                    use std::io::Read;
                    if let Ok(resp) = ureq::get(&url).call() {
                        let mut bytes = Vec::new();
                        if resp.into_reader().read_to_end(&mut bytes).is_ok() {
                            let _ = tx.send_blocking(bytes);
                        }
                    }
                });
                inner_c.input_view.grab_focus();
            });
            frame.add_controller(gesture);

            grid.append(&frame);
        }
    }

    pub fn show_sticker_results(&self, stickers: Vec<crate::bridge::GifResult>) {
        let grid = &self.inner.sticker_grid;
        grid.remove_all();

        for sticker in &stickers {
            let pic = gtk4::Picture::new();
            pic.set_size_request(90, 90);
            pic.set_content_fit(gtk4::ContentFit::Contain);
            pic.set_can_shrink(true);
            pic.set_cursor_from_name(Some("pointer"));

            // Download preview
            let preview_url = sticker.preview_url.clone();
            let pic_clone = pic.clone();
            let (tx_img, rx_img) = async_channel::bounded::<Vec<u8>>(1);
            glib::MainContext::default().spawn_local(async move {
                if let Ok(bytes) = rx_img.recv().await {
                    let gb = glib::Bytes::from(&bytes);
                    if let Ok(tex) = gtk4::gdk::Texture::from_bytes(&gb) {
                        pic_clone.set_paintable(Some(&tex));
                    }
                }
            });
            std::thread::spawn(move || {
                use std::io::Read;
                if let Ok(resp) = ureq::get(&preview_url).call() {
                    let mut bytes = Vec::new();
                    if resp.into_reader().read_to_end(&mut bytes).is_ok() {
                        let _ = tx_img.send_blocking(bytes);
                    }
                }
            });

            // Click to send sticker
            let webp_url = sticker.mp4_url.clone(); // reused field for webp URL
            let inner_c = self.inner.clone();
            let gesture = gtk4::GestureClick::new();
            gesture.set_button(1);
            gesture.connect_released(move |_, _, _, _| {
                let bridge = inner_c.bridge.clone();
                let chat_id = inner_c.current_chat_id.borrow().clone();
                inner_c.emoji_popover.popdown();
                if let Some(chat_id) = chat_id {
                    let tmp_id = format!(
                        "tmp-stk-{:08x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_nanos()
                    );
                    bridge.send_command(WaCommand::SendSticker {
                        chat_id,
                        webp_url: webp_url.clone(),
                        tmp_id,
                    });
                }
            });
            pic.add_controller(gesture);

            grid.append(&pic);
        }
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
    pub fn update_chat_name(&self, chat_id: &str, name: &str) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if is_current {
            self.inner.header_name.set_text(name);
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
            // Scroll to the pinned message bubble
            let bubbles = inner_c.bubbles.borrow();
            if let Some(bubble) = bubbles.get(&msg_id_c) {
                let widget = bubble.widget();
                // Use scroll_child or compute position manually
                let adj = inner_c.scroll.vadjustment();
                // Get the widget's allocation relative to the messages_box
                if let Some((_, y)) = widget.translate_coordinates(&inner_c.messages_box, 0.0, 0.0)
                {
                    adj.set_value(y as f64);
                }
                // Flash the bubble briefly to highlight it
                widget.add_css_class("flash-highlight");
                let w = widget.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    w.remove_css_class("flash-highlight");
                });
            }
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
        let is_current = self.inner.current_chat_id.borrow()
            .as_deref().map(|id| id == chat_id).unwrap_or(false);
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
            gtk4::glib::timeout_add_local_once(
                std::time::Duration::from_secs(15),
                move || {
                    if let Some(inner) = inner_w.upgrade() {
                        let mut all = inner.all_typers.borrow_mut();
                        if let Some(typers) = all.get_mut(&cid) {
                            typers.retain(|t| *t != name);
                        }
                        let is_current = inner.current_chat_id.borrow()
                            .as_deref().map(|id| id == cid).unwrap_or(false);
                        if is_current {
                            let remaining = all.get(&cid).cloned().unwrap_or_default();
                            drop(all);
                            if remaining.is_empty() {
                                inner.typing_box.set_visible(false);
                            } else {
                                let label = remaining.join(", ");
                                inner.typing_name.set_markup(
                                    &format!("<small><b>{label}</b> </small>"),
                                );
                            }
                        }
                    }
                },
            );
        }
    }
}

fn gen_tmp_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("tmp-{:08x}", nanos)
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
        let btn = menu_btn!("Resend");
        let inner_c = inner.clone();
        let msg_id = msg.id.clone();
        let text = inner
            .bubbles
            .borrow()
            .get(&msg_id)
            .and_then(|b| b.text.clone())
            .unwrap_or_default();
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
                let dm_jid = msg_c.sender_id.clone();
                let sender_name = if msg_c.sender_name.is_empty() {
                    crate::ui::runtime::display_name_from_jid(&dm_jid)
                } else {
                    msg_c.sender_name.clone()
                };
                // Fully switch to the DM chat (clear old messages, set new ID)
                *inner_c.current_chat_id.borrow_mut() = Some(dm_jid.clone());
                inner_c.header_name.set_text(&sender_name);
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
                // Switch chat view to the DM
                let sid = sender_id.clone();
                *inner_c.current_chat_id.borrow_mut() = Some(sid.clone());
                inner_c.header_name.set_text(&sender_name_c);
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

        // Copy
        if let Some(text) = &msg.text {
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

        // Star
        let btn = menu_btn!("Star");
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                inner_c.bridge.send_command(WaCommand::StarMessage {
                    chat_id: cid,
                    msg_id: msg_c.id.clone(),
                    starred: true,
                    sender_jid: msg_c.sender_id.clone(),
                    is_from_me: msg_c.is_from_me,
                });
            }
            pop.popdown();
        });
        vbox.append(&btn);

        // Edit (own text messages only)
        if msg.is_from_me && msg.text.is_some() && msg.media_type.is_none() {
            let btn = menu_btn!("Edit");
            let inner_c = inner.clone();
            let msg_c = msg.clone();
            let pop = popover.clone();
            btn.connect_clicked(move |_| {
                pop.popdown();
                // Pre-fill input with current text and set edit mode
                if let Some(text) = &msg_c.text {
                    let buf = inner_c.input_view.buffer();
                    buf.set_text(text);
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
            btn.connect_clicked(move |_| {
                pop.popdown();
                // Show a small dialog for the shortcut name
                let dialog = gtk4::Window::builder()
                    .title("Save Quick Reply")
                    .default_width(300)
                    .default_height(120)
                    .modal(true)
                    .build();
                let vbox = Box::new(Orientation::Vertical, 8);
                vbox.set_margin_top(12);
                vbox.set_margin_bottom(12);
                vbox.set_margin_start(12);
                vbox.set_margin_end(12);
                let label = Label::new(Some("Shortcut name (without /):"));
                label.set_halign(Align::Start);
                vbox.append(&label);
                let entry = gtk4::Entry::new();
                entry.set_placeholder_text(Some("e.g. greeting"));
                vbox.append(&entry);
                let save_btn = Button::with_label("Save");
                save_btn.add_css_class("suggested-action");
                let b = bridge.clone();
                let t = text_c.clone();
                let d = dialog.clone();
                let e = entry.clone();
                save_btn.connect_clicked(move |_| {
                    let shortcut = e.text().to_string().trim().to_string();
                    if !shortcut.is_empty() {
                        b.send_command(WaCommand::SaveQuickReply {
                            shortcut,
                            message: t.clone(),
                        });
                    }
                    d.close();
                });
                vbox.append(&save_btn);
                dialog.set_child(Some(&vbox));
                dialog.present();
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
    let win_c = window.clone();
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
        win_c.close();
    });
    content.append(&create_btn);

    scroll.set_child(Some(&content));
    let vbox = Box::new(Orientation::Vertical, 0);
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
    let win_c = window.clone();
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

        win_c.close();

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
        if let Ok(tex) = gtk4::gdk::Texture::from_filename(path) {
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

    let quick_emojis = ["👍", "❤️", "😂", "😮", "😢", "🙏"];
    for emoji in &quick_emojis {
        let btn = Button::with_label(emoji);
        btn.add_css_class("flat");
        let inner_c = inner.clone();
        let msg_c = msg.clone();
        let emoji_c = emoji.to_string();
        let pop = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(cid) = inner_c.current_chat_id.borrow().clone() {
                inner_c.bridge.send_command(WaCommand::SendReaction {
                    chat_id: cid,
                    msg_id: msg_c.id.clone(),
                    emoji: emoji_c.clone(),
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

    for (_, path) in &items {
        let pic = gtk4::Picture::for_filename(path);
        pic.set_can_shrink(true);
        pic.set_content_fit(gtk4::ContentFit::Contain);
        pic.set_hexpand(true);
        pic.set_vexpand(true);
        carousel.append(&pic);
    }

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

    let close_btn = Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.set_halign(Align::End);
    let win_clone = window.clone();
    close_btn.connect_clicked(move |_| win_clone.close());

    top_bar.append(&counter);
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
        carousel.connect_page_changed(move |_, idx| {
            counter.set_text(&format!("{} / {}", idx + 1, total));
        });
    }

    // ── Keyboard navigation ──
    let key_ctrl = gtk4::EventControllerKey::new();
    {
        let c = carousel.clone();
        let total = items.len() as u32;
        let win = window.clone();
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
                win.close();
                gtk4::glib::Propagation::Stop
            }
            _ => gtk4::glib::Propagation::Proceed,
        });
    }
    window.add_controller(key_ctrl);

    // Double-click or click outside image area to close
    let bg_click = GestureClick::new();
    bg_click.set_button(1);
    let win_bg = window.clone();
    bg_click.connect_released(move |g, n_press, _, _| {
        // Double-click to close (single click navigates carousel)
        if n_press >= 2 {
            win_bg.close();
        }
    });
    carousel.add_controller(bg_click);

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
