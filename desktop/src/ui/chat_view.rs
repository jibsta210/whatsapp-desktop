use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Entry, GestureClick, Label, ListBox, Orientation,
    ScrolledWindow, Separator, Widget,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, IncomingMessage, ReceiptStatus, WaCommand};
use crate::ui::message_bubble::MessageBubble;

#[derive(Clone)]
pub struct ChatViewPanel {
    inner: Rc<ChatViewInner>,
}

struct ChatViewInner {
    root: Box,
    messages_box: Box,
    scroll: ScrolledWindow,
    input_entry: Entry,
    send_button: Button,
    typing_label: Label,
    header_name: Label,
    bridge: Arc<Bridge>,
    current_chat_id: RefCell<Option<String>>,
    // msg_id → bubble (for receipt updates)
    bubbles: RefCell<HashMap<String, MessageBubble>>,
    // For reply: the message being replied to
    reply_context: RefCell<Option<(String, String, String)>>, // (msg_id, sender, text)
    reply_bar: Box,
    reply_label: Label,
}

impl ChatViewPanel {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        let root = Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);

        // ── Header ──
        let header = adw::HeaderBar::new();
        let header_name = Label::new(Some("Select a chat"));
        header_name.add_css_class("title");
        header.set_title_widget(Some(&header_name));

        // ── Message area ──
        let messages_box = Box::new(Orientation::Vertical, 0);
        messages_box.set_vexpand(true);

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_child(Some(&messages_box));

        // ── Typing indicator ──
        let typing_label = Label::new(None);
        typing_label.add_css_class("caption");
        typing_label.add_css_class("dim-label");
        typing_label.set_halign(Align::Start);
        typing_label.set_margin_start(16);
        typing_label.set_margin_bottom(2);
        typing_label.set_visible(false);

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

        // ── Input bar ──
        let input_bar = Box::new(Orientation::Horizontal, 8);
        input_bar.set_margin_start(8);
        input_bar.set_margin_end(8);
        input_bar.set_margin_top(8);
        input_bar.set_margin_bottom(8);

        let input_entry = Entry::new();
        input_entry.set_hexpand(true);
        input_entry.set_placeholder_text(Some("Message"));

        let send_button = Button::with_label("Send");
        send_button.add_css_class("suggested-action");

        input_bar.append(&input_entry);
        input_bar.append(&send_button);

        root.append(&header);
        root.append(&scroll);
        root.append(&typing_label);
        root.append(&reply_bar);
        root.append(&input_bar);

        let inner = Rc::new(ChatViewInner {
            root,
            messages_box,
            scroll,
            input_entry,
            send_button,
            typing_label,
            header_name,
            bridge,
            current_chat_id: RefCell::new(None),
            bubbles: RefCell::new(HashMap::new()),
            reply_context: RefCell::new(None),
            reply_bar,
            reply_label,
        });

        // Wire send button
        {
            let inner_clone = inner.clone();
            inner.send_button.connect_clicked(move |_| {
                Self::do_send(&inner_clone);
            });
        }

        // Wire Enter key in input
        {
            let inner_clone = inner.clone();
            inner.input_entry.connect_activate(move |_| {
                Self::do_send(&inner_clone);
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

        ChatViewPanel { inner }
    }

    fn do_send(inner: &Rc<ChatViewInner>) {
        let chat_id = match inner.current_chat_id.borrow().clone() {
            Some(id) => id,
            None => return,
        };

        let text = inner.input_entry.text().to_string();
        if text.trim().is_empty() {
            return;
        }

        inner.input_entry.set_text("");

        let reply = inner.reply_context.borrow().clone();
        if let Some((quoted_msg_id, quoted_sender, _)) = reply {
            inner.bridge.send_command(WaCommand::SendReply {
                chat_id,
                text,
                quoted_msg_id,
                quoted_sender,
            });
            *inner.reply_context.borrow_mut() = None;
            inner.reply_bar.set_visible(false);
        } else {
            inner.bridge.send_command(WaCommand::SendText { chat_id, text });
        }
    }

    pub fn widget(&self) -> &Box {
        &self.inner.root
    }

    pub fn open_chat(&self, chat_id: String, chat_name: &str) {
        *self.inner.current_chat_id.borrow_mut() = Some(chat_id);
        self.inner.header_name.set_text(chat_name);

        // Clear message area
        while let Some(child) = self.inner.messages_box.first_child() {
            self.inner.messages_box.remove(&child);
        }
        self.inner.bubbles.borrow_mut().clear();
    }

    pub fn append_message(&self, msg: IncomingMessage) {
        let inner = &self.inner;

        // Only render if this message belongs to the open chat
        let is_current = inner.current_chat_id.borrow()
            .as_deref()
            .map(|id| id == msg.chat_id)
            .unwrap_or(false);

        if !is_current {
            return;
        }

        let bubble = MessageBubble::new(&msg);

        // Right-click / long-press context menu for reply, forward, delete
        let gesture = GestureClick::new();
        gesture.set_button(3); // right click
        let inner_clone = inner.clone();
        let msg_clone = msg.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            show_message_menu(&inner_clone, &msg_clone);
        });
        bubble.widget().add_controller(gesture);

        inner.messages_box.append(bubble.widget());
        inner.bubbles.borrow_mut().insert(msg.id.clone(), bubble);

        // Auto-scroll to bottom
        let scroll = inner.scroll.clone();
        glib::idle_add_local_once(move || {
            let adj = scroll.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });
    }

    pub fn update_receipt(&self, msg_id: &str, status: ReceiptStatus) {
        if let Some(bubble) = self.inner.bubbles.borrow().get(msg_id) {
            bubble.update_receipt(&status);
        }
    }

    pub fn set_typing_indicator(&self, chat_id: &str, is_typing: bool) {
        let is_current = self.inner.current_chat_id.borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);

        if !is_current {
            return;
        }

        if is_typing {
            self.inner.typing_label.set_text("typing…");
            self.inner.typing_label.set_visible(true);
        } else {
            self.inner.typing_label.set_visible(false);
        }
    }
}

fn show_message_menu(inner: &Rc<ChatViewInner>, msg: &IncomingMessage) {
    use gtk4::{gio, PopoverMenu};

    let menu = gio::Menu::new();
    menu.append(Some("Reply"), Some("msg.reply"));
    menu.append(Some("Forward"), Some("msg.forward"));
    if msg.is_from_me {
        menu.append(Some("Delete for Everyone"), Some("msg.delete"));
    }

    // We build a simple action group and connect actions
    let action_group = gio::SimpleActionGroup::new();

    // Reply action
    {
        let inner_clone = inner.clone();
        let msg_clone = msg.clone();
        let action = gio::SimpleAction::new("reply", None);
        action.connect_activate(move |_, _| {
            let text_preview = msg_clone.text.clone().unwrap_or("[media]".to_string());
            let sender = msg_clone.sender_name.clone();
            inner_clone.reply_label.set_text(&format!("{}: {}", sender, text_preview));
            inner_clone.reply_bar.set_visible(true);
            *inner_clone.reply_context.borrow_mut() = Some((
                msg_clone.id.clone(),
                msg_clone.sender_id.clone(),
                text_preview,
            ));
            inner_clone.input_entry.grab_focus();
        });
        action_group.add_action(&action);
    }

    // Forward action
    {
        let inner_clone = inner.clone();
        let msg_id = msg.id.clone();
        let action = gio::SimpleAction::new("forward", None);
        action.connect_activate(move |_, _| {
            // TODO: show chat picker dialog, for now forward to same chat
            if let Some(chat_id) = inner_clone.current_chat_id.borrow().clone() {
                inner_clone.bridge.send_command(WaCommand::ForwardMessage {
                    to_chat_id: chat_id,
                    original_msg_id: msg_id.clone(),
                });
            }
        });
        action_group.add_action(&action);
    }

    // Delete for everyone
    {
        let inner_clone = inner.clone();
        let msg_id = msg.id.clone();
        let action = gio::SimpleAction::new("delete", None);
        action.connect_activate(move |_, _| {
            if let Some(chat_id) = inner_clone.current_chat_id.borrow().clone() {
                inner_clone.bridge.send_command(WaCommand::DeleteForEveryone {
                    chat_id,
                    msg_id: msg_id.clone(),
                });
            }
        });
        action_group.add_action(&action);
    }

    // Attach action group to the messages_box widget and pop up menu
    inner.messages_box.insert_action_group("msg", Some(&action_group));
    // (Full popover positioning requires the widget reference — left as TODO for next phase)
}
