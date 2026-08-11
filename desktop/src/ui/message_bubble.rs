use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{Align, Box, Button, GestureClick, Label, Orientation};

// gtk4::Box shadows std::boxed::Box, so alias the closure box type explicitly.
type ClickHandler = std::boxed::Box<dyn Fn()>;

/// Give a gesture-backed widget equivalent keyboard and assistive-technology
/// behaviour. GTK boxes/pictures are not focusable controls by default, so a
/// pointer-only media affordance otherwise disappears from the tab order.
fn install_keyboard_activation(
    widget: &gtk4::Widget,
    accessible_label: &str,
    action: Rc<dyn Fn()>,
) {
    widget.set_focusable(true);
    widget.set_accessible_role(gtk4::AccessibleRole::Button);
    widget.update_property(&[gtk4::accessible::Property::Label(accessible_label)]);
    widget.set_tooltip_text(Some(accessible_label));

    let key = gtk4::EventControllerKey::new();
    key.connect_key_pressed(move |_, key, _, _| {
        if matches!(
            key,
            gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::space
        ) {
            action();
            gtk4::glib::Propagation::Stop
        } else {
            gtk4::glib::Propagation::Proceed
        }
    });
    widget.add_controller(key);
}

use crate::bridge::{IncomingMessage, MediaType, ReceiptStatus};

/// Per-option widget refs stored on MessageBubble for live vote updates.
#[derive(Clone)]
struct PollOptionWidgets {
    radio: Label,
    bar_track: Box,
    bar_fill: Box,
    count_label: Label,
    /// Container for voter avatars (stacked at end of bar row)
    voters_box: Box,
}

/// Renders a single message bubble (sent or received).
#[derive(Clone)]
pub struct MessageBubble {
    root: Box,
    receipt_label: Label,
    msg_id: String,
    pub text: Option<String>,
    media_box: Option<Box>,
    media_type: Option<MediaType>,
    media_loaded: RefCell<bool>,
    /// Text as currently displayed. `text` is fixed at construction, but autocorrect
    /// rewrites the label after the fact, and the SMS echo arrives carrying the
    /// CORRECTED string — dedup has to compare against what's on screen.
    live_text: RefCell<Option<String>>,
    /// Meta row, kept so a late link-preview backfill inserts its card above the
    /// timestamp instead of rebuilding the bubble.
    meta_row: Option<Box>,
    link_preview_shown: std::cell::Cell<bool>,
    /// Live on-disk media path — set at build and by MediaReady, so the context
    /// menu works even when the message clone it captured predates the download.
    media_path: RefCell<Option<String>>,
    on_image_click: Rc<RefCell<Option<ClickHandler>>>,
    /// Invoked when the user taps the quoted-reply context box — chat_view wires
    /// this to jump/scroll to the original message (mb-01).
    on_quoted_click: Rc<RefCell<Option<ClickHandler>>>,
    /// The message id this bubble is quoting, if it's a reply.
    pub quoted_msg_id: Option<String>,
    avatar: libadwaita::Avatar,
    pub sender_id: String,
    /// The group-sender name label (received group messages only) — kept so a
    /// late name resolution can refresh it live via `update_sender_name`.
    sender_label: Option<Label>,
    /// Quick action icons + dropdown chevron (shown on hover)
    hover_actions: Box,
    chevron_btn: Button,
    pub is_from_me: bool,
    pub chat_id: String,
    /// Per-option poll widget refs for live vote updates
    poll_option_widgets: Vec<PollOptionWidgets>,
    pub poll_options: Vec<String>,
    /// Total-votes label below the poll options
    poll_total_label: Option<Label>,
    /// Poll question text (for forward-as-new-poll)
    pub poll_question: Option<String>,
    /// Poll selectable count (for forward-as-new-poll)
    pub poll_selectable: u32,
    /// Contact JID from received contact cards
    pub contact_jid: Option<String>,
    /// Contact message button — needs to be wired by chat_view
    contact_msg_btn: Option<Button>,
    /// Reference to the text label for live editing
    text_label: RefCell<Option<Label>>,
    /// "(edited)" indicator label
    edited_label: RefCell<Option<Label>>,
    /// The inner content Box — kept so update_text can lazily append a caption
    /// label to a media-only bubble that gains text via an edit.
    content_box: Option<Box>,
}

impl MessageBubble {
    pub fn new(
        msg: &IncomingMessage,
        own_display_name: &str,
        bridge: &Arc<crate::bridge::Bridge>,
    ) -> Self {
        let root = Box::new(Orientation::Vertical, 1);
        root.set_margin_top(3);
        root.set_margin_bottom(3);
        root.set_margin_start(8);
        root.set_margin_end(8);
        // Root-level direction class — used by bubble-enter slide-in
        // animation (chat_view.rs adds `bubble-enter`; CSS combines it
        // with these to pick left vs right slide direction).
        if msg.is_from_me {
            root.add_css_class("bubble-row-out");
        } else {
            root.add_css_class("bubble-row-in");
        }

        // ── System messages: centered gray text, no bubble/avatar ──
        if msg.is_system_message {
            let label = Label::new(msg.text.as_deref());
            label.add_css_class("dim-label");
            label.add_css_class("caption");
            label.set_halign(Align::Center);
            label.set_margin_top(4);
            label.set_margin_bottom(4);
            label.set_wrap(true);
            label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
            root.append(&label);
            return Self {
                root,
                receipt_label: Label::new(None),
                msg_id: msg.id.clone(),
                text: msg.text.clone(),
                media_box: None,
                media_type: None,
                media_loaded: RefCell::new(false),
                live_text: RefCell::new(msg.text.clone()),
                meta_row: None,
                link_preview_shown: std::cell::Cell::new(false),
                media_path: RefCell::new(None),
                on_image_click: Rc::new(RefCell::new(None)),
                on_quoted_click: Rc::new(RefCell::new(None)),
                quoted_msg_id: None,
                avatar: libadwaita::Avatar::new(0, None, false),
                sender_id: String::new(),
                sender_label: None,
                hover_actions: Box::new(Orientation::Horizontal, 0),
                chevron_btn: Button::new(),
                is_from_me: false,
                chat_id: msg.chat_id.clone(),
                poll_option_widgets: vec![],
                poll_options: vec![],
                poll_total_label: None,
                poll_question: None,
                poll_selectable: 0,
                contact_jid: None,
                contact_msg_btn: None,
                text_label: RefCell::new(None),
                edited_label: RefCell::new(None),
                content_box: None,
            };
        }

        // Callback invoked when the quoted-reply box is tapped (wired by
        // chat_view to jump to the original message — mb-01). Created early so
        // the reply_box gesture below can capture it.
        let on_quoted_click: Rc<RefCell<Option<ClickHandler>>> = Rc::new(RefCell::new(None));

        // Row: [avatar] [bubble] or [bubble] [avatar]
        let row = Box::new(Orientation::Horizontal, 6);
        row.set_margin_start(4);
        row.set_margin_end(4);

        // Use sender_id for consistent avatar color (sender_name can vary between messages)
        let avatar_text = if msg.is_from_me {
            own_display_name.to_string()
        } else if !msg.sender_name.is_empty() {
            msg.sender_name.clone()
        } else if !msg.sender_id.is_empty() {
            crate::ui::runtime::display_name_from_jid(&msg.sender_id)
        } else {
            "?".to_string()
        };
        let avatar = libadwaita::Avatar::new(28, Some(&avatar_text), true);
        avatar.set_valign(Align::Start);

        // Outer bubble: carries the CSS background + border-radius.
        let bubble = Box::new(Orientation::Vertical, 0);
        bubble.set_hexpand(false);

        if msg.is_from_me {
            row.set_halign(Align::End);
            bubble.add_css_class("message-bubble-out");
        } else {
            row.set_halign(Align::Start);
            bubble.add_css_class("message-bubble-in");
        }
        // Tag bubbles delivered via SMS/MMS/RCS so CSS can color them blue
        // — distinguishes them from green WhatsApp bubbles in the unified
        // chat list. Source is derived from chat_id OR message_id prefix:
        // chat_id may be rewritten to a WhatsApp JID after a Phase 2 merge,
        // but we tag the message_id with `gm:` at gmessages emit time so
        // origin survives the redirect. Keeping source off the serialized
        // struct preserves bincode compatibility with the wa_messages cache.
        if matches!(
            crate::bridge::MessageSource::from_message(&msg.chat_id, &msg.id),
            crate::bridge::MessageSource::GoogleMessages
        ) {
            bubble.add_css_class("message-bubble-sms");
        }

        // Inner content box — extra top padding so text doesn't crowd the chevron.
        // Set an explicit max width on the bubble so text labels can expand
        // to fill it naturally instead of wrapping prematurely.
        let content_wrapper = Box::new(Orientation::Vertical, 0);
        content_wrapper.set_overflow(gtk4::Overflow::Hidden);
        // Allow the wrapper to expand horizontally up to the bubble's constraint.
        // Without this, wrapping labels get crushed to 1-word-per-line.
        content_wrapper.set_hexpand(true);
        // Cap the bubble at 520px so it doesn't span the entire chat pane
        content_wrapper.set_size_request(-1, -1);
        bubble.set_size_request(-1, -1);

        let content = Box::new(Orientation::Vertical, 4);
        content.set_margin_top(6);
        content.set_margin_bottom(4);
        content.set_margin_start(10);
        content.set_margin_end(10);

        let mut stored_text_label: Option<Label> = None;
        let mut stored_edited_label: Option<Label> = None;
        let mut stored_sender_label: Option<Label> = None;
        let mut stored_contact_msg_btn: Option<Button> = None;

        content_wrapper.append(&content);

        // Forwarded badge
        if msg.is_forwarded {
            let fwd_box = Box::new(Orientation::Horizontal, 4);
            let fwd_label = Label::new(Some("↪ Forwarded"));
            fwd_label.add_css_class("caption");
            fwd_label.add_css_class("dim-label");
            fwd_box.append(&fwd_label);
            content.append(&fwd_box);
        }

        // Reply context (quoted message) — show even for media-only quotes
        if let Some(quoted_sender) = &msg.quoted_sender {
            let reply_box = Box::new(Orientation::Horizontal, 6);
            reply_box.add_css_class("reply-context");

            let text_col = Box::new(Orientation::Vertical, 2);

            let resolved_sender = if !quoted_sender.contains('@') {
                // Might be a raw phone number like "+1234567890" — try JID lookup
                let num = quoted_sender.trim_start_matches('+');
                if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                    let phone_jid = format!("{num}@s.whatsapp.net");
                    let formatted = crate::ui::runtime::display_name_from_jid(&phone_jid);
                    if formatted.contains('@') {
                        // Still unresolved — format as phone number
                        quoted_sender.clone()
                    } else {
                        formatted
                    }
                } else {
                    quoted_sender.clone()
                }
            } else {
                let formatted = crate::ui::runtime::display_name_from_jid(quoted_sender);
                if formatted.contains('@') || formatted.contains("lid") {
                    if msg.is_from_me {
                        own_display_name.to_string()
                    } else {
                        formatted
                    }
                } else {
                    formatted
                }
            };
            let sender_label = Label::new(Some(&resolved_sender));
            sender_label.add_css_class("caption");
            sender_label.add_css_class("accent");
            sender_label.set_halign(Align::Start);
            sender_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            sender_label.set_max_width_chars(50);
            text_col.append(&sender_label);

            // Show quoted text or media type indicator
            let display_text = msg.quoted_text.as_deref().unwrap_or("");
            let quoted_label = if display_text.is_empty() {
                // No text — show media type placeholder
                Label::new(Some("📎 Media"))
            } else {
                Label::new(Some(display_text))
            };
            quoted_label.add_css_class("caption");
            quoted_label.add_css_class("dim-label");
            quoted_label.set_halign(Align::Start);
            quoted_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            quoted_label.set_max_width_chars(50);
            quoted_label.set_lines(2);
            quoted_label.set_wrap(true);
            quoted_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
            text_col.append(&quoted_label);

            reply_box.append(&text_col);

            // Show media thumbnail in quote if available
            let mut thumb_loaded = false;
            // Source 1: direct quoted_media_path (set by our reply flow)
            if let Some(ref path) = msg.quoted_media_path {
                if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(path, 96) {
                    let thumb = gtk4::Picture::new();
                    thumb.set_paintable(Some(&tex));
                    thumb.set_size_request(72, 72);
                    thumb.set_can_shrink(true);
                    thumb.set_content_fit(gtk4::ContentFit::Cover);
                    thumb.set_halign(Align::End);
                    reply_box.append(&thumb);
                    thumb_loaded = true;
                }
            }
            // Source 2: deterministic wa_media/{qid}.<ext> lookup.
            // Previously this fell back to a blocking std::fs::read_dir over the
            // ENTIRE wa_media directory (which grows with every attachment) on
            // the GTK main thread during bubble construction — a history load of
            // N reply bubbles was N * dir-size of blocking work. Media is stored
            // keyed by the full quoted_msg_id, so we probe only the handful of
            // known extensions instead of scanning the whole dir.
            if !thumb_loaded {
                if let Some(ref qid) = msg.quoted_msg_id {
                    let media_dir = std::path::PathBuf::from("wa_media");
                    if media_dir.exists() {
                        for ext in ["jpeg", "jpg", "png", "webp"] {
                            let candidate = media_dir.join(format!("{qid}.{ext}"));
                            if candidate.exists() {
                                if let Some(tex) =
                                    crate::ui::texture_cache::texture_thumbnail(&candidate, 96)
                                {
                                    let thumb = gtk4::Picture::new();
                                    thumb.set_paintable(Some(&tex));
                                    thumb.set_size_request(72, 72);
                                    thumb.set_can_shrink(true);
                                    thumb.set_content_fit(gtk4::ContentFit::Cover);
                                    thumb.set_halign(Align::End);
                                    reply_box.append(&thumb);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Make the quoted-reply box tap-to-jump: pointer cursor + a left
            // click gesture that fires the (chat_view-supplied) callback. Inert
            // until wired, so on its own this only adds the cursor affordance.
            reply_box.set_cursor_from_name(Some("pointer"));
            let reply_gesture = GestureClick::new();
            reply_gesture.set_button(1);
            let quoted_cb = on_quoted_click.clone();
            let activate_quote: Rc<dyn Fn()> = Rc::new(move || {
                let borrow = quoted_cb.borrow();
                if let Some(f) = borrow.as_ref() {
                    f();
                }
            });
            let activate_click = activate_quote.clone();
            reply_gesture.connect_released(move |_, _, _, _| activate_click());
            reply_box.add_controller(reply_gesture);
            install_keyboard_activation(
                reply_box.upcast_ref(),
                "Jump to quoted message",
                activate_quote,
            );

            content.append(&reply_box);
        }

        // Sender name (in groups, for incoming messages)
        // Always show in groups — fall back to formatted JID if push_name unavailable
        if !msg.is_from_me && msg.chat_id.ends_with("@g.us") {
            let display_name = if msg.sender_name.is_empty() {
                // Fall back to phone number from sender JID
                crate::ui::runtime::display_name_from_jid(&msg.sender_id)
            } else {
                msg.sender_name.clone()
            };
            let sender = Label::new(Some(&display_name));
            sender.add_css_class("caption");
            sender.set_halign(Align::Start);
            sender.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            sender.set_max_width_chars(50);
            sender.set_max_width_chars(50);
            // Colour the sender name based on a hash of their ID (matches avatar colour)
            let colour = name_to_colour(&msg.sender_id);
            sender.set_markup(&format!(
                "<span foreground='{colour}'><b>{}</b></span>",
                glib::markup_escape_text(&display_name)
            ));
            content.append(&sender);
            stored_sender_label = Some(sender);
        }

        // Poll widget
        let mut poll_widget_refs: Vec<PollOptionWidgets> = Vec::new();
        let mut poll_total_label_ref: Option<Label> = None;
        if let Some(ref question) = msg.poll_question {
            if !msg.poll_options.is_empty() {
                let poll_box = Box::new(Orientation::Vertical, 4);
                poll_box.set_size_request(280, -1);
                poll_box.set_margin_top(4);
                poll_box.set_margin_bottom(2);
                poll_box.add_css_class("poll-widget");

                let q_label = Label::new(None);
                q_label.set_markup(&format!(
                    "<span size='large' weight='bold'>📊 {}</span>",
                    glib::markup_escape_text(question)
                ));
                q_label.set_halign(Align::Start);
                q_label.set_wrap(true);
                q_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                q_label.set_max_width_chars(50);
                q_label.set_margin_bottom(2);
                poll_box.append(&q_label);

                let sel_text = if msg.poll_selectable <= 1 {
                    "Select one"
                } else {
                    "Select multiple"
                };
                let sel_label = Label::new(Some(sel_text));
                sel_label.add_css_class("dim-label");
                sel_label.add_css_class("caption");
                sel_label.set_halign(Align::Start);
                sel_label.set_margin_bottom(4);
                poll_box.append(&sel_label);

                let is_single = msg.poll_selectable <= 1;
                let total_options = msg.poll_options.len();

                // Create total label early so click handlers can reference it
                let total_label = Label::new(Some("0 votes"));
                total_label.add_css_class("dim-label");
                total_label.add_css_class("caption");
                total_label.set_halign(Align::Start);
                total_label.set_margin_top(4);
                let total_label_rc: Rc<Label> = Rc::new(total_label);
                poll_total_label_ref = Some((*total_label_rc).clone());

                // Shared state: which options are selected
                let selected: Rc<RefCell<Vec<bool>>> =
                    Rc::new(RefCell::new(vec![false; total_options]));
                // Shared refs to per-option widgets for the click handler
                let click_widgets: Rc<RefCell<Vec<PollOptionWidgets>>> =
                    Rc::new(RefCell::new(Vec::new()));
                // Shared, full poll-vote state (every voter, not just us) so an
                // optimistic click MERGES our vote into everyone else's instead
                // of wiping them (poll-vote-wipes-other-voters). Seeded from the
                // persisted votes; the server echo later replaces it wholesale.
                let live_votes: Rc<RefCell<Vec<(String, Vec<String>)>>> =
                    Rc::new(RefCell::new(msg.poll_votes.clone()));
                // Key we use for our own voter row in the local merge. Must not
                // collide with a real voter's display name.
                let own_vote_key = own_display_name.to_string();

                for (idx, opt) in msg.poll_options.iter().enumerate() {
                    let opt_row = Box::new(Orientation::Vertical, 2);

                    // Option button: [radio] [label] [count]
                    let opt_btn = Button::new();
                    opt_btn.add_css_class("flat");
                    let opt_hbox = Box::new(Orientation::Horizontal, 8);
                    let radio = Label::new(None);
                    radio.set_use_markup(true);
                    radio.set_markup("<span size='x-large' foreground='#8696a0'>◯</span>");
                    let opt_label = Label::new(Some(opt));
                    opt_label.set_halign(Align::Start);
                    opt_label.set_hexpand(true);
                    opt_label.set_wrap(true);
                    opt_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                    opt_label.set_max_width_chars(50);
                    let count_label = Label::new(None);
                    count_label.add_css_class("dim-label");
                    count_label.add_css_class("caption");
                    opt_hbox.append(&radio);
                    opt_hbox.append(&opt_label);
                    opt_hbox.append(&count_label);
                    opt_btn.set_child(Some(&opt_hbox));

                    // Bar row: [bar_track (expands)] [voter avatars]
                    let bar_row = Box::new(Orientation::Horizontal, 4);
                    bar_row.set_margin_start(28);
                    bar_row.set_margin_end(8);
                    bar_row.set_visible(false);
                    bar_row.set_valign(Align::Center);

                    let bar_track = Box::new(Orientation::Horizontal, 0);
                    bar_track.set_size_request(-1, 6);
                    bar_track.set_hexpand(true);
                    bar_track.set_valign(Align::Center);
                    bar_track.set_overflow(gtk4::Overflow::Hidden);
                    bar_track.add_css_class("poll-bar-track");

                    let bar_fill = Box::new(Orientation::Horizontal, 0);
                    bar_fill.set_size_request(0, 6);
                    bar_fill.add_css_class("poll-bar-fill");
                    bar_track.append(&bar_fill);

                    let voters_box = Box::new(Orientation::Horizontal, 0);
                    voters_box.set_valign(Align::Center);
                    voters_box.add_css_class("poll-voters");

                    bar_row.append(&bar_track);
                    bar_row.append(&voters_box);

                    opt_row.append(&opt_btn);
                    opt_row.append(&bar_row);

                    // Store widget refs for both the struct and the click handler
                    let pw = PollOptionWidgets {
                        radio: radio.clone(),
                        bar_track: bar_row.clone(),
                        bar_fill: bar_fill.clone(),
                        count_label: count_label.clone(),
                        voters_box: voters_box.clone(),
                    };
                    poll_widget_refs.push(pw.clone());
                    click_widgets.borrow_mut().push(pw);

                    // Click handler — update UI immediately AND send vote to WhatsApp
                    let sel = selected.clone();
                    let wdg = click_widgets.clone();
                    let votes_state = live_votes.clone();
                    let is_single_c = is_single;
                    let bridge_c = bridge.clone();
                    let chat_id_c = msg.chat_id.clone();
                    let msg_id_c = msg.id.clone();
                    let sender_id_c = if msg.is_from_me {
                        "self".to_string()
                    } else {
                        msg.sender_id.clone()
                    };
                    let poll_secret_c = msg.poll_secret.clone();
                    let all_options = msg.poll_options.clone();
                    let total_lbl_c = total_label_rc.clone();
                    let own_key_c = own_vote_key.clone();
                    opt_btn.connect_clicked(move |_| {
                        let mut s = sel.borrow_mut();
                        if is_single_c {
                            for (i, v) in s.iter_mut().enumerate() {
                                *v = i == idx;
                            }
                        } else {
                            s[idx] = !s[idx];
                        }

                        // Options we (locally) now have selected.
                        let own_selected: Vec<String> = s
                            .iter()
                            .enumerate()
                            .filter(|(_, v)| **v)
                            .map(|(i, _)| all_options[i].clone())
                            .collect();

                        // Merge our vote into the FULL vote set: drop our old row,
                        // re-add it with the current selection (if any). Other
                        // voters' rows are left untouched so their counts/avatars
                        // and proportional bars stay visible.
                        {
                            let mut v = votes_state.borrow_mut();
                            v.retain(|(voter, _)| voter != &own_key_c);
                            if !own_selected.is_empty() {
                                v.push((own_key_c.clone(), own_selected.clone()));
                            }
                        }

                        // Re-render every option widget from the merged truth using
                        // the shared proportional renderer (real fills, real avatars).
                        let w = wdg.borrow();
                        let merged = votes_state.borrow();
                        Self::apply_votes_to_widgets(
                            w.as_slice(),
                            &all_options,
                            merged.as_slice(),
                            Some(&*total_lbl_c),
                        );
                        drop(merged);
                        drop(w);

                        // Send vote to WhatsApp
                        if !poll_secret_c.is_empty() {
                            bridge_c.send_command(crate::bridge::WaCommand::VotePoll {
                                chat_id: chat_id_c.clone(),
                                poll_msg_id: msg_id_c.clone(),
                                poll_creator: sender_id_c.clone(),
                                poll_secret: poll_secret_c.clone(),
                                selected_options: own_selected,
                            });
                        }
                    });

                    poll_box.append(&opt_row);
                }

                // Append total label (created earlier, shared with click handlers)
                poll_box.append(&*total_label_rc);

                // Apply existing votes from persisted data
                if !msg.poll_votes.is_empty() {
                    Self::apply_votes_to_widgets(
                        &poll_widget_refs,
                        &msg.poll_options,
                        &msg.poll_votes,
                        poll_total_label_ref.as_ref(),
                    );
                }

                content.append(&poll_box);
            }
        }

        let on_image_click: Rc<RefCell<Option<ClickHandler>>> = Rc::new(RefCell::new(None));

        // Whether the stored media file ACTUALLY exists on disk right now. The
        // stale `media_local_path` can point at a cleared/missing file, so we
        // derive this exists-filtered value once and reuse it both for the
        // placeholder decision below AND the `media_loaded` seed — otherwise a
        // deleted file seeds media_loaded=true and re-download is a dead end (B2).
        let existing_path = msg
            .media_local_path
            .as_deref()
            .filter(|p| std::path::Path::new(p).exists());
        let media_exists_on_disk = existing_path.is_some();
        let existing_media_path_for_menu = existing_path.map(str::to_string);

        // Media widget
        let media_box = if msg.media_type.is_some() {
            let mb = Box::new(Orientation::Vertical, 4);
            // Only treat the media as present if the file ACTUALLY exists on
            // disk — a cleared/missing file used to render as a permanent blank
            // box. When it's gone we fall through to the re-download placeholder.
            if let Some(path) = existing_path {
                build_media_content(&mb, path, msg.media_type.as_ref().unwrap(), &on_image_click);
            } else if msg.media_download.is_some() {
                // No local file yet, but we hold the decryption keys — make the
                // placeholder clickable to fetch the attachment on demand.
                mb.append(&build_downloadable_placeholder(msg, bridge));
            } else {
                mb.append(&media_placeholder_label(msg));
            }
            content.append(&mb);
            Some(mb)
        } else {
            None
        };

        // Message text / caption (skip for polls — question is already shown in the poll widget)
        let is_poll_msg = msg.poll_question.is_some() && !msg.poll_options.is_empty();
        let text_to_show = if is_poll_msg {
            None
        } else {
            msg.media_caption
                .as_deref()
                .filter(|_| msg.media_type.is_some() && msg.text.is_none())
                .or_else(|| msg.text.as_deref())
        };

        if let Some(text) = text_to_show {
            // Single emoji (1-3 chars, all emoji) → show large without bubble styling
            let is_single_emoji = {
                let chars: Vec<char> = text.chars().collect();
                chars.len() <= 3
                    && chars.iter().all(|c| {
                        let cp = *c as u32;
                        // Emoji ranges: emoticons, symbols, transport, misc, dingbats, supplemental, flags
                        (0x1F600..=0x1F64F).contains(&cp)
                            || (0x1F300..=0x1F5FF).contains(&cp)
                            || (0x1F680..=0x1F6FF).contains(&cp)
                            || (0x1F900..=0x1F9FF).contains(&cp)
                            || (0x2600..=0x27BF).contains(&cp)
                            || (0x2702..=0x27B0).contains(&cp)
                            || (0xFE00..=0xFE0F).contains(&cp)
                            || (0x200D..=0x200D).contains(&cp)
                            || (0x1FA00..=0x1FA6F).contains(&cp)
                            || (0x1FA70..=0x1FAFF).contains(&cp)
                            || (0x2764..=0x2764).contains(&cp)
                            || (0x1F1E0..=0x1F1FF).contains(&cp)
                            || (0xE0020..=0xE007F).contains(&cp)
                    })
                    && !chars.is_empty()
                    && msg.media_type.is_none()
                    && msg.quoted_text.is_none()
            };

            let text_label = Label::new(None);
            let markup = format_whatsapp_markup(text);
            // Guard against malformed markup blanking the bubble — fall back to
            // the raw text so the message is never lost.
            set_markup_safe(&text_label, &markup, text);
            // Open link clicks through our hardened launcher (setsid + detached)
            // instead of GTK's default gtk_show_uri, for consistent behaviour
            // with the rest of the app (mb-12).
            text_label.connect_activate_link(|_, uri| {
                open_url(uri);
                gtk4::glib::Propagation::Stop
            });

            if is_single_emoji {
                text_label.add_css_class("title-1");
                text_label.set_halign(Align::Start);
                bubble.remove_css_class("message-bubble-out");
                bubble.remove_css_class("message-bubble-in");
            } else {
                text_label.set_wrap(true);
                text_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                text_label.set_max_width_chars(48);
                text_label.set_hexpand(true);
            }
            text_label.set_halign(Align::Start);
            text_label.set_selectable(true);
            text_label.set_xalign(0.0);
            content.append(&text_label);
            stored_text_label = Some(text_label);

            // Show "(edited)" badge if the message was previously edited
            if msg.is_edited {
                let edited = Label::new(Some("edited"));
                edited.add_css_class("caption");
                edited.add_css_class("dim-label");
                edited.set_halign(Align::End);
                content.append(&edited);
                stored_edited_label = Some(edited);
            }
        }

        // Contact card — specialized look (no standard bubble background)
        if let Some(contact_name) = &msg.contact_name {
            // Remove standard bubble styling for contact cards
            bubble.remove_css_class("message-bubble-out");
            bubble.remove_css_class("message-bubble-in");

            let card = Box::new(Orientation::Vertical, 0);
            card.add_css_class("message-bubble-in"); // Use incoming style for the card
            card.set_margin_top(4);

            // Top area: avatar + name + phone
            let top = Box::new(Orientation::Horizontal, 12);
            top.set_margin_start(12);
            top.set_margin_end(12);
            top.set_margin_top(12);
            top.set_margin_bottom(8);
            let av = libadwaita::Avatar::new(52, Some(contact_name), true);
            let info = Box::new(Orientation::Vertical, 2);
            let name_lbl = Label::new(Some(contact_name));
            name_lbl.add_css_class("heading");
            name_lbl.set_halign(Align::Start);
            info.append(&name_lbl);

            // Extract phone from vcard
            if let Some(vcard) = &msg.contact_vcard {
                for line in vcard.lines() {
                    if line.starts_with("TEL") {
                        if let Some(phone) = line.rsplit(':').next() {
                            let phone = phone.trim();
                            if !phone.is_empty() {
                                let phone_lbl = Label::new(Some(phone));
                                phone_lbl.add_css_class("dim-label");
                                phone_lbl.set_halign(Align::Start);
                                info.append(&phone_lbl);
                            }
                        }
                    }
                }
            }
            top.append(&av);
            top.append(&info);
            card.append(&top);

            // Separator
            card.append(&gtk4::Separator::new(Orientation::Horizontal));

            // "Message" button — full width, styled, with widget_name = JID for wiring
            let msg_btn = Button::with_label("Message");
            msg_btn.add_css_class("flat");
            msg_btn.set_margin_start(8);
            msg_btn.set_margin_end(8);
            msg_btn.set_margin_top(6);
            msg_btn.set_margin_bottom(6);
            // Store the JID in widget_name so chat_view can find and wire it
            if let Some(jid) = &msg.contact_vcard.as_ref().and_then(|vc| {
                vc.lines()
                    .find(|l| l.contains("waid="))
                    .and_then(|l| l.split("waid=").nth(1))
                    .and_then(|s| s.split(&[':', ';'][..]).next())
                    .map(|w| format!("{w}@s.whatsapp.net"))
            }) {
                msg_btn.set_widget_name(jid);
            }
            stored_contact_msg_btn = Some(msg_btn.clone());
            card.append(&msg_btn);

            content.append(&card);
        }

        // Link preview rendering
        if let Some(url) = &msg.link_url {
            content.append(&build_link_preview_card(
                url,
                msg.link_title.as_deref(),
                msg.link_description.as_deref(),
                msg.link_thumbnail_path.as_deref(),
            ));
        }

        // Bottom row: time + receipt
        let meta_row = Box::new(Orientation::Horizontal, 4);
        meta_row.set_halign(Align::End);
        // Named so a lazily-added edit caption can be inserted ABOVE it (below).
        meta_row.set_widget_name("meta-row");

        let time_label = Label::new(Some(&format_time(msg.timestamp)));
        time_label.add_css_class("caption");
        time_label.add_css_class("dim-label");

        let receipt_text = if msg.is_from_me {
            match &msg.receipt_status {
                crate::bridge::ReceiptStatus::Pending => "🕐",
                crate::bridge::ReceiptStatus::Sent => "✓",
                crate::bridge::ReceiptStatus::Delivered => "✓✓",
                crate::bridge::ReceiptStatus::Read => "✓✓",
                crate::bridge::ReceiptStatus::Failed => "✗",
            }
        } else {
            ""
        };
        let receipt_label = Label::new(Some(receipt_text));
        receipt_label.add_css_class("caption");
        receipt_label.add_css_class("dim-label");

        meta_row.append(&time_label);
        if msg.is_from_me {
            meta_row.append(&receipt_label);
        }

        // ── Dropdown chevron — always visible, top-right of bubble ──
        let chevron_btn = Button::from_icon_name("go-down-symbolic");
        chevron_btn.add_css_class("flat");
        chevron_btn.set_tooltip_text(Some("More"));
        chevron_btn.set_halign(Align::End);
        chevron_btn.set_valign(Align::Start);
        chevron_btn.set_margin_end(2);
        chevron_btn.set_margin_top(2);
        chevron_btn.set_opacity(0.5);
        // Pointer cursor so the chevron reads as a clickable control (mb-09).
        chevron_btn.set_cursor_from_name(Some("pointer"));

        // Overlay the chevron on top of the bubble
        let bubble_overlay = gtk4::Overlay::new();
        bubble_overlay.set_child(Some(&bubble));
        bubble_overlay.add_overlay(&chevron_btn);

        // Hard pixel cap on bubble width. Clamp limits allocation to 380px.
        // No halign set — the parent row handles Start/End alignment.
        let bubble_limiter = libadwaita::Clamp::new();
        bubble_limiter.set_maximum_size(380);
        bubble_limiter.set_tightening_threshold(380); // No tightening — hard cap only
        bubble_limiter.set_child(Some(&bubble_overlay));

        content.append(&meta_row);
        bubble.append(&content_wrapper);

        // ── Quick action icons (vertical, shown on hover) ──
        let hover_actions = Box::new(Orientation::Horizontal, 2);
        hover_actions.set_valign(Align::Start);
        hover_actions.set_visible(false);

        let btn_react = Button::with_label("☺");
        btn_react.add_css_class("flat");
        btn_react.add_css_class("circular");
        btn_react.set_tooltip_text(Some("React"));

        let btn_reply = Button::with_label("↩");
        btn_reply.add_css_class("flat");
        btn_reply.add_css_class("circular");
        btn_reply.set_tooltip_text(Some("Reply"));

        let btn_forward = Button::from_icon_name("mail-forward-symbolic");
        btn_forward.add_css_class("flat");
        btn_forward.add_css_class("circular");
        btn_forward.add_css_class("suggested-action");
        btn_forward.set_size_request(32, 32);
        btn_forward.set_tooltip_text(Some("Forward"));

        hover_actions.append(&btn_react);
        hover_actions.append(&btn_reply);
        hover_actions.append(&btn_forward);

        // Layout: [avatar] [bubble+chevron] [actions] or [actions] [bubble+chevron] [avatar]
        if msg.is_from_me {
            row.append(&hover_actions);
            row.append(&bubble_limiter);
            row.append(&avatar);
        } else {
            row.append(&avatar);
            row.append(&bubble_limiter);
            row.append(&hover_actions);
        }

        root.append(&row);

        // ── Reaction pills (shown below the bubble) ──
        if let Some(reaction_row) = build_reaction_row(&msg.reactions, msg.is_from_me) {
            root.append(&reaction_row);
        }

        // ── Hover show/hide for quick actions — disabled in forward mode ──
        let motion = gtk4::EventControllerMotion::new();
        let actions_ref = hover_actions.clone();
        // Raise the (normally faint) chevron to full opacity on hover for
        // discoverability; restored to 0.5 on leave (mb-09).
        let chevron_hover = chevron_btn.clone();
        // Get widget from controller at callback time — never capture widget in its
        // own controller's closure (creates ref cycle → memory leak).
        motion.connect_enter(move |ctrl, _, _| {
            if let Some(w) = ctrl.widget() {
                if let Some(parent) = w.parent() {
                    if parent.has_css_class("forward-mode") {
                        return;
                    }
                }
            }
            actions_ref.set_visible(true);
            chevron_hover.set_opacity(1.0);
        });
        let actions_ref2 = hover_actions.clone();
        let chevron_leave = chevron_btn.clone();
        motion.connect_leave(move |_| {
            actions_ref2.set_visible(false);
            chevron_leave.set_opacity(0.5);
        });
        root.add_controller(motion);

        let bubble = Self {
            root,
            receipt_label,
            msg_id: msg.id.clone(),
            text: msg.text.clone().or_else(|| msg.media_caption.clone()),
            media_box,
            media_type: msg.media_type.clone(),
            media_loaded: RefCell::new(media_exists_on_disk),
            live_text: RefCell::new(msg.text.clone().or_else(|| msg.media_caption.clone())),
            meta_row: Some(meta_row.clone()),
            link_preview_shown: std::cell::Cell::new(msg.link_url.is_some()),
            media_path: RefCell::new(
                existing_media_path_for_menu,
            ),
            on_image_click,
            on_quoted_click,
            quoted_msg_id: msg.quoted_msg_id.clone(),
            avatar,
            sender_id: if msg.is_from_me {
                String::new()
            } else {
                msg.sender_id.clone()
            },
            sender_label: stored_sender_label,
            hover_actions,
            chevron_btn,
            is_from_me: msg.is_from_me,
            chat_id: msg.chat_id.clone(),
            poll_option_widgets: poll_widget_refs,
            poll_options: msg.poll_options.clone(),
            poll_total_label: poll_total_label_ref,
            poll_question: msg.poll_question.clone(),
            poll_selectable: msg.poll_selectable,
            contact_jid: msg.contact_vcard.as_ref().and_then(|vc| {
                vc.lines()
                    .find(|l| l.contains("waid="))
                    .and_then(|l| l.split("waid=").nth(1))
                    .and_then(|s| s.split(&[':', ';'][..]).next())
                    .map(|waid| format!("{waid}@s.whatsapp.net"))
            }),
            contact_msg_btn: stored_contact_msg_btn,
            text_label: RefCell::new(stored_text_label),
            edited_label: RefCell::new(stored_edited_label),
            content_box: Some(content),
        };
        // Paint the receipt tick from the persisted status at construction, so
        // read (blue ✓✓) history renders correctly on restart / history load —
        // previously ticks stayed gray until a live receipt arrived this session.
        if bubble.is_from_me {
            bubble.update_receipt(&msg.receipt_status);
        }
        bubble
    }

    /// Get the contact "Message" button if this bubble has a contact card
    pub fn contact_message_button(&self) -> Option<&Button> {
        self.contact_msg_btn.as_ref()
    }

    /// Update the text of this bubble (for message edits) and show "(edited)" badge.
    pub fn update_text(&self, new_text: &str, show_edited: bool) {
        *self.live_text.borrow_mut() = Some(new_text.to_string());
        let markup = format_whatsapp_markup(new_text);
        // If the bubble had no text label (a media-only / no-caption message
        // that just gained a caption via edit), lazily create one and append it
        // to the content box so the new text — and the "(edited)" badge below —
        // actually appear instead of silently no-oping (edited-badge fix).
        if self.text_label.borrow().is_none() {
            if let Some(content) = &self.content_box {
                let label = Label::new(None);
                set_markup_safe(&label, &markup, new_text);
                label.set_wrap(true);
                label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                label.set_max_width_chars(48);
                label.set_hexpand(true);
                label.set_halign(Align::Start);
                label.set_selectable(true);
                label.set_xalign(0.0);
                // Insert above the time/ticks meta row instead of appending
                // after it, so the new caption renders in the right place.
                let meta_row = {
                    let mut found = None;
                    let mut child = content.first_child();
                    while let Some(c) = child {
                        if c.widget_name() == "meta-row" {
                            found = Some(c);
                            break;
                        }
                        child = c.next_sibling();
                    }
                    found
                };
                if let Some(meta_row) = meta_row {
                    label.insert_before(content, Some(&meta_row));
                } else {
                    content.append(&label);
                }
                *self.text_label.borrow_mut() = Some(label);
            }
        } else if let Some(label) = self.text_label.borrow().as_ref() {
            set_markup_safe(label, &markup, new_text);
        }
        if show_edited && self.edited_label.borrow().is_none() {
            // Add "(edited)" label next to the text
            if let Some(text_label) = self.text_label.borrow().as_ref() {
                if let Some(parent) = text_label.parent().and_then(|p| p.downcast::<Box>().ok()) {
                    let edited = Label::new(Some("edited"));
                    edited.add_css_class("caption");
                    edited.add_css_class("dim-label");
                    edited.set_halign(gtk4::Align::End);
                    parent.append(&edited);
                    *self.edited_label.borrow_mut() = Some(edited);
                }
            }
        }
    }

    /// Update the existing per-option widgets with vote data (no new elements created).
    fn apply_votes_to_widgets(
        widgets: &[PollOptionWidgets],
        options: &[String],
        votes: &[(String, Vec<String>)],
        total_label: Option<&Label>,
    ) {
        // Build per-option voter lists
        let mut option_voters: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for (voter, selected) in votes {
            for opt in selected {
                option_voters
                    .entry(opt.as_str())
                    .or_default()
                    .push(voter.as_str());
            }
        }

        let total_voters = votes.len();

        for (i, pw) in widgets.iter().enumerate() {
            if let Some(opt) = options.get(i) {
                let voters = option_voters.get(opt.as_str());
                let count = voters.map(|v| v.len()).unwrap_or(0);
                let fill_width = if total_voters > 0 {
                    (count * 250) / total_voters
                } else {
                    0
                };

                // Update radio indicator
                if count > 0 {
                    pw.radio
                        .set_markup("<span size='x-large' foreground='#25D366'>●</span>");
                    pw.count_label.set_text(&format!("{count}"));
                } else {
                    pw.radio
                        .set_markup("<span size='x-large' foreground='#8696a0'>◯</span>");
                    pw.count_label.set_text("");
                }

                // Show and size the bar
                pw.bar_track.set_visible(total_voters > 0);
                pw.bar_fill.set_size_request(fill_width as i32, 6);

                // Rebuild voter avatars
                while let Some(child) = pw.voters_box.first_child() {
                    pw.voters_box.remove(&child);
                }
                if let Some(voter_names) = voters {
                    let max_show = 4;
                    for (j, name) in voter_names.iter().enumerate() {
                        if j >= max_show {
                            break;
                        }
                        let av = libadwaita::Avatar::new(20, Some(name), true);
                        av.set_size_request(20, 20);
                        pw.voters_box.append(&av);
                    }
                    if voter_names.len() > max_show {
                        let overflow =
                            Label::new(Some(&format!("+{}", voter_names.len() - max_show)));
                        overflow.add_css_class("caption");
                        overflow.add_css_class("dim-label");
                        pw.voters_box.append(&overflow);
                    }
                }
            }
        }

        if let Some(lbl) = total_label {
            lbl.set_text(&format!(
                "{total_voters} vote{}",
                if total_voters != 1 { "s" } else { "" }
            ));
        }
    }

    /// Update poll votes from a live vote event — updates existing widgets in place
    pub fn update_poll_votes(&self, votes: &[(String, Vec<String>)]) {
        log::info!(
            "update_poll_votes: {} voters, {} option widgets, total_label={}",
            votes.len(),
            self.poll_option_widgets.len(),
            self.poll_total_label.is_some()
        );
        if !self.poll_option_widgets.is_empty() {
            Self::apply_votes_to_widgets(
                &self.poll_option_widgets,
                &self.poll_options,
                votes,
                self.poll_total_label.as_ref(),
            );
        }
    }

    pub fn widget(&self) -> &Box {
        &self.root
    }

    pub fn msg_id(&self) -> &str {
        &self.msg_id
    }

    pub fn chevron_button(&self) -> &Button {
        &self.chevron_btn
    }

    /// Get the quick action button box children: (react, reply, forward).
    /// Returns None for system-message bubbles which have no hover actions.
    pub fn quick_action_buttons(&self) -> Option<(Button, Button, Button)> {
        let mut children = Vec::new();
        let mut child = self.hover_actions.first_child();
        while let Some(c) = child {
            if let Ok(btn) = c.clone().downcast::<Button>() {
                children.push(btn);
            }
            child = c.next_sibling();
        }
        if children.len() < 3 {
            return None;
        }
        Some((
            children[0].clone(),
            children[1].clone(),
            children[2].clone(),
        ))
    }

    pub fn avatar_widget(&self) -> &libadwaita::Avatar {
        &self.avatar
    }

    pub fn set_avatar_image(&self, path: &str) {
        if let Some(texture) = crate::ui::texture_cache::texture_thumbnail(path, 96) {
            self.avatar.set_custom_image(Some(&texture));
        }
    }

    /// Set the callback invoked when the user clicks an image/gif/sticker bubble.
    /// Called by chat_view after creation so it can pass a closure over the media list.
    pub fn set_image_click_handler(&self, f: impl Fn() + 'static) {
        *self.on_image_click.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    /// Set the callback invoked when the user taps this bubble's quoted-reply
    /// context box. chat_view wires this to scroll to `quoted_msg_id` (mb-01).
    pub fn set_quoted_click_handler(&self, f: impl Fn() + 'static) {
        *self.on_quoted_click.borrow_mut() = Some(std::boxed::Box::new(f));
    }

    pub fn update_receipt(&self, status: &ReceiptStatus) {
        let tick = match status {
            ReceiptStatus::Pending => "🕐",
            ReceiptStatus::Sent => "✓",
            ReceiptStatus::Delivered => "✓✓",
            ReceiptStatus::Read => "✓✓",
            ReceiptStatus::Failed => "✗",
        };
        self.receipt_label.set_text(tick);
        self.receipt_label.remove_css_class("dim-label");
        self.receipt_label.remove_css_class("accent");
        self.receipt_label.remove_css_class("error");
        match status {
            ReceiptStatus::Read => {
                self.receipt_label.add_css_class("accent");
            }
            ReceiptStatus::Failed => {
                self.receipt_label.add_css_class("error");
            }
            _ => {
                self.receipt_label.add_css_class("dim-label");
            }
        }
    }

    /// True if this bubble is in a failed-send state (shows red ✗).
    pub fn is_failed(&self) -> bool {
        self.receipt_label.has_css_class("error")
    }

    /// Refresh the group-sender name label if this bubble is from `sender_id`
    /// (late name resolution — a raw number becomes the real name in place).
    pub fn update_sender_name(&self, sender_id: &str, name: &str) {
        if self.sender_id == sender_id
            && let Some(label) = &self.sender_label
        {
            let colour = name_to_colour(&self.sender_id);
            label.set_markup(&format!(
                "<span foreground='{colour}'><b>{}</b></span>",
                glib::markup_escape_text(name)
            ));
        }
    }

    /// Replace this bubble's reaction row from the message's FULL deduped
    /// reactions vec. Fixes live reactions that previously appended a second
    /// row and never deduped/removed: the old row is removed and rebuilt (or
    /// dropped entirely when `reactions` is empty).
    pub fn rebuild_reactions(&self, reactions: &[(String, String)]) {
        let mut child = self.root.first_child();
        while let Some(c) = child {
            let next = c.next_sibling();
            if c.widget_name() == "reaction-row" {
                self.root.remove(&c);
            }
            child = next;
        }
        if let Some(row) = build_reaction_row(reactions, self.is_from_me) {
            self.root.append(&row);
        }
    }

    /// Replace the media placeholder with the actual downloaded file.
    pub fn set_media_loaded(&self, path: &str) {
        *self.media_path.borrow_mut() = Some(path.to_string());
        if *self.media_loaded.borrow() {
            return;
        }
        let Some(mb) = &self.media_box else {
            return;
        };
        let Some(mt) = &self.media_type else {
            return;
        };

        while let Some(child) = mb.first_child() {
            mb.remove(&child);
        }
        build_media_content(mb, path, mt, &self.on_image_click);
        *self.media_loaded.borrow_mut() = true;
    }

    /// Attach a link-preview card to an already-rendered bubble (the sender sent
    /// a bare link and we fetched the metadata afterwards).
    pub fn set_link_preview(
        &self,
        url: &str,
        title: Option<&str>,
        description: Option<&str>,
        thumbnail_url: Option<&str>,
    ) {
        if self.link_preview_shown.get() {
            return;
        }
        self.link_preview_shown.set(true);
        let (Some(content), Some(meta_row)) = (&self.content_box, &self.meta_row) else {
            return;
        };
        let card = build_link_preview_card(url, title, description, thumbnail_url);
        content.append(&card);
        content.reorder_child_after(meta_row, Some(&card));
    }

    /// Text as currently rendered (autocorrect may have rewritten it post-send).
    pub fn live_text(&self) -> Option<String> {
        self.live_text.borrow().clone()
    }

    /// On-disk media path if the file is present (live view — survives the
    /// stale message clones captured by menu closures).
    pub fn media_path(&self) -> Option<String> {
        self.media_path
            .borrow()
            .clone()
            .filter(|p| std::path::Path::new(p).exists())
    }

    /// True if this bubble holds visual media (image / gif / sticker).
    pub fn is_visual_media(&self) -> bool {
        matches!(
            &self.media_type,
            Some(MediaType::Image | MediaType::Sticker)
        )
    }
}

// ── Media content builders ────────────────────────────────────────────────────

/// Build the actual media content widget into `container`.
fn build_media_content(
    container: &Box,
    path: &str,
    media_type: &MediaType,
    on_image_click: &Rc<RefCell<Option<ClickHandler>>>,
) {
    match media_type {
        MediaType::Image | MediaType::Sticker => {
            let css = if matches!(media_type, MediaType::Sticker) {
                "media-preview-sticker"
            } else {
                "media-preview"
            };
            build_image_widget(container, path, css, on_image_click);
        }
        MediaType::Gif => {
            build_gif_widget(container, path);
        }
        MediaType::Video => {
            build_video_widget(container, path);
        }
        MediaType::Audio => {
            build_audio_widget(container, path);
        }
        MediaType::Document => {
            build_document_widget(container, path);
        }
    }
}

/// Render an image with a click-to-expand gesture.
///
/// The requested preview texture is decoded near its display size, so the widget
/// never retains the camera-resolution source just to draw a chat thumbnail.
fn build_image_widget(
    container: &Box,
    path: &str,
    css_class: &str,
    on_click: &Rc<RefCell<Option<ClickHandler>>>,
) {
    let is_sticker = css_class == "media-preview-sticker";
    // Media fills the full bubble width (Clamp max 380px minus 20px padding)
    let max_w: i32 = if is_sticker { 180 } else { 380 };
    let max_h: i32 = if is_sticker { 180 } else { 520 };

    // Probe the texture from file to compute display size. We keep the loaded
    // texture (not just its dimensions) so we can show it immediately below.
    let probe = crate::ui::texture_cache::texture_thumbnail(path, max_h.max(max_w));
    let (display_w, display_h) = probe
        .as_ref()
        .map(|t| {
            let iw = t.width();
            let ih = t.height();
            let scale = (max_w as f64 / iw as f64)
                .min(max_h as f64 / ih as f64)
                .min(1.0);
            (
                (iw as f64 * scale).round() as i32,
                (ih as f64 * scale).round() as i32,
            )
        })
        .unwrap_or((max_w, max_h));

    let pic = gtk4::Picture::new();
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk4::ContentFit::Contain);
    pic.set_size_request(display_w, display_h);
    pic.set_hexpand(false);
    pic.set_vexpand(false);

    // Show the texture eagerly. We already loaded it for the size probe, so this
    // is free. Previously the paintable was ONLY set from `connect_map` (lazy),
    // which on a bulk history-load — especially through the hide/show paint
    // cycle — did not reliably fire, leaving valid, on-disk images blank until
    // some unrelated event. Setting it here guarantees first paint; the map/
    // unmap handlers below still manage GPU residency on scroll.
    if let Some(tex) = &probe {
        pic.set_paintable(Some(tex));
    }

    // Keep the texture resident once set — we deliberately do NOT clear it on
    // unmap. The chat-view runs a hide/show paint cycle that unmaps every bubble,
    // and GTK's `map` signal does not reliably fire on the remap, so clearing on
    // unmap left valid, on-disk images permanently blank (the bug). The `map`
    // handler below is now just a fallback that reloads if the paintable is
    // somehow missing. Textures are released when the bubble is dropped on chat
    // switch, bounding GPU memory to roughly the open chat's worth of images.
    let path_owned = path.to_string();
    let pic_weak = pic.downgrade();
    pic.connect_map(move |_| {
        let Some(p) = pic_weak.upgrade() else { return };
        if p.paintable().is_some() {
            return;
        }
        if let Some(tex) =
            crate::ui::texture_cache::texture_thumbnail(&path_owned, max_h.max(max_w))
        {
            p.set_paintable(Some(&tex));
        }
    });

    // Image frame: set exact dimensions, don't expand
    let frame = Box::new(Orientation::Vertical, 0);
    frame.set_size_request(display_w, display_h);
    frame.set_overflow(gtk4::Overflow::Hidden);
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    frame.set_cursor_from_name(Some("pointer"));
    frame.append(&pic);

    // Click → carousel
    let gesture = GestureClick::new();
    gesture.set_button(1);
    let cb = on_click.clone();
    let activate_image: Rc<dyn Fn()> = Rc::new(move || {
        let borrow = cb.borrow();
        if let Some(f) = borrow.as_ref() {
            f();
        }
    });
    let activate_click = activate_image.clone();
    gesture.connect_released(move |_, _, _, _| activate_click());
    frame.add_controller(gesture);
    install_keyboard_activation(frame.upcast_ref(), "Open image viewer", activate_image);

    container.append(&frame);
}

/// Render a video: thumbnail placeholder + play-in-player button.
fn build_video_widget(container: &Box, path: &str) {
    // The gtk4::Video stays empty until the user explicitly presses play.
    // Merely setting its file creates a GStreamer pipeline and several driver
    // threads; message-list children remain mapped even when scrolled away, so
    // map-based loading retained one pipeline per video in the open history.
    let video = gtk4::Video::new();
    video.set_size_request(380, 250);
    video.set_autoplay(false);
    video.set_hexpand(false);
    video.set_vexpand(false);
    video.set_can_focus(false);
    video.set_sensitive(false);

    // Wrap in a clickable Box
    let wrapper = Box::new(Orientation::Vertical, 0);
    wrapper.set_overflow(gtk4::Overflow::Hidden);

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&video));

    let play_btn = Label::new(Some("▶"));
    play_btn.add_css_class("title-1");
    play_btn.set_halign(Align::Center);
    play_btn.set_valign(Align::Center);
    play_btn.set_opacity(0.8);
    overlay.add_overlay(&play_btn);

    // Pop-out button (top-right corner)
    let popout_btn = Button::from_icon_name("view-fullscreen-symbolic");
    popout_btn.add_css_class("osd");
    popout_btn.add_css_class("circular");
    popout_btn.set_halign(Align::End);
    popout_btn.set_valign(Align::Start);
    popout_btn.set_margin_top(4);
    popout_btn.set_margin_end(4);
    popout_btn.set_tooltip_text(Some("Open fullscreen"));
    let path_popout = path.to_string();
    let video_weak_popout = video.downgrade();
    let play_weak_popout = play_btn.downgrade();
    popout_btn.connect_clicked(move |_| {
        if let Some(video) = video_weak_popout.upgrade()
            && let Some(stream) = video.media_stream()
        {
            stream.pause();
        }
        if let Some(play) = play_weak_popout.upgrade() {
            play.set_visible(true);
        }
        open_video_window(&path_popout);
    });
    overlay.add_overlay(&popout_btn);

    wrapper.append(&overlay);

    // Fully tear down the pipeline when the bubble leaves the widget tree.
    let video_weak_unmap = video.downgrade();
    let play_weak_unmap = play_btn.downgrade();
    wrapper.connect_unmap(move |_| {
        if let Some(v) = video_weak_unmap.upgrade() {
            if let Some(stream) = v.media_stream() {
                stream.pause();
                if let Ok(mf) = stream.downcast::<gtk4::MediaFile>() {
                    mf.clear();
                }
            }
            v.set_media_stream(None::<&gtk4::MediaStream>);
            v.set_file(None::<&gtk4::gio::File>);
        }
        if let Some(p) = play_weak_unmap.upgrade() {
            p.set_visible(true);
        }
    });

    // Click → play inline (toggle play/pause)
    let path_owned = path.to_string();
    let play_weak = play_btn.downgrade();
    let video_weak = video.downgrade();
    let activate_video: Rc<dyn Fn()> = Rc::new(move || {
        let Some(video_ref) = video_weak.upgrade() else {
            return;
        };
        let Some(play_ref) = play_weak.upgrade() else {
            return;
        };
        video_ref.set_sensitive(true);
        if let Some(stream) = video_ref.media_stream() {
            if stream.is_playing() {
                stream.pause();
                play_ref.set_visible(true);
            } else {
                stream.play();
                play_ref.set_visible(false);
            }
        } else {
            let mf = gtk4::MediaFile::for_filename(&path_owned);
            mf.set_muted(false);
            mf.play();
            video_ref.set_media_stream(Some(&mf));
            play_ref.set_visible(false);
        }
    });
    let gesture = GestureClick::new();
    gesture.set_button(1);
    let activate_click = activate_video.clone();
    gesture.connect_released(move |_, _, _, _| activate_click());
    wrapper.add_controller(gesture);
    wrapper.set_cursor_from_name(Some("pointer"));
    install_keyboard_activation(wrapper.upcast_ref(), "Play or pause video", activate_video);

    container.append(&wrapper);
}

/// State for a lightweight GIF animation (frame textures + timer).
struct GifAnimation {
    frames: Vec<gtk4::gdk::Texture>,
    index: usize,
    timer_id: Option<glib::SourceId>,
}

const MAX_ACTIVE_GIF_ANIMATIONS: usize = 4;

thread_local! {
    /// Message rows are not virtualized, so every GIF in the 50-message
    /// history may be mapped at once. Retain full frame sets for only the four
    /// newest animations; older GIFs remain visible on their current frame.
    static ACTIVE_GIF_ANIMATIONS: RefCell<Vec<std::rc::Weak<RefCell<Option<GifAnimation>>>>> =
        const { RefCell::new(Vec::new()) };
}

fn register_gif_animation(anim: &Rc<RefCell<Option<GifAnimation>>>) {
    ACTIVE_GIF_ANIMATIONS.with(|active| {
        let mut active = active.borrow_mut();
        active.retain(|weak| weak.upgrade().is_some_and(|state| state.borrow().is_some()));
        active.push(Rc::downgrade(anim));

        while active.len() > MAX_ACTIVE_GIF_ANIMATIONS {
            let Some(old) = active.remove(0).upgrade() else {
                continue;
            };
            if let Some(mut animation) = old.borrow_mut().take() {
                if let Some(id) = animation.timer_id.take() {
                    id.remove();
                }
                animation.frames.clear();
            }
        }
    });
}

/// Extract frames from an MP4 using ffmpeg into a cache dir.
/// Returns the cache directory path. Frames are named frame_001.png, frame_002.png, etc.
fn extract_gif_frames(mp4_path: &str) -> Option<PathBuf> {
    // Use a stable cache dir based on the filename so we only extract once
    let hash = mp4_path.len() as u64
        ^ mp4_path
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let cache_dir = std::env::temp_dir().join(format!("wa_gif_v2_{hash:016x}"));
    // If we already extracted, skip ffmpeg
    if cache_dir.join("frame_001.png").exists() {
        return Some(cache_dir);
    }
    std::fs::create_dir_all(&cache_dir).ok()?;
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-i",
            mp4_path,
            "-vf",
            "fps=8,scale=180:-1",
            "-frames:v",
            "48",
            "-y",
        ])
        .arg(cache_dir.join("frame_%03d.png").to_str()?)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if status.success() && cache_dir.join("frame_001.png").exists() {
        Some(cache_dir)
    } else {
        let _ = std::fs::remove_dir_all(&cache_dir);
        None
    }
}

/// Render a GIF (short MP4 with gif_playback flag).
/// Autoplay using extracted PNG frames cycled on a timer — no GStreamer pipeline.
/// At most 48 180px frames (~6MB worst-case) are held per visible GIF instead
/// of a full GStreamer pipeline or an unbounded full-resolution frame set.
fn build_gif_widget(container: &Box, path: &str) {
    let pic = gtk4::Picture::new();
    pic.set_size_request(380, 340);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk4::ContentFit::Cover);
    pic.set_hexpand(false);
    pic.set_vexpand(false);
    pic.set_margin_start(2);
    pic.set_margin_end(2);
    pic.set_margin_top(2);
    pic.set_margin_bottom(2);

    let anim: Rc<RefCell<Option<GifAnimation>>> = Rc::new(RefCell::new(None));

    // On map: extract frames (cached) and start cycling.
    // All widget refs are weak to prevent reference cycles / memory leaks.
    let path_owned = path.to_string();
    let pic_weak_map = pic.downgrade();
    let anim_map = anim.clone();
    pic.connect_map(move |_| {
        if anim_map.borrow().is_some() {
            return;
        }
        let Some(pic_strong) = pic_weak_map.upgrade() else {
            return;
        };
        let path_c = path_owned.clone();
        let pic_weak_inner = pic_strong.downgrade();
        let anim_c = anim_map.clone();
        // Extract frames in a background thread so we don't block the UI
        glib::spawn_future_local(async move {
            let cache_dir = {
                let p = path_c.clone();
                let (tx, rx) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let _ = tx.send_blocking(extract_gif_frames(&p));
                });
                match rx.recv().await {
                    Ok(Some(dir)) => dir,
                    _ => return,
                }
            };
            // Load frame textures
            let mut frames = Vec::new();
            for i in 1..=48 {
                let frame_path = cache_dir.join(format!("frame_{i:03}.png"));
                if !frame_path.exists() {
                    break;
                }
                if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(&frame_path, 200) {
                    frames.push(tex);
                }
            }
            if frames.is_empty() {
                return;
            }
            // Show first frame immediately
            if let Some(p) = pic_weak_inner.upgrade() {
                p.set_paintable(Some(&frames[0]));
            }
            // Match the 8fps extraction rate so playback duration stays correct.
            // Timer closure uses weak ref to pic — returns Break if widget is gone.
            let pic_weak_timer = pic_weak_inner.clone();
            let anim_timer = anim_c.clone();
            let timer_id =
                glib::timeout_add_local(std::time::Duration::from_millis(125), move || {
                    let Some(p) = pic_weak_timer.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let mut borrow = anim_timer.borrow_mut();
                    if let Some(ref mut a) = *borrow {
                        a.index = (a.index + 1) % a.frames.len();
                        p.set_paintable(Some(&a.frames[a.index]));
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
                });
            *anim_c.borrow_mut() = Some(GifAnimation {
                frames,
                index: 0,
                timer_id: Some(timer_id),
            });
            register_gif_animation(&anim_c);
        });
    });

    // On unmap: stop timer and drop all textures
    let pic_weak_unmap = pic.downgrade();
    let anim_unmap = anim.clone();
    pic.connect_unmap(move |_| {
        if let Some(mut a) = anim_unmap.borrow_mut().take() {
            if let Some(id) = a.timer_id.take() {
                id.remove();
            }
            a.frames.clear();
        }
        if let Some(p) = pic_weak_unmap.upgrade() {
            p.set_paintable(None::<&gtk4::gdk::Paintable>);
        }
    });

    container.append(&pic);
}

/// Render an audio message: waveform placeholder + play button.
fn build_audio_widget(container: &Box, path: &str) {
    let hbox = Box::new(Orientation::Horizontal, 8);

    let path_owned = path.to_string();
    // Behaviour is "open in external player" (open_with_xdg), so use an
    // open-external glyph rather than a play-triangle that implies inline
    // playback the widget does not do (mb-10).
    let play_btn = Button::from_icon_name("document-open-symbolic");
    play_btn.add_css_class("flat");
    play_btn.set_tooltip_text(Some("Open audio player"));
    play_btn.connect_clicked(move |_| open_with_xdg(&path_owned));

    let wave = Label::new(Some("▬▬▬▬▬▬▬▬▬▬"));
    wave.add_css_class("dim-label");
    wave.add_css_class("caption");

    hbox.append(&play_btn);
    hbox.append(&wave);
    container.append(&hbox);
}

/// Render a document: PDF thumbnail (if available) + filename, click to open.
fn build_document_widget(container: &Box, path: &str) {
    let fname = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Document");
    // Strip the msg_id prefix (first 8 chars + '_') added during download
    let display = fname.splitn(2, '_').nth(1).unwrap_or(fname);
    let decoded = urldecode(display);
    let is_pdf = path.to_lowercase().ends_with(".pdf");

    // One focusable control wraps thumbnail + filename. This avoids exposing
    // duplicate tab stops for two pieces that perform the same action.
    let document = Box::new(Orientation::Vertical, 4);
    document.set_cursor_from_name(Some("pointer"));

    // File info row: icon + filename. Its width is matched once an async PDF
    // thumbnail is ready.
    let hbox = Box::new(Orientation::Horizontal, 8);
    hbox.set_hexpand(false);

    let icon = Label::new(Some(file_type_icon(path)));
    let name_label = Label::new(Some(&decoded));
    name_label.add_css_class("body");
    name_label.set_wrap(true);
    name_label.set_wrap_mode(gtk4::pango::WrapMode::Char);
    name_label.set_max_width_chars(50);
    name_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    name_label.set_lines(2);
    name_label.set_hexpand(false);

    hbox.append(&icon);
    hbox.append(&name_label);

    if is_pdf {
        let pic = gtk4::Picture::new();
        pic.set_can_shrink(true);
        pic.set_content_fit(gtk4::ContentFit::Contain);

        let frame = Box::new(Orientation::Vertical, 0);
        frame.set_overflow(gtk4::Overflow::Hidden);
        frame.append(&pic);
        document.append(&frame);

        let pic_w = pic.downgrade();
        let frame_w = frame.downgrade();
        let hbox_w = hbox.downgrade();
        render_pdf_thumbnail_async(path, move |thumb_path| {
            let (Some(pic), Some(frame), Some(hbox), Some(thumb_path)) = (
                pic_w.upgrade(),
                frame_w.upgrade(),
                hbox_w.upgrade(),
                thumb_path,
            ) else {
                return;
            };
            let Some(tex) = crate::ui::texture_cache::texture_thumbnail(&thumb_path, 480) else {
                return;
            };
            let scale = (380.0 / tex.width() as f64)
                .min(480.0 / tex.height() as f64)
                .min(1.0);
            let w = (tex.width() as f64 * scale).round() as i32;
            let h = (tex.height() as f64 * scale).round() as i32;
            pic.set_size_request(w, h);
            frame.set_size_request(w, h);
            hbox.set_size_request(w, -1);
            hbox.set_overflow(gtk4::Overflow::Hidden);
            pic.set_paintable(Some(&tex));
        });
    }

    document.append(&hbox);

    let path_owned = path.to_string();
    let activate_document: Rc<dyn Fn()> = Rc::new(move || open_with_xdg(&path_owned));
    let gesture = GestureClick::new();
    gesture.set_button(1);
    let activate_click = activate_document.clone();
    gesture.connect_released(move |_, _, _, _| activate_click());
    document.add_controller(gesture);
    install_keyboard_activation(document.upcast_ref(), "Open document", activate_document);

    container.append(&document);
}

/// Render the first PDF page without blocking GTK. The callback always runs on
/// the GTK main context and receives `None` if pdftocairo is unavailable or the
/// render fails. Existing cached thumbnails take the same asynchronous path so
/// callers never have re-entrancy surprises.
pub(crate) fn render_pdf_thumbnail_async(
    pdf_path: &str,
    on_complete: impl FnOnce(Option<PathBuf>) + 'static,
) {
    let pdf_path = pdf_path.to_string();
    let thumb_path = PathBuf::from(format!("{pdf_path}.thumb.png"));
    let (tx, rx) = async_channel::bounded::<Option<PathBuf>>(1);

    glib::MainContext::default().spawn_local(async move {
        let result = rx.recv().await.unwrap_or(None);
        on_complete(result);
    });

    struct PdfJob {
        pdf_path: String,
        thumb_path: PathBuf,
        reply: async_channel::Sender<Option<PathBuf>>,
    }
    static PDF_WORKER: std::sync::OnceLock<std::sync::mpsc::Sender<PdfJob>> =
        std::sync::OnceLock::new();
    let worker = PDF_WORKER.get_or_init(|| {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<PdfJob>();
        if let Err(error) = std::thread::Builder::new()
            .name("pdf-thumbnail-worker".to_string())
            .spawn(move || {
                for job in job_rx {
                    let result = if job.thumb_path.exists() {
                        Some(job.thumb_path)
                    } else {
                        let output_prefix = format!("{}.thumb", job.pdf_path);
                        let rendered = std::process::Command::new("pdftocairo")
                            .args([
                                "-png",
                                "-f",
                                "1",
                                "-l",
                                "1",
                                "-scale-to",
                                "380",
                                "-singlefile",
                                &job.pdf_path,
                                &output_prefix,
                            ])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map(|status| status.success())
                            .unwrap_or(false);
                        if rendered && job.thumb_path.exists() {
                            Some(job.thumb_path)
                        } else {
                            None
                        }
                    };
                    let _ = job.reply.send_blocking(result);
                }
            })
        {
            log::warn!("Failed to start PDF thumbnail worker: {error}");
        }
        job_tx
    });

    if worker
        .send(PdfJob {
            pdf_path,
            thumb_path,
            reply: tx.clone(),
        })
        .is_err()
    {
        // Wake the awaiting main-context task even if the worker exited.
        let _ = tx.try_send(None);
    }
}

/// Placeholder shown while media is still downloading.
/// Build the grouped reaction pill row for a set of reactions, or None if empty.
/// Shared by the initial bubble render and live `rebuild_reactions` so both
/// group-by-emoji + count + reactor popover identically. Tagged with the widget
/// name "reaction-row" so the live path can find and replace it.
fn build_reaction_row(reactions: &[(String, String)], is_from_me: bool) -> Option<Box> {
    if reactions.is_empty() {
        return None;
    }
    let mut grouped: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (sender, emoji) in reactions {
        grouped
            .entry(emoji.clone())
            .or_default()
            .push(sender.clone());
    }

    let reaction_row = Box::new(Orientation::Horizontal, 4);
    reaction_row.set_widget_name("reaction-row");
    reaction_row.set_margin_top(-6); // slight overlap with bubble
    reaction_row.set_margin_start(if is_from_me { 60 } else { 44 });
    reaction_row.set_halign(if is_from_me { Align::End } else { Align::Start });

    for (emoji, senders) in &grouped {
        let count = senders.len();
        let pill_text = if count > 1 {
            format!("{emoji} {count}")
        } else {
            emoji.clone()
        };
        let pill_btn = Button::with_label(&pill_text);
        pill_btn.add_css_class("flat");
        pill_btn.add_css_class("caption");
        pill_btn.set_cursor_from_name(Some("pointer"));

        let names: Vec<String> = senders
            .iter()
            .map(|s| {
                if s.is_empty() {
                    "You".to_string()
                } else {
                    crate::ui::runtime::display_name_for_jid_global(s)
                }
            })
            .collect();
        pill_btn.set_tooltip_text(Some(&names.join(", ")));

        let emoji_c = emoji.clone();
        pill_btn.connect_clicked(move |btn| {
            let popover = gtk4::Popover::new();
            popover.set_parent(btn);
            popover.set_has_arrow(true);
            let vbox = Box::new(Orientation::Vertical, 4);
            vbox.set_margin_start(8);
            vbox.set_margin_end(8);
            vbox.set_margin_top(6);
            vbox.set_margin_bottom(6);
            for name in &names {
                let row = Box::new(Orientation::Horizontal, 8);
                row.append(&Label::new(Some(&emoji_c)));
                let name_lbl = Label::new(Some(name));
                name_lbl.add_css_class("body");
                row.append(&name_lbl);
                vbox.append(&row);
            }
            popover.set_child(Some(&vbox));
            popover.popup();
        });
        reaction_row.append(&pill_btn);
    }
    Some(reaction_row)
}

fn media_placeholder_label(msg: &IncomingMessage) -> Label {
    // For old messages without downloaded media, show "unavailable" not "downloading"
    let text = match &msg.media_type {
        Some(MediaType::Image) => "📷 Photo".to_string(),
        Some(MediaType::Video) => "🎥 Video".to_string(),
        Some(MediaType::Audio) => "🎵 Audio".to_string(),
        Some(MediaType::Gif) => "🎞 GIF".to_string(),
        Some(MediaType::Sticker) => "🎭 Sticker".to_string(),
        Some(MediaType::Document) => {
            // URL-decode the filename for display (e.g., %20 → space)
            let raw = msg.media_filename.as_deref().unwrap_or("Document");
            let decoded = urldecode(raw);
            let icon = file_type_icon(raw);
            format!("{icon} {decoded}")
        }
        None => "📎 Message".to_string(),
    };
    let lbl = Label::new(Some(&text));
    lbl.add_css_class("dim-label");
    lbl.set_halign(Align::Start);
    lbl.set_wrap(true);
    lbl.set_wrap_mode(gtk4::pango::WrapMode::Char);
    lbl.set_max_width_chars(50);
    lbl.set_lines(2);
    lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    lbl
}

/// A clickable placeholder for media we have keys for but haven't downloaded
/// yet. Clicking sends `RequestMediaDownload`; the runtime fetches + decrypts
/// the file and emits `MediaReady`, which swaps in the real content.
fn build_downloadable_placeholder(
    msg: &IncomingMessage,
    bridge: &Arc<crate::bridge::Bridge>,
) -> Box {
    let row = Box::new(Orientation::Horizontal, 6);
    row.set_halign(Align::Start);

    let label = media_placeholder_label(msg);
    row.append(&label);

    let hint = Label::new(Some("⬇ Tap to download"));
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    row.append(&hint);

    row.set_cursor_from_name(Some("pointer"));

    let clicked = Rc::new(std::cell::Cell::new(false));
    let bridge_c = bridge.clone();
    let chat_id = msg.chat_id.clone();
    let msg_id = msg.id.clone();
    let hint_c = hint.clone();
    let activate_download: Rc<dyn Fn()> = Rc::new(move || {
        if clicked.replace(true) {
            return; // already requested — ignore repeat taps
        }
        bridge_c.send_command(crate::bridge::WaCommand::RequestMediaDownload {
            chat_id: chat_id.clone(),
            msg_id: msg_id.clone(),
        });
        hint_c.set_text("⏳ Downloading…");
    });
    let gesture = GestureClick::new();
    let activate_click = activate_download.clone();
    gesture.connect_pressed(move |_, _, _, _| activate_click());
    row.add_controller(gesture);
    install_keyboard_activation(row.upcast_ref(), "Download attachment", activate_download);
    row
}

/// Simple percent-decode (%XX → char) for display purposes.
/// Return an emoji icon based on the file extension.
pub fn file_type_icon(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        // Archives
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "tgz" => "🗜️",
        // PDF
        "pdf" => "📕",
        // Spreadsheets
        "xlsx" | "xls" | "csv" | "tsv" | "ods" => "📊",
        // Presentations
        "pptx" | "ppt" | "odp" | "key" => "📽️",
        // Word / text documents
        "docx" | "doc" | "odt" | "rtf" | "txt" | "md" => "📝",
        // Images (when sent as document)
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "heic" => "🖼️",
        // Audio
        "mp3" | "wav" | "ogg" | "flac" | "aac" | "m4a" | "opus" => "🎵",
        // Video
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "wmv" => "🎬",
        // Code
        "py" | "js" | "ts" | "rs" | "go" | "java" | "c" | "cpp" | "h" | "html" | "css" | "json"
        | "xml" | "yaml" | "yml" | "toml" | "sh" | "sql" => "💻",
        // Executables / installers
        "exe" | "msi" | "dmg" | "deb" | "rpm" | "appimage" | "apk" => "⚙️",
        // Fonts
        "ttf" | "otf" | "woff" | "woff2" => "🔤",
        // Default
        _ => "📄",
    }
}

fn urldecode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            let hex = [hi, lo];
            if let Ok(val) = u8::from_str_radix(std::str::from_utf8(&hex).unwrap_or("00"), 16) {
                out.push(val as char);
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Highlight @mentions in text. Handles multi-word names like "@Jake Steinman".
/// Only highlights when @ is at the start of text or preceded by whitespace
/// (not part of an email address like user@domain.com).
fn highlight_mentions(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 64);
    let mut i = 0;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    while i < len {
        if chars[i] == '@' && i + 1 < len && chars[i + 1].is_alphabetic() {
            // Only treat as mention if @ is at start or preceded by whitespace
            let preceded_by_space = i == 0 || chars[i - 1].is_whitespace();
            if !preceded_by_space {
                // Part of an email or other token — don't highlight
                result.push(chars[i]);
                i += 1;
                continue;
            }
            // Check that the word after @ starts with uppercase (real mentions do)
            if !chars[i + 1].is_uppercase() {
                result.push(chars[i]);
                i += 1;
                continue;
            }
            // Found a real mention — collect @ + capitalized words
            let start = i;
            i += 1; // skip @
            // First word after @
            while i < len && !chars[i].is_whitespace() {
                i += 1;
            }
            // Greedily consume additional capitalized words
            loop {
                if i < len && chars[i] == ' ' {
                    let next_word_start = i + 1;
                    if next_word_start < len && chars[next_word_start].is_uppercase() {
                        i = next_word_start;
                        while i < len && !chars[i].is_whitespace() {
                            i += 1;
                        }
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            let mention: String = chars[start..i].iter().collect();
            result.push_str(&format!(
                "<span foreground='#00a884'><b>{mention}</b></span>"
            ));
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// Set Pango markup on a label, falling back to plain text if the markup is
/// malformed. Our WhatsApp→Pango conversion can, in rare cases (e.g. a `_`/`*`/`~`
/// delimiter opened before a URL whose href then swallows the emitted close tag),
/// produce invalid markup. GtkLabel::set_markup on invalid input logs a g_critical
/// and leaves the label BLANK, silently losing the whole message. Pre-validate
/// with pango::parse_markup and, on failure, render the raw text so the message
/// is never lost.
fn set_markup_safe(label: &Label, markup: &str, plain: &str) {
    // accel marker '\u{0}' disables accelerator parsing (we never use accels).
    //
    // GtkLabel::set_markup supports the `<a href>` LINK extension, but the raw
    // `pango::parse_markup` validator does NOT recognise `<a>` and rejects it —
    // so validating the full markup marked EVERY message containing a URL as
    // invalid and fell back to plain text, killing link rendering + clicks.
    // Validate with the link tags stripped: that still catches real markup
    // corruption (bad escaping / broken *_~ spans) while letting a valid link
    // through, and GtkLabel renders the `<a>` fine.
    let valid = gtk4::pango::parse_markup(markup, '\u{0}').is_ok()
        || gtk4::pango::parse_markup(&strip_link_tags(markup), '\u{0}').is_ok();
    if valid {
        label.set_markup(markup);
    } else {
        log::warn!("invalid Pango markup, falling back to plain text");
        label.set_text(plain);
    }
}

/// Remove `<a ...>` / `</a>` tags (keeping their inner text) so the markup can
/// be validated by the raw Pango parser, which doesn't support GtkLabel's link
/// extension. User text is already Pango-escaped, so the only `<a` / `</a>` in
/// the string are our own generated link tags.
fn strip_link_tags(markup: &str) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(pos) = rest.find("<a") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        if (after.starts_with("<a ") || after.starts_with("<a>"))
            && let Some(gt) = after.find('>')
        {
            rest = &after[gt + 1..];
        } else {
            out.push_str("<a");
            rest = &after[2..];
        }
    }
    out.push_str(rest);
    out.replace("</a>", "")
}

/// Convert WhatsApp-style formatting to Pango markup.
/// Handles: *bold*, _italic_, ~strikethrough~, @mentions (green+bold),
/// URLs (clickable links), and preserves newlines.
fn format_whatsapp_markup(text: &str) -> String {
    // First escape for Pango (handles &, <, > etc.)
    let escaped = glib::markup_escape_text(text).to_string();

    // Process line by line to preserve newlines
    let lines: Vec<String> = escaped
        .lines()
        .map(|line| {
            // First pass: linkify URLs
            let with_links: String = line
                .split_whitespace()
                .map(|word| {
                    if word.starts_with("http://") || word.starts_with("https://") {
                        format!("<a href=\"{word}\">{word}</a>")
                    } else {
                        word.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            // Second pass: highlight @mentions (may span multiple words)
            // Find @Name patterns: @ followed by word(s) starting with uppercase
            highlight_mentions(&with_links)
        })
        .collect();
    let mut result = lines.join("\n");

    // WhatsApp inline formatting: *bold*, _italic_, ~strikethrough~
    // (highlight_mentions is applied per-line above)
    // Only match when delimiters are not escaped and contain non-empty content
    result = apply_inline_format(&result, '*', "b");
    result = apply_inline_format(&result, '_', "i");
    result = apply_inline_format(&result, '~', "s");

    result
}

/// Apply WhatsApp inline formatting for a single delimiter character.
/// Converts e.g., `*text*` → `<b>text</b>`.
fn apply_inline_format(text: &str, delim: char, tag: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut inside = false;
    let mut last_was_open = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Skip over already-emitted `<a ...>...</a>` link spans verbatim. Without
        // this, a delimiter opened earlier in the message whose matching char
        // appears inside the href/link text would inject a close tag inside the
        // <a> attribute, producing invalid Pango markup that blanks the bubble.
        if chars[i] == '<' && chars[i + 1..].starts_with(&['a', ' ']) {
            // Find the end of the closing </a>
            let close: Vec<char> = "</a>".chars().collect();
            let mut j = i;
            let mut end = None;
            while j + close.len() <= chars.len() {
                if chars[j..j + close.len()] == close[..] {
                    end = Some(j + close.len());
                    break;
                }
                j += 1;
            }
            if let Some(e) = end {
                for &c in &chars[i..e] {
                    result.push(c);
                }
                i = e;
                last_was_open = false;
                continue;
            }
        }
        if chars[i] == delim {
            // Don't match inside URLs or Pango tags
            if inside {
                // Close tag — only if we opened one and there's content
                result.push_str(&format!("</{tag}>"));
                inside = false;
                last_was_open = false;
            } else {
                // Open tag — only at word boundary (start of string, after space/newline)
                let prev = if i > 0 { chars[i - 1] } else { ' ' };
                if prev == ' ' || prev == '\n' || i == 0 {
                    result.push_str(&format!("<{tag}>"));
                    inside = true;
                    last_was_open = true;
                } else {
                    result.push(delim);
                }
            }
        } else {
            if chars[i] == '\n' && inside {
                // Newline breaks formatting — close unclosed tag
                result.push_str(&format!("</{tag}>"));
                inside = false;
            }
            if last_was_open && chars[i] == ' ' {
                // Space immediately after delimiter — not formatting, revert
                result.push_str(&format!("</{tag}>"));
                result.push(delim);
                result.push(' ');
                inside = false;
                last_was_open = false;
                i += 1;
                continue;
            }
            last_was_open = false;
            result.push(chars[i]);
        }
        i += 1;
    }
    // Close any unclosed tag (malformed formatting)
    if inside {
        result.push_str(&format!("</{tag}>"));
    }
    result
}

fn open_video_window(path: &str) {
    let window = gtk4::Window::builder()
        .title("Video")
        .default_width(800)
        .default_height(600)
        .decorated(false)
        .build();
    window.add_css_class("lightbox");

    let empty_header = gtk4::Box::new(Orientation::Horizontal, 0);
    empty_header.set_visible(false);
    window.set_titlebar(Some(&empty_header));

    let video = gtk4::Video::for_filename(Some(path));
    video.set_autoplay(true);
    video.set_hexpand(true);
    video.set_vexpand(true);

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&video));

    // Close button
    let close_btn = Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("osd");
    close_btn.add_css_class("circular");
    close_btn.set_halign(Align::End);
    close_btn.set_valign(Align::Start);
    close_btn.set_margin_top(12);
    close_btn.set_margin_end(12);
    let win_c = window.downgrade();
    close_btn.connect_clicked(move |_| {
        if let Some(window) = win_c.upgrade() {
            window.close();
        }
    });
    overlay.add_overlay(&close_btn);

    // Escape to close
    let key_ctrl = gtk4::EventControllerKey::new();
    let win_c = window.downgrade();
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            if let Some(window) = win_c.upgrade() {
                window.close();
            }
            gtk4::glib::Propagation::Stop
        } else {
            gtk4::glib::Propagation::Proceed
        }
    });
    window.add_controller(key_ctrl);

    let video_cleanup = video.clone();
    window.connect_close_request(move |_| {
        if let Some(stream) = video_cleanup.media_stream() {
            stream.pause();
            if let Ok(file) = stream.downcast::<gtk4::MediaFile>() {
                file.clear();
            }
        }
        video_cleanup.set_media_stream(None::<&gtk4::MediaStream>);
        video_cleanup.set_file(None::<&gtk4::gio::File>);
        gtk4::glib::Propagation::Proceed
    });

    window.set_child(Some(&overlay));
    window.fullscreen();
    window.present();
}

/// Open an http(s) URL in the browser, hardened like `open_with_xdg`
/// (setsid + detached + null stdio) so it survives the app and never blocks.
///
/// The URL is sender-controlled, so only `http://` and `https://` are allowed
/// through — any other scheme (`file://`, `javascript:`, a bare local path,
/// etc.) is rejected without ever reaching xdg-open.
fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let lower = url.trim_start().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        eprintln!("open_url: refusing non-http(s) URL: {url}");
        return;
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        // Prefer setsid so the browser survives the app; fall back to a bare
        // xdg-open if setsid isn't available. Log failures instead of silently
        // discarding them.
        let spawn_setsid = Command::new("setsid")
            .arg("xdg-open")
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(e) = spawn_setsid {
            eprintln!("open_url: setsid spawn failed ({e}); falling back to xdg-open");
            if let Err(e2) = Command::new("xdg-open")
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                eprintln!("open_url: xdg-open spawn failed: {e2}");
            }
        }
    });
}

fn open_with_xdg(path: &str) {
    use std::process::{Command, Stdio};
    let src = path.to_string();
    std::thread::spawn(move || {
        // Copy into ~/Downloads and open THAT, for two reasons:
        //  • wa_media lives under ~/.local/share, which sandboxed (Flatpak)
        //    viewers/browsers — e.g. a Flatpak Vivaldi/Firefox — cannot read,
        //    so xdg-open'ing the internal path silently opens a blank window.
        //    ~/Downloads is reachable to them via the xdg-download portal.
        //  • the user expects "download" to leave a real file where they can
        //    find it.
        // Falls back to the original path if the copy fails (no regression).
        let target = save_to_downloads(std::path::Path::new(&src))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| src.clone());
        let _ = Command::new("setsid")
            .arg("xdg-open")
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    });
}

/// Copy a downloaded media file into ~/Downloads under a clean, human-readable
/// name (our `<8hex>_` download prefix stripped, percent-decoded). Returns the
/// destination path. Idempotent: a same-name, same-size file is reused; a name
/// collision with *different* content gets a " (n)" suffix.
pub(crate) fn save_to_downloads(src: &std::path::Path) -> Option<std::path::PathBuf> {
    let fname = src.file_name()?.to_str()?;
    let base = fname
        .splitn(2, '_')
        .nth(1)
        .filter(|s| !s.is_empty())
        .unwrap_or(fname);
    let clean = urldecode(base);
    let dir = std::path::PathBuf::from(std::env::var("HOME").ok()?).join("Downloads");
    std::fs::create_dir_all(&dir).ok()?;
    let src_len = std::fs::metadata(src).ok()?.len();

    let preferred = dir.join(&clean);
    if preferred.exists() && std::fs::metadata(&preferred).ok().map(|m| m.len()) == Some(src_len) {
        return Some(preferred); // already downloaded, identical — reuse
    }
    let dest = if preferred.exists() {
        // Name collision with different content → "name (n).ext".
        let (stem, ext) = match clean.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), format!(".{e}")),
            None => (clean.clone(), String::new()),
        };
        (1..1000)
            .map(|i| dir.join(format!("{stem} ({i}){ext}")))
            .find(|p| !p.exists())
            .unwrap_or(preferred)
    } else {
        preferred
    };
    std::fs::copy(src, &dest).ok()?;
    log::info!("Saved attachment to {}", dest.display());
    Some(dest)
}

fn format_time(ts: i64) -> String {
    use chrono::{DateTime, Local, Utc};
    let dt: DateTime<Local> =
        DateTime::from(DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default());
    dt.format("%-I:%M %p").to_string()
}

/// Generate a consistent colour for a sender name/JID (WhatsApp-style palette).
fn name_to_colour(name: &str) -> &'static str {
    const COLOURS: &[&str] = &[
        "#e15d44", "#e6855e", "#d4a03c", "#5ca95c", "#45b5a7", "#5b91c5", "#7b68c4", "#c474b7",
        "#e06080", "#6bb89c", "#c49640", "#8e6cc0", "#d47070", "#50a0d0", "#7eb050",
    ];
    let hash: u32 = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    COLOURS[(hash as usize) % COLOURS.len()]
}

/// Build the link-preview card. Shared by bubble construction and the late
/// backfill, which arrives after the sender-less link already rendered.
fn build_link_preview_card(
    url: &str,
    link_title: Option<&str>,
    link_description: Option<&str>,
    link_thumbnail_path: Option<&str>,
) -> Box {
    let url_owned_outer = url.to_string();
    let url = &url_owned_outer;

            // Extract domain for fallback title
            let domain = url
                .split("//")
                .nth(1)
                .and_then(|s| s.split('/').next())
                .unwrap_or(url);
            let title = link_title.unwrap_or(domain);
            let preview_box = Box::new(Orientation::Vertical, 2);
            preview_box.add_css_class("reply-context");
            preview_box.set_margin_top(4);

            // Thumbnail image (async download)
            if let Some(thumb_url) = link_thumbnail_path.map(str::to_string).as_ref() {
                let pic = gtk4::Picture::new();
                pic.set_size_request(-1, 140);
                pic.set_content_fit(gtk4::ContentFit::Cover);
                pic.set_can_shrink(true);
                preview_box.append(&pic);
                // Download thumbnail on background thread
                let url_c = thumb_url.clone();
                let pic_c = pic.clone();
                let (tx_img, rx_img) = async_channel::bounded::<Vec<u8>>(1);
                glib::MainContext::default().spawn_local(async move {
                    if let Ok(bytes) = rx_img.recv().await {
                        let gb = glib::Bytes::from(&bytes);
                        if let Ok(tex) = gtk4::gdk::Texture::from_bytes(&gb) {
                            pic_c.set_paintable(Some(&tex));
                        }
                    }
                });
                std::thread::spawn(move || {
                    use std::io::Read;
                    if let Ok(resp) = ureq::get(&url_c).call() {
                        let mut bytes = Vec::new();
                        if resp.into_reader().read_to_end(&mut bytes).is_ok() {
                            let _ = tx_img.send_blocking(bytes);
                        }
                    }
                });
            }

            let title_lbl = Label::new(Some(title));
            title_lbl.add_css_class("heading");
            title_lbl.set_halign(Align::Start);
            title_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            title_lbl.set_max_width_chars(50);
            title_lbl.set_max_width_chars(50);
            preview_box.append(&title_lbl);

            if let Some(desc) = link_description {
                let desc_lbl = Label::new(Some(desc));
                desc_lbl.add_css_class("dim-label");
                desc_lbl.add_css_class("caption");
                desc_lbl.set_halign(Align::Start);
                desc_lbl.set_wrap(true);
                desc_lbl.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                desc_lbl.set_max_width_chars(50);
                desc_lbl.set_max_width_chars(50);
                desc_lbl.set_lines(2);
                desc_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                preview_box.append(&desc_lbl);
            }

            let url_lbl = Label::new(Some(url));
            url_lbl.add_css_class("dim-label");
            url_lbl.add_css_class("caption");
            url_lbl.set_halign(Align::Start);
            url_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            url_lbl.set_max_width_chars(50);
            url_lbl.set_max_width_chars(50);
            preview_box.append(&url_lbl);

            // Click to open URL — route through the same hardened launcher used
            // for the text-label link path (setsid + null stdio + detached). The
            // URL is sender-controlled, so `open_url` allowlists http/https only
            // and rejects any other scheme before spawning xdg-open (mb-12).
            let url_owned = url.clone();
            let activate_link: Rc<dyn Fn()> = Rc::new(move || open_url(&url_owned));
            let gesture = GestureClick::new();
            gesture.set_button(1);
            let activate_click = activate_link.clone();
            gesture.connect_released(move |_, _, _, _| activate_click());
            preview_box.add_controller(gesture);
            preview_box.set_cursor_from_name(Some("pointer"));
            install_keyboard_activation(
                preview_box.upcast_ref(),
                "Open link preview",
                activate_link,
            );
    preview_box
}
