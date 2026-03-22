use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Label, ListBox, ListBoxRow, Orientation, SearchEntry,
    ScrolledWindow, SelectionMode, Widget,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, ChatSummary, IncomingMessage, WaCommand};

#[derive(Clone)]
pub struct ChatListPanel {
    inner: Rc<ChatListInner>,
}

struct ChatListInner {
    root: Box,
    list_box: ListBox,
    search: SearchEntry,
    bridge: Arc<Bridge>,
    // chat_id → row widget
    rows: RefCell<HashMap<String, ChatRow>>,
}

impl ChatListPanel {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        let root = Box::new(Orientation::Vertical, 0);
        root.set_width_request(360);

        // Header bar
        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        let title = Label::new(Some("WhatsApp"));
        title.add_css_class("title");
        header.set_title_widget(Some(&title));

        // Search bar
        let search = SearchEntry::new();
        search.set_placeholder_text(Some("Search chats…"));
        search.set_margin_start(8);
        search.set_margin_end(8);
        search.set_margin_top(4);
        search.set_margin_bottom(4);

        // Chat list
        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::Single);
        list_box.add_css_class("navigation-sidebar");

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list_box));

        root.append(&header);
        root.append(&search);
        root.append(&scroll);

        // Filter function for search
        let search_clone = search.clone();
        list_box.set_filter_func(move |row| {
            let query = search_clone.text().to_lowercase();
            if query.is_empty() {
                return true;
            }
            // Filter by widget name set to lowercased chat name
            let name: String = row.widget_name().into();
            if !name.is_empty() {
                return name.to_lowercase().contains(&query);
            }
            true
        });

        search.connect_search_changed({
            let list = list_box.clone();
            move |_| list.invalidate_filter()
        });

        let inner = Rc::new(ChatListInner {
            root,
            list_box,
            search,
            bridge,
            rows: RefCell::new(HashMap::new()),
        });

        ChatListPanel { inner }
    }

    pub fn widget(&self) -> &Box {
        &self.inner.root
    }

    pub fn load_chats(&self, chats: Vec<ChatSummary>) {
        let inner = &self.inner;
        // Clear existing
        while let Some(child) = inner.list_box.first_child() {
            inner.list_box.remove(&child);
        }
        inner.rows.borrow_mut().clear();

        for chat in chats {
            self.add_chat_row(chat);
        }
    }

    pub fn update_last_message(&self, chat_id: &str, msg: &IncomingMessage) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            let preview = msg.text.as_deref().unwrap_or("[media]");
            row.update_preview(preview, msg.timestamp);
        }
    }

    fn add_chat_row(&self, chat: ChatSummary) {
        let inner = &self.inner;
        let row = ChatRow::new(&chat);

        // Store chat_id in widget name for filtering
        row.gtk_row.set_widget_name(&chat.name.to_lowercase());

        let chat_id = chat.id.clone();
        let bridge = inner.bridge.clone();
        row.gtk_row.connect_activate(move |_| {
            bridge.send_command(WaCommand::LoadChat { chat_id: chat_id.clone() });
            bridge.send_command(WaCommand::MarkRead { chat_id: chat_id.clone() });
        });

        inner.list_box.append(&row.gtk_row);
        inner.rows.borrow_mut().insert(chat.id, row);
    }
}

#[derive(Clone)]
struct ChatRow {
    gtk_row: ListBoxRow,
    name_label: Label,
    preview_label: Label,
    time_label: Label,
    unread_badge: Label,
}

impl ChatRow {
    fn new(chat: &ChatSummary) -> Self {
        let gtk_row = ListBoxRow::new();

        let hbox = Box::new(Orientation::Horizontal, 12);
        hbox.set_margin_top(8);
        hbox.set_margin_bottom(8);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);

        // Avatar (initials for now, real avatars later)
        let avatar = adw::Avatar::new(48, Some(&chat.name), true);

        // Text column
        let vbox = Box::new(Orientation::Vertical, 2);
        vbox.set_hexpand(true);

        let top_row = Box::new(Orientation::Horizontal, 0);
        top_row.set_hexpand(true);

        let name_label = Label::new(Some(&chat.name));
        name_label.set_halign(Align::Start);
        name_label.set_hexpand(true);
        name_label.add_css_class("heading");
        name_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        name_label.set_max_width_chars(20);

        let time_label = Label::new(Some(&format_timestamp(chat.timestamp)));
        time_label.add_css_class("dim-label");
        time_label.add_css_class("caption");
        time_label.set_halign(Align::End);

        top_row.append(&name_label);
        top_row.append(&time_label);

        let bottom_row = Box::new(Orientation::Horizontal, 0);
        bottom_row.set_hexpand(true);

        let preview_label = Label::new(Some(&chat.last_message));
        preview_label.set_halign(Align::Start);
        preview_label.set_hexpand(true);
        preview_label.add_css_class("dim-label");
        preview_label.add_css_class("body");
        preview_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        preview_label.set_max_width_chars(25);

        let unread_badge = Label::new(Some(&chat.unread_count.to_string()));
        unread_badge.add_css_class("badge");
        unread_badge.add_css_class("success");
        unread_badge.set_visible(chat.unread_count > 0);
        unread_badge.set_halign(Align::End);

        bottom_row.append(&preview_label);
        bottom_row.append(&unread_badge);

        vbox.append(&top_row);
        vbox.append(&bottom_row);

        hbox.append(&avatar);
        hbox.append(&vbox);
        gtk_row.set_child(Some(&hbox));

        ChatRow { gtk_row, name_label, preview_label, time_label, unread_badge }
    }

    fn update_preview(&self, text: &str, timestamp: i64) {
        self.preview_label.set_text(text);
        self.time_label.set_text(&format_timestamp(timestamp));
    }
}

fn format_timestamp(ts: i64) -> String {
    use chrono::{DateTime, Local, Utc};
    let dt: DateTime<Local> = DateTime::from(DateTime::<Utc>::from_timestamp(ts, 0)
        .unwrap_or_default());
    let now = Local::now();

    if dt.date_naive() == now.date_naive() {
        dt.format("%-I:%M %p").to_string()
    } else {
        dt.format("%b %-d").to_string()
    }
}
