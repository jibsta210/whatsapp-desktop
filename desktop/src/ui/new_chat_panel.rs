use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow, SearchEntry,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, ChatSummary, WaCommand};

const MAX_RENDERED_CONTACTS: usize = 100;

struct ContactData {
    jid: String,
    name: String,
    /// Existing direct chats sort first, with newest activity first. Contacts
    /// without a chat follow alphabetically.
    recent_rank: usize,
}

#[derive(Clone)]
pub struct NewChatPanel {
    inner: Rc<NewChatInner>,
}

struct NewChatInner {
    root: Box,
    search_entry: SearchEntry,
    contacts_list: ListBox,
    /// Row shown when search looks like a phone number — "Search WhatsApp for +1234..."
    phone_lookup_row: ListBoxRow,
    phone_lookup_label: Label,
    /// Row shown after lookup succeeds — "Start chat with +1234..."
    phone_result_row: ListBoxRow,
    phone_result_label: Label,
    bridge: Arc<Bridge>,
    on_chat_selected: Rc<dyn Fn(String, String)>,
    on_back: RefCell<Option<std::boxed::Box<dyn Fn()>>>,
    found_jid: RefCell<Option<String>>,
    /// Normalized number for the only lookup result the UI may currently accept.
    pending_phone: RefCell<Option<String>>,
    lookup_generation: Cell<u64>,
    /// Lightweight backing store. GTK rows and avatar textures are created
    /// only for the small, currently visible result window.
    contacts: RefCell<Vec<ContactData>>,
}

impl NewChatPanel {
    pub fn new(bridge: Arc<Bridge>, on_chat_selected: impl Fn(String, String) + 'static) -> Self {
        let root = Box::new(Orientation::Vertical, 0);
        root.set_width_request(360);
        root.set_vexpand(true);

        // Header with back button
        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        let title = Label::new(Some("New chat"));
        title.add_css_class("title");
        header.set_title_widget(Some(&title));

        let back_btn = Button::from_icon_name("go-previous-symbolic");
        back_btn.add_css_class("flat");
        back_btn.set_tooltip_text(Some("Back"));
        header.pack_start(&back_btn);

        // Unified search — searches contacts AND phone numbers
        let search_entry = SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search name or phone number"));
        search_entry.set_margin_start(8);
        search_entry.set_margin_end(8);
        search_entry.set_margin_top(6);
        search_entry.set_margin_bottom(6);

        // New group button
        let new_group_row = Box::new(Orientation::Horizontal, 8);
        new_group_row.set_margin_start(12);
        new_group_row.set_margin_end(12);
        new_group_row.set_margin_top(4);
        new_group_row.set_margin_bottom(4);
        let group_icon = Label::new(Some("👥"));
        let group_label = Label::new(Some("New group"));
        group_label.set_hexpand(true);
        group_label.set_halign(Align::Start);
        new_group_row.append(&group_icon);
        new_group_row.append(&group_label);

        let group_btn = Button::new();
        group_btn.set_child(Some(&new_group_row));
        group_btn.add_css_class("flat");
        group_btn.set_margin_start(4);
        group_btn.set_margin_end(4);

        // Contact list
        let contacts_list = ListBox::new();
        contacts_list.set_selection_mode(gtk4::SelectionMode::Single);
        contacts_list.add_css_class("navigation-sidebar");

        // Phone lookup row (shown when search looks like a phone number)
        let phone_lookup_row = ListBoxRow::new();
        let phone_lookup_hbox = Box::new(Orientation::Horizontal, 8);
        phone_lookup_hbox.set_margin_start(12);
        phone_lookup_hbox.set_margin_end(12);
        phone_lookup_hbox.set_margin_top(8);
        phone_lookup_hbox.set_margin_bottom(8);
        let phone_icon = Label::new(Some("🔍"));
        let phone_lookup_label = Label::new(Some(""));
        phone_lookup_label.set_hexpand(true);
        phone_lookup_label.set_halign(Align::Start);
        phone_lookup_hbox.append(&phone_icon);
        phone_lookup_hbox.append(&phone_lookup_label);
        phone_lookup_row.set_child(Some(&phone_lookup_hbox));
        phone_lookup_row.set_widget_name("phone-lookup");
        phone_lookup_row.set_visible(false);

        // Phone result row (shown after successful lookup)
        let phone_result_row = ListBoxRow::new();
        let phone_result_hbox = Box::new(Orientation::Horizontal, 8);
        phone_result_hbox.set_margin_start(12);
        phone_result_hbox.set_margin_end(12);
        phone_result_hbox.set_margin_top(8);
        phone_result_hbox.set_margin_bottom(8);
        let result_icon = Label::new(Some("💬"));
        let phone_result_label = Label::new(Some(""));
        phone_result_label.set_hexpand(true);
        phone_result_label.set_halign(Align::Start);
        phone_result_hbox.append(&result_icon);
        phone_result_hbox.append(&phone_result_label);
        phone_result_row.set_child(Some(&phone_result_hbox));
        phone_result_row.set_widget_name("phone-result");
        phone_result_row.set_visible(false);

        // Add special rows at top of list
        contacts_list.prepend(&phone_result_row);
        contacts_list.prepend(&phone_lookup_row);

        let contacts_scroll = ScrolledWindow::new();
        contacts_scroll.set_child(Some(&contacts_list));
        contacts_scroll.set_vexpand(true);

        root.append(&header);
        root.append(&search_entry);
        root.append(&group_btn);
        root.append(&gtk4::Separator::new(Orientation::Horizontal));
        root.append(&contacts_scroll);

        let inner = Rc::new(NewChatInner {
            root,
            search_entry,
            contacts_list,
            phone_lookup_row,
            phone_lookup_label,
            phone_result_row,
            phone_result_label,
            bridge,
            on_chat_selected: Rc::new(on_chat_selected),
            on_back: RefCell::new(None),
            found_jid: RefCell::new(None),
            pending_phone: RefCell::new(None),
            lookup_generation: Cell::new(0),
            contacts: RefCell::new(Vec::new()),
        });

        // Search: filter contacts + auto phone lookup
        {
            let inner_weak = Rc::downgrade(&inner);
            inner.search_entry.connect_search_changed(move |entry| {
                let Some(inner_c) = inner_weak.upgrade() else {
                    return;
                };
                let query = entry.text().to_string();
                let generation = inner_c.lookup_generation.get().wrapping_add(1);
                inner_c.lookup_generation.set(generation);
                *inner_c.found_jid.borrow_mut() = None;

                // Check if query looks like a phone number
                let trimmed = query.trim().replace([' ', '-', '(', ')'], "");
                let is_phone =
                    trimmed.len() >= 4 && trimmed.chars().all(|c| c.is_ascii_digit() || c == '+');

                if is_phone {
                    *inner_c.pending_phone.borrow_mut() = Some(normalize_phone(&trimmed));
                    inner_c
                        .phone_lookup_label
                        .set_text(&format!("Search WhatsApp for {trimmed}"));
                    inner_c.phone_lookup_row.set_visible(true);
                    inner_c.phone_result_row.set_visible(false);

                    // Auto-lookup after 800ms debounce. The per-panel generation
                    // also advances when the text stops being a phone number,
                    // so an old timer cannot issue a request for stale input.
                    let inner_weak = Rc::downgrade(&inner_c);
                    let phone = trimmed.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(800),
                        move || {
                            if let Some(inner) = inner_weak.upgrade() {
                                if inner.lookup_generation.get() == generation {
                                    inner
                                        .bridge
                                        .send_command(WaCommand::CheckOnWhatsApp { phone });
                                }
                            }
                        },
                    );
                } else {
                    *inner_c.pending_phone.borrow_mut() = None;
                    inner_c.phone_lookup_row.set_visible(false);
                    inner_c.phone_result_row.set_visible(false);
                }

                render_contacts(&inner_c, &query);
            });
        }

        // Stack pages are mapped only while shown. Drop all contact widgets
        // (and their avatar paintables) as soon as this page is left, then
        // rebuild its bounded result window when reopened.
        {
            let inner_weak = Rc::downgrade(&inner);
            inner.root.connect_map(move |_| {
                if let Some(inner) = inner_weak.upgrade() {
                    render_contacts(&inner, inner.search_entry.text().as_str());
                }
            });
        }
        {
            let inner_weak = Rc::downgrade(&inner);
            inner.root.connect_unmap(move |_| {
                if let Some(inner) = inner_weak.upgrade() {
                    clear_rendered_contacts(&inner.contacts_list);
                }
            });
        }

        // Row activation — handle contacts and phone result row
        {
            let inner_weak = Rc::downgrade(&inner);
            inner.contacts_list.connect_row_activated(move |_, row| {
                let Some(inner_c) = inner_weak.upgrade() else {
                    return;
                };
                let name = row.widget_name().to_string();
                if name == "phone-result" {
                    // Start chat with found phone JID
                    if let Some(jid) = inner_c.found_jid.borrow().clone() {
                        inner_c
                            .bridge
                            .send_command(WaCommand::StartNewChat { jid: jid.clone() });
                        let display = inner_c.search_entry.text().to_string();
                        (inner_c.on_chat_selected)(jid, display);
                        if let Some(back) = inner_c.on_back.borrow().as_ref() {
                            back();
                        }
                    }
                } else if name == "phone-lookup" {
                    // Manual trigger of phone search
                    let query = inner_c.search_entry.text().to_string();
                    let trimmed = query.trim().replace([' ', '-', '(', ')'], "");
                    *inner_c.pending_phone.borrow_mut() = Some(normalize_phone(&trimmed));
                    inner_c
                        .bridge
                        .send_command(WaCommand::CheckOnWhatsApp { phone: trimmed });
                } else {
                    // Regular contact
                    let jid = name;
                    let display = row
                        .child()
                        .and_then(|c| c.last_child())
                        .and_then(|c| c.downcast::<Label>().ok())
                        .map(|l| l.text().to_string())
                        .unwrap_or_default();
                    (inner_c.on_chat_selected)(jid.clone(), display);
                    if let Some(back) = inner_c.on_back.borrow().as_ref() {
                        back();
                    }
                }
            });
        }

        // Back button
        {
            let inner_weak = Rc::downgrade(&inner);
            back_btn.connect_clicked(move |_| {
                if let Some(inner) = inner_weak.upgrade() {
                    if let Some(back) = inner.on_back.borrow().as_ref() {
                        back();
                    }
                }
            });
        }

        // New group button
        {
            let inner_weak = Rc::downgrade(&inner);
            group_btn.connect_clicked(move |_| {
                if let Some(inner) = inner_weak.upgrade() {
                    show_create_group_dialog(&inner);
                }
            });
        }

        NewChatPanel { inner }
    }

    pub fn widget(&self) -> &Box {
        &self.inner.root
    }

    /// Set the callback for the back button (switches sidebar back to chat list).
    pub fn connect_back(&self, callback: impl Fn() + 'static) {
        *self.inner.on_back.borrow_mut() = Some(std::boxed::Box::new(callback));
    }

    /// Populate the contact list from existing chats.
    pub fn load_contacts(
        &self,
        chats: &[ChatSummary],
        all_contacts: &std::collections::HashMap<String, String>,
    ) {
        let mut direct_chats: Vec<_> = chats.iter().filter(|chat| !chat.is_group).collect();
        direct_chats.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let mut contacts = Vec::with_capacity(direct_chats.len() + all_contacts.len());
        let mut existing_ids = std::collections::HashSet::with_capacity(direct_chats.len());
        for (recent_rank, chat) in direct_chats.into_iter().enumerate() {
            if !existing_ids.insert(chat.id.as_str()) {
                continue;
            }
            contacts.push(ContactData {
                jid: chat.id.clone(),
                name: chat.name.clone(),
                recent_rank,
            });
        }

        // Contacts without an existing direct chat follow the recent chats.
        let mut contact_entries: Vec<(&String, &String)> = all_contacts
            .iter()
            .filter(|(jid, _)| {
                jid.ends_with("@s.whatsapp.net") && !existing_ids.contains(jid.as_str())
            })
            .collect();
        contact_entries.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

        let contact_rank_start = contacts.len();
        for (index, (jid, name)) in contact_entries.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            contacts.push(ContactData {
                jid: jid.clone(),
                name: name.clone(),
                recent_rank: contact_rank_start + index,
            });
        }

        *self.inner.contacts.borrow_mut() = contacts;
        if self.inner.root.is_mapped() {
            render_contacts(&self.inner, self.inner.search_entry.text().as_str());
        } else {
            clear_rendered_contacts(&self.inner.contacts_list);
        }
    }

    /// Handle phone lookup result from runtime.
    pub fn set_phone_result(&self, phone: &str, jid: Option<&str>, is_registered: bool) {
        let response_phone = normalize_phone(phone);
        let current_phone = normalize_phone(self.inner.search_entry.text().as_str());
        if self.inner.pending_phone.borrow().as_deref() != Some(response_phone.as_str())
            || current_phone != response_phone
        {
            return;
        }
        self.inner.phone_lookup_row.set_visible(false);
        if is_registered {
            if let Some(jid) = jid {
                self.inner
                    .phone_result_label
                    .set_text(&format!("{phone} — tap to start chat"));
                self.inner.phone_result_row.set_visible(true);
                *self.inner.found_jid.borrow_mut() = Some(jid.to_string());
            }
        } else {
            self.inner
                .phone_result_label
                .set_text(&format!("{phone} is not on WhatsApp"));
            self.inner.phone_result_row.set_visible(true);
            *self.inner.found_jid.borrow_mut() = None;
        }
    }
}

fn clear_rendered_contacts(list: &ListBox) {
    let mut child = list.first_child();
    while let Some(widget) = child {
        let next = widget.next_sibling();
        if let Ok(row) = widget.downcast::<ListBoxRow>() {
            let name = row.widget_name();
            if name != "phone-lookup" && name != "phone-result" {
                list.remove(&row);
            }
        }
        child = next;
    }
}

fn render_contacts(inner: &NewChatInner, query: &str) {
    clear_rendered_contacts(&inner.contacts_list);

    let query = query.trim().to_lowercase();
    let query_digits: String = query.chars().filter(char::is_ascii_digit).collect();
    let contacts = inner.contacts.borrow();

    // The backing store is already ordered for the empty state, so reopening
    // the panel remains O(the 100 rows we actually materialize).
    if query.is_empty() {
        for contact in contacts.iter().take(MAX_RENDERED_CONTACTS) {
            inner.contacts_list.append(&build_contact_row(contact));
        }
        return;
    }

    let mut matches: Vec<(&ContactData, u8)> = contacts
        .iter()
        .filter_map(|contact| {
            let name = contact.name.to_lowercase();
            let jid = contact.jid.to_lowercase();
            let jid_digits: String = jid.chars().filter(char::is_ascii_digit).collect();
            let score = if name == query {
                0
            } else if name.starts_with(&query) {
                1
            } else if name.split_whitespace().any(|word| word.starts_with(&query)) {
                2
            } else if name.contains(&query) {
                3
            } else if !query_digits.is_empty() && jid_digits.starts_with(&query_digits) {
                4
            } else if !query_digits.is_empty() && jid_digits.contains(&query_digits) {
                5
            } else if jid.contains(&query) {
                6
            } else {
                return None;
            };
            Some((contact, score))
        })
        .collect();

    matches.sort_by(|(a, score_a), (b, score_b)| {
        score_a
            .cmp(score_b)
            .then_with(|| a.recent_rank.cmp(&b.recent_rank))
    });

    for (contact, _) in matches.into_iter().take(MAX_RENDERED_CONTACTS) {
        inner.contacts_list.append(&build_contact_row(contact));
    }
}

fn build_contact_row(contact: &ContactData) -> ListBoxRow {
    let row = ListBoxRow::new();
    let hbox = Box::new(Orientation::Horizontal, 8);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);
    hbox.set_margin_top(6);
    hbox.set_margin_bottom(6);

    let avatar = adw::Avatar::new(36, Some(&contact.name), true);
    let safe = contact.jid.replace(['/', '\\', '@', ':'], "_");
    let avatar_path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
    if avatar_path.exists() {
        if let Some(texture) = crate::ui::texture_cache::texture_thumbnail(&avatar_path, 96) {
            avatar.set_custom_image(Some(&texture));
        }
    }

    let name = Label::new(Some(&contact.name));
    name.set_hexpand(true);
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);

    hbox.append(&avatar);
    hbox.append(&name);
    row.set_child(Some(&hbox));
    row.set_widget_name(&contact.jid);
    row
}

fn normalize_phone(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

fn show_create_group_dialog(panel: &Rc<NewChatInner>) {
    // Step 1: Ask for group name
    let dialog = libadwaita::AlertDialog::builder()
        .heading("New Group")
        .body("Enter a group name, then select members.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("next", "Next");
    dialog.set_response_appearance("next", libadwaita::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("next"));
    dialog.set_close_response("cancel");

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Group subject"));
    dialog.set_extra_child(Some(&entry));

    let parent = panel
        .root
        .root()
        .and_then(|r| r.downcast::<gtk4::Window>().ok());
    let bridge = panel.bridge.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "next" {
            return;
        }
        let subject = entry.text().to_string();
        if subject.is_empty() {
            return;
        }

        // Step 2: Pick members using universal picker
        let bridge_c = bridge.clone();
        crate::ui::chat_picker::show_chat_picker(
            &format!("Add members to {subject}"),
            true,
            parent.as_ref(),
            move |selected| {
                if !selected.is_empty() {
                    bridge_c.send_command(crate::bridge::WaCommand::CreateGroup {
                        subject: subject.clone(),
                        participants: selected,
                    });
                }
            },
        );
    });

    let parent_win = panel
        .root
        .root()
        .and_then(|r| r.downcast::<gtk4::Window>().ok());
    dialog.present(parent_win.as_ref());
}
