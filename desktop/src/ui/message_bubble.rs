use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{Align, Box, Button, GestureClick, Label, Orientation};

// gtk4::Box shadows std::boxed::Box, so alias the closure box type explicitly.
type ClickHandler = std::boxed::Box<dyn Fn()>;

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
    on_image_click: Rc<RefCell<Option<ClickHandler>>>,
    avatar: libadwaita::Avatar,
    pub sender_id: String,
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
    text_label: Option<Label>,
    /// "(edited)" indicator label
    edited_label: Option<Label>,
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
                on_image_click: Rc::new(RefCell::new(None)),
                avatar: libadwaita::Avatar::new(0, None, false),
                sender_id: String::new(),
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
                text_label: None,
                edited_label: None,
            };
        }

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
                if let Ok(tex) = gtk4::gdk::Texture::from_filename(path) {
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
            // Source 2: scan wa_media/ by quoted_msg_id prefix
            if !thumb_loaded {
                if let Some(ref qid) = msg.quoted_msg_id {
                    let media_dir = std::path::PathBuf::from("wa_media");
                    if media_dir.exists() {
                        // Try full ID first, then prefix
                        let full = media_dir.join(format!("{qid}.jpeg"));
                        if full.exists() {
                            if let Ok(tex) = gtk4::gdk::Texture::from_filename(&full) {
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
                        if !thumb_loaded {
                            let prefix = &qid[..8.min(qid.len())];
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
                                    if let Ok(tex) = gtk4::gdk::Texture::from_filename(entry.path())
                                    {
                                        let thumb = gtk4::Picture::new();
                                        thumb.set_paintable(Some(&tex));
                                        thumb.set_size_request(72, 72);
                                        thumb.set_can_shrink(true);
                                        thumb.set_content_fit(gtk4::ContentFit::Cover);
                                        thumb.set_halign(Align::End);
                                        reply_box.append(&thumb);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }

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
                    let own_name_c = own_display_name.to_string();
                    opt_btn.connect_clicked(move |_| {
                        let mut s = sel.borrow_mut();
                        if is_single_c {
                            for (i, v) in s.iter_mut().enumerate() {
                                *v = i == idx;
                            }
                        } else {
                            s[idx] = !s[idx];
                        }

                        let total_votes: usize = s.iter().filter(|v| **v).count();
                        let w = wdg.borrow();
                        for (i, pw) in w.iter().enumerate() {
                            if s[i] {
                                pw.radio.set_markup(
                                    "<span size='x-large' foreground='#25D366'>●</span>",
                                );
                                pw.count_label.set_text("1");
                            } else {
                                pw.radio.set_markup(
                                    "<span size='x-large' foreground='#8696a0'>◯</span>",
                                );
                                pw.count_label.set_text("");
                            }
                            pw.bar_track.set_visible(true);
                            if s[i] {
                                pw.bar_fill.set_size_request(250, 6);
                            } else {
                                pw.bar_fill.set_size_request(0, 6);
                            }
                            // Show own avatar on selected options, clear on deselected
                            while let Some(child) = pw.voters_box.first_child() {
                                pw.voters_box.remove(&child);
                            }
                            if s[i] {
                                let av = libadwaita::Avatar::new(20, Some(&own_name_c), true);
                                av.set_size_request(20, 20);
                                pw.voters_box.append(&av);
                            }
                        }

                        // Update total label
                        if total_votes > 0 {
                            total_lbl_c.set_text(&format!(
                                "{total_votes} vote{}",
                                if total_votes != 1 { "s" } else { "" }
                            ));
                        } else {
                            total_lbl_c.set_text("0 votes");
                        }

                        // Send vote to WhatsApp
                        if !poll_secret_c.is_empty() {
                            let selected_names: Vec<String> = s
                                .iter()
                                .enumerate()
                                .filter(|(_, v)| **v)
                                .map(|(i, _)| all_options[i].clone())
                                .collect();
                            bridge_c.send_command(crate::bridge::WaCommand::VotePoll {
                                chat_id: chat_id_c.clone(),
                                poll_msg_id: msg_id_c.clone(),
                                poll_creator: sender_id_c.clone(),
                                poll_secret: poll_secret_c.clone(),
                                selected_options: selected_names,
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

        // Media widget
        let media_box = if msg.media_type.is_some() {
            let mb = Box::new(Orientation::Vertical, 4);
            if let Some(path) = &msg.media_local_path {
                build_media_content(&mb, path, msg.media_type.as_ref().unwrap(), &on_image_click);
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
            text_label.set_markup(&markup);
            text_label.set_use_markup(true);

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
            card.append(&msg_btn);

            content.append(&card);
        }

        // Link preview rendering
        if let Some(url) = &msg.link_url {
            // Extract domain for fallback title
            let domain = url
                .split("//")
                .nth(1)
                .and_then(|s| s.split('/').next())
                .unwrap_or(url);
            let title = msg.link_title.as_deref().unwrap_or(domain);
            let preview_box = Box::new(Orientation::Vertical, 2);
            preview_box.add_css_class("reply-context");
            preview_box.set_margin_top(4);

            // Thumbnail image (async download)
            if let Some(thumb_url) = &msg.link_thumbnail_path {
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

            if let Some(desc) = &msg.link_description {
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

            // Click to open URL
            let url_owned = url.clone();
            let gesture = GestureClick::new();
            gesture.set_button(1);
            gesture.connect_released(move |_, _, _, _| {
                let _ = std::process::Command::new("xdg-open")
                    .arg(&url_owned)
                    .spawn();
            });
            preview_box.add_controller(gesture);
            preview_box.set_cursor_from_name(Some("pointer"));

            content.append(&preview_box);
        }

        // Bottom row: time + receipt
        let meta_row = Box::new(Orientation::Horizontal, 4);
        meta_row.set_halign(Align::End);

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
        if !msg.reactions.is_empty() {
            // Group reactions by emoji, collect senders for each
            let mut grouped: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for (sender, emoji) in &msg.reactions {
                grouped
                    .entry(emoji.clone())
                    .or_default()
                    .push(sender.clone());
            }

            let reaction_row = Box::new(Orientation::Horizontal, 4);
            reaction_row.set_margin_top(-6); // slight overlap with bubble
            reaction_row.set_margin_start(if msg.is_from_me { 60 } else { 44 });
            reaction_row.set_halign(if msg.is_from_me {
                Align::End
            } else {
                Align::Start
            });

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

                // Build reactor names
                let names: Vec<String> = senders
                    .iter()
                    .map(|s| {
                        if s.is_empty() {
                            "You".to_string()
                        } else {
                            crate::ui::runtime::display_name_from_jid(s)
                        }
                    })
                    .collect();
                // Tooltip for hover
                pill_btn.set_tooltip_text(Some(&names.join(", ")));
                // Click shows popover with reactor list
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
                        let emoji_lbl = Label::new(Some(&emoji_c));
                        let name_lbl = Label::new(Some(name));
                        name_lbl.add_css_class("body");
                        row.append(&emoji_lbl);
                        row.append(&name_lbl);
                        vbox.append(&row);
                    }

                    popover.set_child(Some(&vbox));
                    popover.popup();
                });

                reaction_row.append(&pill_btn);
            }
            root.append(&reaction_row);
        }

        // ── Hover show/hide for quick actions — disabled in forward mode ──
        let motion = gtk4::EventControllerMotion::new();
        let actions_ref = hover_actions.clone();
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
        });
        let actions_ref2 = hover_actions.clone();
        motion.connect_leave(move |_| {
            actions_ref2.set_visible(false);
        });
        root.add_controller(motion);

        Self {
            root,
            receipt_label,
            msg_id: msg.id.clone(),
            text: msg.text.clone().or_else(|| msg.media_caption.clone()),
            media_box,
            media_type: msg.media_type.clone(),
            media_loaded: RefCell::new(msg.media_local_path.is_some()),
            on_image_click,
            avatar,
            sender_id: if msg.is_from_me {
                String::new()
            } else {
                msg.sender_id.clone()
            },
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
            contact_msg_btn: None, // Wired by chat_view after creation
            text_label: stored_text_label,
            edited_label: stored_edited_label,
        }
    }

    /// Get the contact "Message" button if this bubble has a contact card
    pub fn contact_message_button(&self) -> Option<&Button> {
        self.contact_msg_btn.as_ref()
    }

    /// Update the text of this bubble (for message edits) and show "(edited)" badge.
    pub fn update_text(&self, new_text: &str, show_edited: bool) {
        if let Some(label) = &self.text_label {
            let markup = format_whatsapp_markup(new_text);
            label.set_markup(&markup);
        }
        if show_edited && self.edited_label.is_none() {
            // Add "(edited)" label next to the text
            if let Some(text_label) = &self.text_label {
                if let Some(parent) = text_label.parent().and_then(|p| p.downcast::<Box>().ok()) {
                    let edited = Label::new(Some("edited"));
                    edited.add_css_class("caption");
                    edited.add_css_class("dim-label");
                    edited.set_halign(gtk4::Align::End);
                    parent.append(&edited);
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

    /// Get the quick action button box children: (react, reply, forward)
    pub fn quick_action_buttons(&self) -> (Button, Button, Button) {
        let mut children = Vec::new();
        let mut child = self.hover_actions.first_child();
        while let Some(c) = child {
            if let Ok(btn) = c.clone().downcast::<Button>() {
                children.push(btn);
            }
            child = c.next_sibling();
        }
        (
            children[0].clone(),
            children[1].clone(),
            children[2].clone(),
        )
    }

    pub fn avatar_widget(&self) -> &libadwaita::Avatar {
        &self.avatar
    }

    pub fn set_avatar_image(&self, path: &str) {
        if let Ok(texture) = gtk4::gdk::Texture::from_filename(path) {
            self.avatar.set_custom_image(Some(&texture));
        }
    }

    /// Set the callback invoked when the user clicks an image/gif/sticker bubble.
    /// Called by chat_view after creation so it can pass a closure over the media list.
    pub fn set_image_click_handler(&self, f: impl Fn() + 'static) {
        *self.on_image_click.borrow_mut() = Some(std::boxed::Box::new(f));
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

    /// Replace the media placeholder with the actual downloaded file.
    pub fn set_media_loaded(&self, path: &str) {
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
/// The `css_class` is applied to the Picture to cap its size via CSS max-width/max-height.
/// This is the correct GTK4 approach — `set_size_request` only sets the minimum, not the
/// maximum. CSS max-width/max-height affect the natural size reported during measure.
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

    // Probe dimensions from file to compute display size without loading full texture.
    // Falls back to max size if file can't be read.
    let (display_w, display_h) = gtk4::gdk::Texture::from_filename(path)
        .ok()
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

    // Lazy load: only hold texture in GPU while widget is visible.
    // Use weak refs to avoid prevent widget → closure → widget reference cycles.
    let path_owned = path.to_string();
    let pic_weak = pic.downgrade();
    pic.connect_map(move |_| {
        let Some(p) = pic_weak.upgrade() else { return };
        if p.paintable().is_some() {
            return;
        }
        if let Ok(tex) = gtk4::gdk::Texture::from_filename(&path_owned) {
            p.set_paintable(Some(&tex));
        }
    });
    let pic_weak2 = pic.downgrade();
    pic.connect_unmap(move |_| {
        if let Some(p) = pic_weak2.upgrade() {
            p.set_paintable(None::<&gtk4::gdk::Paintable>);
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
    gesture.connect_released(move |_, _, _, _| {
        let borrow = cb.borrow();
        if let Some(f) = borrow.as_ref() {
            f();
        }
    });
    frame.add_controller(gesture);

    container.append(&frame);
}

/// Render a video: thumbnail placeholder + play-in-player button.
fn build_video_widget(container: &Box, path: &str) {
    // Video widget with lazy loading: the gtk4::Video is only populated when
    // the widget is mapped (visible). When unmapped the media stream is cleared.
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
    popout_btn.connect_clicked(move |_| {
        open_video_window(&path_popout);
    });
    overlay.add_overlay(&popout_btn);

    wrapper.append(&overlay);

    // Lazy load: set video file when mapped, fully tear down on unmap.
    // Use weak refs to avoid widget → closure → widget reference cycles.
    let path_map = path.to_string();
    let video_weak_map = video.downgrade();
    wrapper.connect_map(move |_| {
        let Some(v) = video_weak_map.upgrade() else {
            return;
        };
        if v.media_stream().is_some() {
            return;
        }
        let file = gtk4::gio::File::for_path(&path_map);
        v.set_file(Some(&file));
    });
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
    let gesture = GestureClick::new();
    gesture.set_button(1);
    gesture.connect_released(move |_, _, _, _| {
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
    wrapper.add_controller(gesture);
    wrapper.set_cursor_from_name(Some("pointer"));

    container.append(&wrapper);
}

/// State for a lightweight GIF animation (frame textures + timer).
struct GifAnimation {
    frames: Vec<gtk4::gdk::Texture>,
    index: usize,
    timer_id: Option<glib::SourceId>,
}

/// Extract frames from an MP4 using ffmpeg into a cache dir.
/// Returns the cache directory path. Frames are named frame_001.png, frame_002.png, etc.
fn extract_gif_frames(mp4_path: &str) -> Option<PathBuf> {
    // Use a stable cache dir based on the filename so we only extract once
    let hash = mp4_path.len() as u64
        ^ mp4_path
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let cache_dir = std::env::temp_dir().join(format!("wa_gif_{hash:016x}"));
    // If we already extracted, skip ffmpeg
    if cache_dir.join("frame_001.png").exists() {
        return Some(cache_dir);
    }
    std::fs::create_dir_all(&cache_dir).ok()?;
    let status = std::process::Command::new("ffmpeg")
        .args(["-i", mp4_path, "-vf", "fps=15,scale=280:-1", "-y"])
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
/// ~15MB per GIF instead of ~400MB with MediaFile.
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
            for i in 1.. {
                let frame_path = cache_dir.join(format!("frame_{i:03}.png"));
                if !frame_path.exists() {
                    break;
                }
                if let Ok(tex) = gtk4::gdk::Texture::from_filename(&frame_path) {
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
            // Start cycling at ~15fps (67ms per frame).
            // Timer closure uses weak ref to pic — returns Break if widget is gone.
            let pic_weak_timer = pic_weak_inner.clone();
            let anim_timer = anim_c.clone();
            let timer_id =
                glib::timeout_add_local(std::time::Duration::from_millis(67), move || {
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
    let play_btn = Button::from_icon_name("media-playback-start-symbolic");
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

    // For PDFs: show thumbnail + filename in a fixed-width wrapper so text never exceeds image
    let mut pdf_wrapper_width: Option<i32> = None;
    if is_pdf {
        let thumb_path = format!("{path}.thumb.png");
        if !std::path::Path::new(&thumb_path).exists() {
            let path_c = path.to_string();
            let _ = std::process::Command::new("pdftocairo")
                .args([
                    "-png",
                    "-f",
                    "1",
                    "-l",
                    "1",
                    "-scale-to",
                    "380",
                    "-singlefile",
                    &path_c,
                    &format!("{path_c}.thumb"),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        if std::path::Path::new(&thumb_path).exists() {
            if let Ok(tex) = gtk4::gdk::Texture::from_filename(&thumb_path) {
                let scale = (380.0 / tex.width() as f64)
                    .min(480.0 / tex.height() as f64)
                    .min(1.0);
                let w = (tex.width() as f64 * scale).round() as i32;
                let h = (tex.height() as f64 * scale).round() as i32;
                pdf_wrapper_width = Some(w);

                let pic = gtk4::Picture::new();
                pic.set_can_shrink(true);
                pic.set_content_fit(gtk4::ContentFit::Contain);
                pic.set_size_request(w, h);
                pic.set_paintable(Some(&tex));

                let frame = Box::new(Orientation::Vertical, 0);
                frame.set_overflow(gtk4::Overflow::Hidden);
                frame.set_cursor_from_name(Some("pointer"));
                frame.append(&pic);

                let path_c = path.to_string();
                let gesture = GestureClick::new();
                gesture.set_button(1);
                gesture.connect_released(move |_, _, _, _| open_with_xdg(&path_c));
                frame.add_controller(gesture);

                container.append(&frame);
            }
        }
    }

    // File info row: icon + filename — width matched to PDF thumbnail if present
    let hbox = Box::new(Orientation::Horizontal, 8);
    hbox.set_cursor_from_name(Some("pointer"));
    hbox.set_hexpand(false);
    // If PDF thumbnail exists, hard-cap the filename row to the thumbnail width
    if let Some(w) = pdf_wrapper_width {
        hbox.set_size_request(w, -1);
        hbox.set_overflow(gtk4::Overflow::Hidden);
    }

    let icon = Label::new(Some(if is_pdf { "📕" } else { "📄" }));
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

    let path_owned = path.to_string();
    let gesture = GestureClick::new();
    gesture.set_button(1);
    gesture.connect_released(move |_, _, _, _| open_with_xdg(&path_owned));
    hbox.add_controller(gesture);

    // Filename is inside the content box which is inside the bubble Clamp (380px).
    // No extra limiter needed — the Clamp handles max width.
    container.append(&hbox);
}

/// Placeholder shown while media is still downloading.
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
            format!("📄 {decoded}")
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

/// Simple percent-decode (%XX → char) for display purposes.
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
    let win_c = window.clone();
    close_btn.connect_clicked(move |_| win_c.close());
    overlay.add_overlay(&close_btn);

    // Escape to close
    let key_ctrl = gtk4::EventControllerKey::new();
    let win_c = window.clone();
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            win_c.close();
            gtk4::glib::Propagation::Stop
        } else {
            gtk4::glib::Propagation::Proceed
        }
    });
    window.add_controller(key_ctrl);

    window.set_child(Some(&overlay));
    window.fullscreen();
    window.present();
}

fn open_with_xdg(path: &str) {
    // Spawn xdg-open fully detached from the GTK process
    use std::process::{Command, Stdio};
    let path = path.to_string();
    std::thread::spawn(move || {
        let _ = Command::new("setsid")
            .arg("xdg-open")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    });
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
