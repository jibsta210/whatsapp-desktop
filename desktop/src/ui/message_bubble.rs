use gtk4::prelude::*;
use gtk4::{Align, Box, Label, Orientation, Widget};
use libadwaita as adw;

use crate::bridge::{IncomingMessage, ReceiptStatus};

/// Renders a single message bubble (sent or received).
/// Handles: plain text, reply context, forwarded badge, receipt ticks.
#[derive(Clone)]
pub struct MessageBubble {
    root: Box,
    text_label: Label,
    time_label: Label,
    receipt_label: Label,
    msg_id: String,
}

impl MessageBubble {
    pub fn new(msg: &IncomingMessage) -> Self {
        let root = Box::new(Orientation::Vertical, 2);
        root.set_margin_top(2);
        root.set_margin_bottom(2);
        root.set_margin_start(8);
        root.set_margin_end(8);

        let bubble = Box::new(Orientation::Vertical, 4);
        bubble.set_margin_start(4);
        bubble.set_margin_end(4);

        if msg.is_from_me {
            bubble.set_halign(Align::End);
            bubble.add_css_class("message-bubble-out");
        } else {
            bubble.set_halign(Align::Start);
            bubble.add_css_class("message-bubble-in");
        }

        // Forwarded badge
        if msg.is_forwarded {
            let fwd_box = Box::new(Orientation::Horizontal, 4);
            fwd_box.add_css_class("forwarded-badge");
            let fwd_label = Label::new(Some("↪ Forwarded"));
            fwd_label.add_css_class("caption");
            fwd_label.add_css_class("dim-label");
            fwd_box.append(&fwd_label);
            bubble.append(&fwd_box);
        }

        // Reply context (quoted message)
        if let (Some(quoted_text), Some(quoted_sender)) =
            (&msg.quoted_text, &msg.quoted_sender)
        {
            let reply_box = Box::new(Orientation::Vertical, 2);
            reply_box.add_css_class("reply-context");

            let sender_label = Label::new(Some(quoted_sender));
            sender_label.add_css_class("caption");
            sender_label.add_css_class("accent");
            sender_label.set_halign(Align::Start);

            let quoted_label = Label::new(Some(quoted_text));
            quoted_label.add_css_class("caption");
            quoted_label.add_css_class("dim-label");
            quoted_label.set_halign(Align::Start);
            quoted_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            quoted_label.set_max_width_chars(40);
            quoted_label.set_lines(2);
            quoted_label.set_wrap(true);

            reply_box.append(&sender_label);
            reply_box.append(&quoted_label);
            bubble.append(&reply_box);
        }

        // Sender name (in groups, for incoming messages)
        if !msg.is_from_me && !msg.sender_name.is_empty() {
            let sender = Label::new(Some(&msg.sender_name));
            sender.add_css_class("caption");
            sender.add_css_class("accent");
            sender.set_halign(Align::Start);
            bubble.append(&sender);
        }

        // Message text
        let text = msg.text.as_deref().unwrap_or("[media]");
        let text_label = Label::new(Some(text));
        text_label.set_wrap(true);
        text_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        text_label.set_halign(Align::Start);
        text_label.set_selectable(true);
        text_label.set_xalign(0.0);
        bubble.append(&text_label);

        // Bottom row: time + receipt
        let meta_row = Box::new(Orientation::Horizontal, 4);
        meta_row.set_halign(Align::End);

        let time_label = Label::new(Some(&format_time(msg.timestamp)));
        time_label.add_css_class("caption");
        time_label.add_css_class("dim-label");

        let receipt_label = Label::new(Some(if msg.is_from_me { "✓" } else { "" }));
        receipt_label.add_css_class("caption");
        receipt_label.add_css_class("dim-label");

        meta_row.append(&time_label);
        if msg.is_from_me {
            meta_row.append(&receipt_label);
        }

        bubble.append(&meta_row);
        root.append(&bubble);

        Self {
            root,
            text_label,
            time_label,
            receipt_label,
            msg_id: msg.id.clone(),
        }
    }

    pub fn widget(&self) -> &Box {
        &self.root
    }

    pub fn msg_id(&self) -> &str {
        &self.msg_id
    }

    pub fn update_receipt(&self, status: &ReceiptStatus) {
        let tick = match status {
            ReceiptStatus::Sent => "✓",
            ReceiptStatus::Delivered => "✓✓",
            ReceiptStatus::Read => "✓✓", // blue ticks handled via CSS class
        };
        self.receipt_label.set_text(tick);
        if matches!(status, ReceiptStatus::Read) {
            self.receipt_label.remove_css_class("dim-label");
            self.receipt_label.add_css_class("accent");
        }
    }
}

fn format_time(ts: i64) -> String {
    use chrono::{DateTime, Local, Utc};
    let dt: DateTime<Local> = DateTime::from(
        DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default()
    );
    dt.format("%-I:%M %p").to_string()
}
