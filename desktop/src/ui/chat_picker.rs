//! Universal chat/contact picker dialog.
//! Used by: forward messages, share contact, and any future feature needing a chat selection.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow, SearchEntry,
};
use libadwaita as adw;

use crate::bridge::ChatSummary;

/// Result returned when user picks chats.
pub enum PickerResult {
    /// User selected one or more chats
    Selected(Vec<String>), // chat_id list
    /// User cancelled
    Cancelled,
}

/// Open a modal chat picker dialog.
/// `title` — dialog title
/// `multi_select` — allow multiple selections with checkboxes
/// `parent` — parent window for modality
/// `callback` — called with selected chat IDs when user confirms
pub fn show_chat_picker(
    title: &str,
    multi_select: bool,
    parent: Option<&gtk4::Window>,
    callback: impl Fn(Vec<String>) + 'static,
) {
    let chats = crate::ui::runtime::load_chats();
    if chats.is_empty() {
        return;
    }
    show_chat_picker_with_chats(title, multi_select, parent, &chats, &[], callback);
}

/// Open a modal chat picker with pre-selected chat IDs (shown as checked).
pub fn show_chat_picker_preselected(
    title: &str,
    parent: Option<&gtk4::Window>,
    pre_selected: &[String],
    callback: impl Fn(Vec<String>) + 'static,
) {
    let chats = crate::ui::runtime::load_chats();
    if chats.is_empty() {
        return;
    }
    show_chat_picker_with_chats(title, true, parent, &chats, pre_selected, callback);
}

/// Open picker with pre-loaded chat list.
/// `pre_selected` — chat IDs that should start checked (multi-select only).
pub fn show_chat_picker_with_chats(
    title: &str,
    multi_select: bool,
    parent: Option<&gtk4::Window>,
    chats: &[ChatSummary],
    pre_selected: &[String],
    callback: impl Fn(Vec<String>) + 'static,
) {
    let win = gtk4::Window::builder()
        .title(title)
        .default_width(500)
        .default_height(800)
        .modal(true)
        .build();
    if let Some(p) = parent {
        win.set_transient_for(Some(p));
    }

    let vbox = Box::new(Orientation::Vertical, 0);

    let search = SearchEntry::new();
    search.set_placeholder_text(Some("Search chats…"));
    search.set_margin_start(12);
    search.set_margin_end(12);
    search.set_margin_top(12);
    search.set_margin_bottom(8);

    let list = ListBox::new();
    list.add_css_class("navigation-sidebar");

    let selected: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(pre_selected.to_vec()));

    if multi_select {
        list.set_selection_mode(gtk4::SelectionMode::None);
    } else {
        list.set_selection_mode(gtk4::SelectionMode::Single);
    }

    // Bottom bar for multi-select
    let bottom_bar = Box::new(Orientation::Horizontal, 8);
    bottom_bar.set_margin_start(12);
    bottom_bar.set_margin_end(12);
    bottom_bar.set_margin_top(8);
    bottom_bar.set_margin_bottom(12);

    let count_label = Label::new(Some("Select chats"));
    count_label.set_hexpand(true);
    count_label.set_halign(Align::Start);
    count_label.add_css_class("dim-label");
    let send_btn = Button::with_label("OK");
    send_btn.add_css_class("suggested-action");
    if multi_select {
        let n = pre_selected.len();
        if n > 0 {
            count_label.set_text(&format!("{n} selected"));
            send_btn.set_sensitive(true);
        } else {
            send_btn.set_sensitive(false);
        }
    }
    bottom_bar.append(&count_label);
    bottom_bar.append(&send_btn);

    for chat in chats {
        if chat.id.contains("@broadcast") {
            continue;
        }
        let row = ListBoxRow::new();
        row.set_size_request(-1, 72);
        let hbox = Box::new(Orientation::Horizontal, 12);
        hbox.set_margin_start(12);
        hbox.set_margin_end(12);
        hbox.set_margin_top(12);
        hbox.set_margin_bottom(12);

        if multi_select {
            let check = gtk4::CheckButton::new();
            // Pre-select if this chat was in the pre_selected list
            if pre_selected.contains(&chat.id) {
                check.set_active(true);
            }
            let chat_id = chat.id.clone();
            let sel = selected.clone();
            let lbl_ref = count_label.clone();
            let btn_ref = send_btn.clone();
            check.connect_toggled(move |cb| {
                let mut s = sel.borrow_mut();
                if cb.is_active() {
                    if !s.contains(&chat_id) {
                        s.push(chat_id.clone());
                    }
                } else {
                    s.retain(|id| id != &chat_id);
                }
                let n = s.len();
                let text = if n == 0 {
                    "Select chats".to_string()
                } else {
                    format!("{n} selected")
                };
                lbl_ref.set_text(&text);
                btn_ref.set_sensitive(n > 0);
            });
            hbox.append(&check);
        }

        let av = adw::Avatar::new(52, Some(&chat.name), true);
        let safe = chat.id.replace(['/', '\\', '@', ':'], "_");
        let av_path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
        if av_path.exists() {
            if let Ok(tex) = gtk4::gdk::Texture::from_filename(&av_path) {
                av.set_custom_image(Some(&tex));
            }
        }

        let name_lbl = Label::new(Some(&chat.name));
        name_lbl.set_hexpand(true);
        name_lbl.set_halign(Align::Start);
        name_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        hbox.append(&av);
        hbox.append(&name_lbl);
        row.set_child(Some(&hbox));
        row.set_widget_name(&chat.id);
        list.append(&row);
    }

    // Search filter
    let search_ref = search.clone();
    list.set_filter_func(move |row| {
        let q = search_ref.text().to_lowercase();
        if q.is_empty() {
            return true;
        }
        let q_digits: String = q.chars().filter(|c| c.is_ascii_digit()).collect();
        let jid = row.widget_name().to_lowercase();
        if !q_digits.is_empty() && jid.contains(&q_digits) {
            return true;
        }
        row.child()
            .and_then(|hb| hb.last_child())
            .and_then(|c| c.downcast::<Label>().ok())
            .map(|l| l.text().to_lowercase().contains(&q))
            .unwrap_or(true)
    });
    search.connect_search_changed({
        let list_ref = list.clone();
        move |_| list_ref.invalidate_filter()
    });

    // Row click for single-select: select and close
    if !multi_select {
        let cb = Rc::new(callback);
        let win_c = win.clone();
        list.connect_row_activated(move |_, row| {
            let id = row.widget_name().to_string();
            cb(vec![id]);
            win_c.close();
        });
    } else {
        // Row click toggles checkbox
        list.connect_row_activated(move |_, row| {
            if let Some(hbox) = row.child() {
                if let Some(first) = hbox.first_child() {
                    if let Ok(cb) = first.downcast::<gtk4::CheckButton>() {
                        cb.set_active(!cb.is_active());
                    }
                }
            }
        });

        // OK button for multi-select
        let cb = Rc::new(callback);
        let sel_ref = selected.clone();
        let win_c = win.clone();
        send_btn.connect_clicked(move |_| {
            let ids = sel_ref.borrow().clone();
            cb(ids);
            win_c.close();
        });
    }

    let scroll = ScrolledWindow::new();
    scroll.set_child(Some(&list));
    scroll.set_vexpand(true);
    scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

    vbox.append(&search);
    vbox.append(&scroll);
    if multi_select {
        vbox.append(&gtk4::Separator::new(Orientation::Horizontal));
        vbox.append(&bottom_bar);
    }
    win.set_child(Some(&vbox));
    win.present();
}
