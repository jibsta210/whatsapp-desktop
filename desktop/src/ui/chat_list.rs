use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, ListBox, ListBoxRow, Orientation, Popover,
    ScrolledWindow, SearchEntry, SelectionMode, Separator, ToggleButton,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, ChatSummary, IncomingMessage, WaCommand};

#[derive(Clone, Copy, PartialEq, Default)]
enum ChatFilter {
    #[default]
    All,
    Unread,
    Favourites,
    Groups,
    Archived,
}

#[derive(Clone)]
pub struct ChatListPanel {
    inner: Rc<ChatListInner>,
}

struct ChatListInner {
    root: Box,
    list_box: ListBox,
    new_chat_btn: gtk4::Button,
    bridge: Arc<Bridge>,
    on_select: Rc<dyn Fn(String, String)>,
    rows: RefCell<HashMap<String, ChatRow>>,
    timestamps: RefCell<HashMap<String, i64>>,
    active_filter: RefCell<ChatFilter>,
    /// Saved preview text for chats where a typing indicator is shown
    typing_previews: RefCell<HashMap<String, String>>,
    /// Active typers per chat (for multi-typer display)
    active_typers: RefCell<HashMap<String, Vec<String>>>,
    /// Message search results section (visible when search query matches messages)
    msg_results_section: Box,
    msg_results_box: ListBox,
    /// In-memory cache of recent messages per chat for stealth peek.
    /// Updated on every update_last_message. Keeps last 10 per chat.
    /// Wrapped in Rc so stealth hover closures can share it.
    recent_messages: Rc<RefCell<HashMap<String, Vec<IncomingMessage>>>>,
    /// Stored so we can clear it programmatically when a chat is selected
    /// from filtered results. Without clearing, the SearchEntry retains
    /// keyboard focus across the row-activation, which intercepts Ctrl+V
    /// before our paste handler on input_view sees it.
    search_entry: SearchEntry,
}

impl ChatListPanel {
    pub fn new(bridge: Arc<Bridge>, on_select: impl Fn(String, String) + 'static) -> Self {
        let root = Box::new(Orientation::Vertical, 0);
        root.set_width_request(360);
        root.add_css_class("chat-list-bg");

        // ── Search + new chat inline ──
        let search_row = Box::new(Orientation::Horizontal, 6);
        search_row.set_margin_start(10);
        search_row.set_margin_end(10);
        search_row.set_margin_top(14);
        search_row.set_margin_bottom(10);

        let search = SearchEntry::new();
        search.set_placeholder_text(Some("Search or start new chat"));
        search.set_hexpand(true);
        search.set_size_request(-1, 42); // 50% taller
        search.add_css_class("search-rounded");

        let new_chat_btn = gtk4::Button::from_icon_name("chat-message-new-symbolic");
        new_chat_btn.add_css_class("flat");
        new_chat_btn.add_css_class("circular");
        new_chat_btn.set_tooltip_text(Some("New chat"));

        search_row.append(&search);
        search_row.append(&new_chat_btn);

        // ── Filter chips ──
        // spacing=0 eliminates 1px rendering artifacts between ToggleButtons
        let filter_bar = Box::new(Orientation::Horizontal, 0);
        filter_bar.set_margin_start(12);
        filter_bar.set_margin_end(12);
        filter_bar.set_margin_top(4);
        filter_bar.set_margin_bottom(14);

        // Use regular Buttons instead of ToggleButton radio groups to
        // avoid 1px rendering artifacts that GTK4 draws between grouped toggles.
        let btn_all = make_filter_chip("All");
        let btn_unread = make_filter_chip("Unread");
        let btn_favs = make_filter_chip("Favourites");
        let btn_groups = make_filter_chip("Groups");
        let btn_archived = make_filter_chip("Archived");

        // Manual radio behavior — clicking one deactivates the others.
        // This avoids ToggleButton::set_group() which draws internal separators.
        let all_btns: Vec<ToggleButton> = vec![
            btn_all.clone(), btn_unread.clone(), btn_favs.clone(),
            btn_groups.clone(), btn_archived.clone(),
        ];
        for btn in &all_btns {
            let btns = all_btns.clone();
            let this = btn.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    for other in &btns {
                        if other != &this && other.is_active() {
                            other.set_active(false);
                        }
                    }
                }
            });
        }
        btn_all.set_active(true);

        filter_bar.append(&btn_all);
        filter_bar.append(&btn_unread);
        filter_bar.append(&btn_favs);
        filter_bar.append(&btn_groups);
        filter_bar.append(&btn_archived);

        // ── Chat list ──
        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::Single);
        list_box.add_css_class("navigation-sidebar");

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_kinetic_scrolling(true);
        scroll.set_overlay_scrolling(true);
        scroll.set_child(Some(&list_box));

        // ── Message search results section (hidden until query is typed) ──
        let msg_results_label = Label::new(Some("Messages"));
        msg_results_label.add_css_class("heading");
        msg_results_label.add_css_class("dim-label");
        msg_results_label.set_halign(Align::Start);
        msg_results_label.set_margin_start(14);
        msg_results_label.set_margin_top(8);
        msg_results_label.set_margin_bottom(4);

        let msg_results_box = ListBox::new();
        msg_results_box.set_selection_mode(SelectionMode::None);
        msg_results_box.add_css_class("navigation-sidebar");

        let msg_results_scroll = ScrolledWindow::new();
        msg_results_scroll.set_max_content_height(250);
        msg_results_scroll.set_propagate_natural_height(true);
        msg_results_scroll.set_child(Some(&msg_results_box));

        let msg_results_section = Box::new(Orientation::Vertical, 0);
        msg_results_section.append(&Separator::new(Orientation::Horizontal));
        msg_results_section.append(&msg_results_label);
        msg_results_section.append(&msg_results_scroll);
        msg_results_section.set_visible(false);

        root.append(&search_row);
        root.append(&filter_bar);
        root.append(&scroll);
        root.append(&msg_results_section);

        let inner = Rc::new(ChatListInner {
            root,
            list_box,
            new_chat_btn,
            bridge,
            on_select: Rc::new(on_select),
            rows: RefCell::new(HashMap::new()),
            timestamps: RefCell::new(HashMap::new()),
            active_filter: RefCell::new(ChatFilter::All),
            typing_previews: RefCell::new(HashMap::new()),
            active_typers: RefCell::new(HashMap::new()),
            msg_results_section,
            msg_results_box,
            recent_messages: Rc::new(RefCell::new(HashMap::new())),
            search_entry: search.clone(),
        });

        // Sort: pinned first, then newest
        {
            let weak = Rc::downgrade(&inner);
            inner.list_box.set_sort_func(move |row_a, row_b| {
                let Some(inner) = weak.upgrade() else {
                    return gtk4::Ordering::Equal;
                };
                let rows = inner.rows.borrow();
                let ts = inner.timestamps.borrow();
                let pinned_a = rows
                    .get(row_a.widget_name().as_str())
                    .map(|r| r.is_pinned.get())
                    .unwrap_or(false);
                let pinned_b = rows
                    .get(row_b.widget_name().as_str())
                    .map(|r| r.is_pinned.get())
                    .unwrap_or(false);
                if pinned_a != pinned_b {
                    return if pinned_a {
                        gtk4::Ordering::Smaller
                    } else {
                        gtk4::Ordering::Larger
                    };
                }
                let ta = ts.get(row_a.widget_name().as_str()).copied().unwrap_or(0);
                let tb = ts.get(row_b.widget_name().as_str()).copied().unwrap_or(0);
                match ta.cmp(&tb) {
                    std::cmp::Ordering::Greater => gtk4::Ordering::Smaller,
                    std::cmp::Ordering::Less => gtk4::Ordering::Larger,
                    std::cmp::Ordering::Equal => gtk4::Ordering::Equal,
                }
            });
        }

        // Filter
        {
            let search_clone = search.clone();
            let weak = Rc::downgrade(&inner);
            inner.list_box.set_filter_func(move |row| {
                let Some(inner) = weak.upgrade() else {
                    return true;
                };
                let id: String = row.widget_name().into();
                let rows = inner.rows.borrow();
                let Some(chat_row) = rows.get(&id) else {
                    return true;
                };

                let filter = *inner.active_filter.borrow();

                // Archived chats only visible in the Archived filter
                if filter == ChatFilter::Archived {
                    if !chat_row.is_archived.get() {
                        return false;
                    }
                } else if chat_row.is_archived.get() {
                    return false;
                }

                let passes = match filter {
                    ChatFilter::All => true,
                    ChatFilter::Unread => chat_row.unread_count.get() > 0,
                    ChatFilter::Favourites => {
                        chat_row.is_pinned.get() || chat_row.is_favorite.get()
                    }
                    ChatFilter::Groups => chat_row.is_group,
                    ChatFilter::Archived => true, // already filtered above
                };
                if !passes {
                    return false;
                }

                // The synthetic Verification Codes inbox lives in the
                // sidebar rail (always reachable). It only shows up in the
                // main chat list when something fresh has landed there
                // (unread > 0). Otherwise it's quiet space.
                if id == crate::bridge::VERIFICATION_CODES_CHAT_ID
                    && chat_row.unread_count.get() == 0
                {
                    return false;
                }

                let query = search_clone.text().to_lowercase();
                if query.is_empty() {
                    return true;
                }
                chat_row.chat_name.to_lowercase().contains(&query)
            });

            search.connect_search_changed({
                let list = inner.list_box.clone();
                let msg_section = inner.msg_results_section.clone();
                let msg_box = inner.msg_results_box.clone();
                let bridge = inner.bridge.clone();
                let on_select = inner.on_select.clone();
                let inner_weak = Rc::downgrade(&inner);
                // Debounce timer for message search
                let debounce: Rc<Cell<u32>> = Rc::new(Cell::new(0));
                move |entry| {
                    list.invalidate_filter();
                    let query = entry.text().to_string();
                    if query.trim().is_empty() || query.len() < 2 {
                        msg_section.set_visible(false);
                        // Clear old results
                        while let Some(child) = msg_box.first_child() {
                            msg_box.remove(&child);
                        }
                        return;
                    }
                    // Debounce: search messages after 400ms of no typing
                    let tag = debounce.get().wrapping_add(1);
                    debounce.set(tag);
                    let db = debounce.clone();
                    let q = query.clone();
                    let ms = msg_section.clone();
                    let mb = msg_box.clone();
                    let os = on_select.clone();
                    let br = bridge.clone();
                    let inner_w = inner_weak.clone();
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(400),
                        move || {
                            if db.get() != tag {
                                return; // superseded by a newer keystroke
                            }
                            // Search synchronously (message cache is small, < 50ms)
                            let hits = search_local_messages(&q);
                            // Clear old results
                            while let Some(child) = mb.first_child() {
                                mb.remove(&child);
                            }
                            if hits.is_empty() {
                                ms.set_visible(false);
                                return;
                            }
                            ms.set_visible(true);
                            for hit in hits.iter().take(15) {
                                let row = Box::new(Orientation::Vertical, 2);
                                row.set_margin_start(14);
                                row.set_margin_end(14);
                                row.set_margin_top(4);
                                row.set_margin_bottom(4);
                                let sender = Label::new(Some(
                                    if hit.sender_name.is_empty() { "You" } else { &hit.sender_name }
                                ));
                                sender.add_css_class("caption");
                                sender.add_css_class("accent");
                                sender.set_halign(Align::Start);
                                let text = Label::new(Some(
                                    &hit.text.chars().take(80).collect::<String>()
                                ));
                                text.set_halign(Align::Start);
                                text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                                text.set_max_width_chars(40);
                                row.append(&sender);
                                row.append(&text);
                                let gtk_row = ListBoxRow::new();
                                gtk_row.set_child(Some(&row));
                                gtk_row.set_selectable(true);
                                let cid = hit.chat_id.clone();
                                let cn = hit.chat_name.clone();
                                let rows_ref = inner_w.clone();
                                let os2 = os.clone();
                                let br2 = br.clone();
                                let gesture = GestureClick::new();
                                gesture.set_button(1);
                                gesture.connect_released(move |_, _, _, _| {
                                    // Look up the actual chat display name from
                                    // the existing chat list rows. Falls back to
                                    // the hit's chat_name, then to formatted JID.
                                    let name = if !cn.is_empty() {
                                        cn.clone()
                                    } else if let Some(inner) = rows_ref.upgrade() {
                                        inner
                                            .rows
                                            .borrow()
                                            .get(&cid)
                                            .map(|r| r.chat_name.clone())
                                            .filter(|n| !n.is_empty())
                                            .unwrap_or_else(|| {
                                                crate::ui::runtime::display_name_from_jid(&cid)
                                            })
                                    } else {
                                        crate::ui::runtime::display_name_from_jid(&cid)
                                    };
                                    // Note: search_entry is left intact so
                                    // the user can keep clicking through
                                    // results without re-typing the query.
                                    // Focus moves to the message input via
                                    // on_select → open_chat → grab_focus.
                                    (os2)(cid.clone(), name.clone());
                                    br2.send_command(crate::bridge::WaCommand::LoadChat {
                                        chat_id: cid.clone(),
                                        chat_name: name,
                                    });
                                });
                                gtk_row.add_controller(gesture);
                                mb.append(&gtk_row);
                            }
                        },
                    );
                }
            });
        }

        wire_filter_chip(&btn_all, ChatFilter::All, &inner);
        wire_filter_chip(&btn_unread, ChatFilter::Unread, &inner);
        wire_filter_chip(&btn_favs, ChatFilter::Favourites, &inner);
        wire_filter_chip(&btn_groups, ChatFilter::Groups, &inner);
        wire_filter_chip(&btn_archived, ChatFilter::Archived, &inner);

        // Row activation
        {
            let weak = Rc::downgrade(&inner);
            inner
                .list_box
                .connect_row_activated(move |_, activated_row| {
                    let Some(inner) = weak.upgrade() else { return };
                    let found = {
                        let rows = inner.rows.borrow();
                        rows.iter()
                            .find(|(_, r)| &r.gtk_row == activated_row)
                            .map(|(id, r)| (id.clone(), r.chat_name.clone()))
                    };
                    if let Some((chat_id, chat_name)) = found {
                        // Note: we deliberately DO NOT clear search_entry
                        // here. Users browsing search results often click
                        // through several matches; clearing forces them
                        // to retype the query each time. Focus is moved
                        // off the search entry by on_select (which calls
                        // input_view.grab_focus in chat_view::open_chat),
                        // so Ctrl+V reaches the message input correctly.
                        (inner.on_select)(chat_id.clone(), chat_name.clone());
                        // LoadChat + MarkRead are now handled by the on_select callback
                        let mut rows = inner.rows.borrow_mut();
                        if let Some(row) = rows.get_mut(&chat_id) {
                            row.set_unread(0);
                        }
                    }
                });
        }

        ChatListPanel { inner }
    }

    pub fn widget(&self) -> &Box {
        &self.inner.root
    }

    pub fn connect_new_chat(&self, callback: impl Fn() + 'static) {
        self.inner.new_chat_btn.connect_clicked(move |_| callback());
    }

    pub fn load_chats(&self, chats: Vec<ChatSummary>) {
        let inner = &self.inner;

        // Build a set of incoming chat IDs for fast lookup.
        let incoming_ids: std::collections::HashSet<&str> =
            chats.iter().map(|c| c.id.as_str()).collect();

        self.remove_stale(&incoming_ids);

        // Add or update each incoming chat — reuses existing widgets.
        for chat in chats {
            self.add_chat(chat);
        }

        self.invalidate();
    }

    /// Remove rows whose IDs are not in the provided set.
    ///
    /// Chats whose IDs start with `gm:` (Google Messages) are skipped —
    /// they're managed by the gmessages runtime, which doesn't contribute
    /// to `keep_ids` so they'd be wrongly purged otherwise.
    pub fn remove_stale(&self, keep_ids: &std::collections::HashSet<&str>) {
        let inner = &self.inner;
        let stale_ids: Vec<String> = inner
            .rows
            .borrow()
            .keys()
            .filter(|id| !keep_ids.contains(id.as_str()) && !id.starts_with("gm:"))
            .cloned()
            .collect();
        for id in &stale_ids {
            if let Some(row) = inner.rows.borrow_mut().remove(id) {
                inner.list_box.remove(&row.gtk_row);
            }
            inner.timestamps.borrow_mut().remove(id);
        }
    }

    /// Re-sort and re-filter the list box.
    pub fn invalidate(&self) {
        self.inner.list_box.invalidate_sort();
        self.inner.list_box.invalidate_filter();
    }

    pub fn add_chat(&self, chat: ChatSummary) {
        // If the chat already exists, only update if the incoming data is NEWER.
        // During sync, old batches arrive after newer ones — never overwrite
        // a recent preview with an older one.
        {
            let rows = self.inner.rows.borrow();
            if let Some(row) = rows.get(&chat.id) {
                // Always update flags (mute, pin, etc.)
                row.is_pinned.set(chat.is_pinned);
                row.is_muted.set(chat.is_muted);
                row.is_archived.set(chat.is_archived);
                row.is_favorite.set(chat.is_favorite);
                row.pin_indicator.set_visible(chat.is_pinned);
                row.mute_indicator.set_visible(chat.is_muted);
                row.auto_mark_read.set(chat.auto_mark_read);
                row.auto_mr_indicator.set_visible(chat.auto_mark_read);
                // Trust the server's unread count directly. The server knows
                // what's been read on any device. If this wipes a real-time
                // increment from a MessageReceived that fired during sync,
                // the next MessageReceived or server refresh will correct it.
                // (Using max() here was wrong — it traps stale counts from
                // disk when the user reads on phone before reopening desktop.)
                row.set_unread(chat.unread_count);

                // Update timestamp INDEPENDENTLY of preview. The two used
                // to be coupled by an && — but the server sometimes hands
                // us a chat with a fresh timestamp and an empty
                // last_message (e.g. outgoing RCS where display_content
                // isn't filled). If we skipped both updates in that case,
                // the chat stayed at its old sort position even though a
                // newer message had arrived. Now timestamp moves the row
                // up; preview only updates when there's actually text.
                let existing_ts = self
                    .inner
                    .timestamps
                    .borrow()
                    .get(&chat.id)
                    .copied()
                    .unwrap_or(0);
                let mut moved = false;
                if chat.timestamp > existing_ts {
                    if !chat.last_message.is_empty() {
                        row.update_preview(&chat.last_message, chat.timestamp);
                    } else {
                        // Just bump the timestamp displayed on the row.
                        row.update_preview_timestamp(chat.timestamp);
                    }
                    moved = true;
                }
                drop(rows);
                if moved {
                    self.inner
                        .timestamps
                        .borrow_mut()
                        .insert(chat.id.clone(), chat.timestamp);
                    self.inner.list_box.invalidate_sort();
                }
                self.inner.list_box.invalidate_filter();
                return;
            }
        }
        self.add_chat_row(chat);
    }

    pub fn update_last_message(
        &self,
        chat_id: &str,
        msg: &IncomingMessage,
        current_chat_id: Option<&str>,
    ) {
        // If this chat doesn't exist yet, create it on the fly
        if !self.inner.rows.borrow().contains_key(chat_id) {
            let preview = msg
                .text
                .as_deref()
                .or(msg.media_caption.as_deref())
                .unwrap_or("")
                .to_string();
            // Name resolution priority for a brand-new chat row:
            //   1. msg.sender_name if it looks like a real name (alphabetic
            //      and not a numeric internal ID like "6")
            //   2. Cross-protocol global directory lookup by chat_id
            //      (covers gm: chats whose phone is in WhatsApp contacts)
            //   3. display_name_from_jid (WhatsApp JID → contact name)
            //   4. msg.sender_name as a last resort (better than nothing)
            let sender_looks_real = !msg.sender_name.is_empty()
                && !msg.is_from_me
                && msg.sender_name.chars().any(|c| c.is_alphabetic())
                && !msg
                    .sender_name
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '+' || c == ' ' || c == '(' || c == ')' || c == '-');
            let name = if sender_looks_real {
                msg.sender_name.clone()
            } else if let Some(n) = crate::contacts::global().lookup(chat_id) {
                n
            } else {
                let from_jid = crate::ui::runtime::display_name_from_jid(chat_id);
                if from_jid.is_empty() || from_jid == chat_id {
                    if msg.sender_name.is_empty() {
                        chat_id.to_string()
                    } else {
                        msg.sender_name.clone()
                    }
                } else {
                    from_jid
                }
            };
            self.add_chat(crate::bridge::ChatSummary {
                id: chat_id.to_string(),
                name,
                last_message: preview,
                timestamp: msg.timestamp,
                unread_count: if msg.is_from_me { 0 } else { 1 },
                is_group: chat_id.ends_with("@g.us"),
                is_muted: false,
                is_pinned: false,
                is_archived: false,
                is_favorite: false,
                label: None,
                pinned_msg_id: None,
                auto_mark_read: false,
            });
        }
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            let doc_preview: String;
            let content = msg
                .text
                .as_deref()
                .or(msg.media_caption.as_deref())
                .unwrap_or(match &msg.media_type {
                    Some(crate::bridge::MediaType::Image) => "📷 Photo",
                    Some(crate::bridge::MediaType::Video) => "🎥 Video",
                    Some(crate::bridge::MediaType::Audio) => "🎵 Audio",
                    Some(crate::bridge::MediaType::Document) => {
                        let fname = msg.media_filename.as_deref().unwrap_or("Document");
                        let icon = crate::ui::message_bubble::file_type_icon(fname);
                        let ext = fname.rsplit('.').next().unwrap_or("").to_uppercase();
                        doc_preview = if ext.is_empty() || ext == fname.to_uppercase() {
                            format!("{icon} Document")
                        } else {
                            format!("{icon} {ext} File")
                        };
                        &doc_preview
                    }
                    Some(crate::bridge::MediaType::Sticker) => "🎭 Sticker",
                    Some(crate::bridge::MediaType::Gif) => "🎞 GIF",
                    None => "",
                });
            // Strip any remaining @JID mentions from preview
            let clean = if content.contains('@') {
                let mut c = content.to_string();
                for word in content.split_whitespace() {
                    if word.starts_with('@')
                        && word.len() > 4
                        && word[1..]
                            .chars()
                            .next()
                            .map(|ch| ch.is_ascii_digit())
                            .unwrap_or(false)
                    {
                        c = c.replace(word, "@user");
                    }
                }
                c
            } else {
                content.to_string()
            };
            let preview = if row.is_group && !msg.is_from_me && !msg.sender_name.is_empty() {
                format!("{}: {clean}", msg.sender_name)
            } else if row.is_group && msg.is_from_me {
                format!("You: {clean}")
            } else {
                clean
            };
            row.update_preview(&preview, msg.timestamp);
            // Cache message for stealth peek (keep last 10)
            {
                let mut cache = self.inner.recent_messages.borrow_mut();
                let entry = cache.entry(chat_id.to_string()).or_default();
                // Dedup by message id
                if !entry.iter().any(|m| m.id == msg.id) {
                    entry.push(msg.clone());
                    if entry.len() > 10 {
                        entry.remove(0);
                    }
                }
            }
            // Only increment unread if NOT from us AND NOT the chat we're currently viewing
            let is_viewing = current_chat_id == Some(chat_id);
            if !msg.is_from_me && !is_viewing {
                let new_count = row.unread_count.get() + 1;
                row.set_unread(new_count);
            }
        }
        drop(rows);
        // Clear typing indicator state — a real message supersedes it
        self.inner.typing_previews.borrow_mut().remove(chat_id);
        self.inner.active_typers.borrow_mut().remove(chat_id);
        // Reset dots visibility
        {
            let rows = self.inner.rows.borrow();
            if let Some(row) = rows.get(chat_id) {
                row.preview_label.set_visible(true);
                row.typing_box.set_visible(false);
            }
        }
        self.inner
            .timestamps
            .borrow_mut()
            .insert(chat_id.to_string(), msg.timestamp);
        self.inner.list_box.invalidate_sort();
        self.inner.list_box.invalidate_filter();
    }

    pub fn has_chat(&self, chat_id: &str) -> bool {
        self.inner.rows.borrow().contains_key(chat_id)
    }

    /// Remove any @lid chat row whose name matches the given name.
    /// Called when a phone JID version of the same contact arrives.
    pub fn remove_lid_duplicate(&self, name: &str) {
        let lid_ids: Vec<String> = self
            .inner
            .rows
            .borrow()
            .iter()
            .filter(|(id, row)| id.ends_with("@lid") && row.chat_name == name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in lid_ids {
            self.remove_chat(&id);
        }
    }

    /// Extract phone number from JID (strips :device and @domain).
    fn phone_from_jid(jid: &str) -> String {
        // Google Messages chat ids are `gm:<conversation_id>` — the
        // colon separates the source-tag from the gmessages internal
        // ID, NOT a device suffix on a phone number. If we naively
        // split on `:` like we do for WhatsApp JIDs, every gm chat
        // reduces to "gm" and they all dedup against each other,
        // collapsing the entire gmessages portion of the chat list
        // down to the last one inserted. Keep the full id in that case.
        if jid.starts_with("gm:") {
            return jid.to_string();
        }
        let local = jid.split('@').next().unwrap_or(jid);
        local.split(':').next().unwrap_or(local).to_string()
    }

    pub fn chat_name(&self, chat_id: &str) -> Option<String> {
        self.inner
            .rows
            .borrow()
            .get(chat_id)
            .map(|r| r.chat_name.clone())
    }

    /// Force a chat to the top of the list by updating its timestamp to now.
    pub fn bump_chat_to_top(&self, chat_id: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.inner
            .timestamps
            .borrow_mut()
            .insert(chat_id.to_string(), now);
        self.inner.list_box.invalidate_sort();
    }

    pub fn reset_unread(&self, chat_id: &str) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.set_unread(0);
        }
    }

    pub fn set_typing(&self, chat_id: &str, sender_name: &str, is_typing: bool) {
        let rows = self.inner.rows.borrow();
        let Some(row) = rows.get(chat_id) else { return };
        // Hide raw JIDs (unresolved LIDs, numeric user parts) as "Someone"
        let looks_like_jid = sender_name.contains('@')
            || (sender_name.len() > 8 && sender_name.chars().all(|c| c.is_ascii_digit()));
        let display = if sender_name.is_empty() || looks_like_jid {
            "Someone".to_string()
        } else {
            sender_name.to_string()
        };

        let mut typers = self.inner.active_typers.borrow_mut();
        let chat_typers = typers.entry(chat_id.to_string()).or_default();
        if is_typing {
            if !chat_typers.contains(&display) {
                chat_typers.push(display.clone());
            }
        } else {
            chat_typers.retain(|t| *t != display);
        }

        if chat_typers.is_empty() {
            // All stopped — show preview, hide dots
            typers.remove(chat_id);
            row.preview_label.set_visible(true);
            row.typing_box.set_visible(false);
        } else {
            // Show typing dots, hide preview text
            let label = chat_typers.join(", ");
            // Update the name label inside the typing_box (first child)
            if let Some(first) = row.typing_box.first_child() {
                if let Some(name_lbl) = first.downcast_ref::<Label>() {
                    name_lbl.set_markup(
                        &format!("<span foreground='#00a884'>{label} </span>")
                    );
                }
            }
            row.preview_label.set_visible(false);
            row.typing_box.set_visible(true);
        }

        // Auto-expire after 15s (WhatsApp re-sends Composing every ~10s)
        if is_typing {
            let inner_w = Rc::downgrade(&self.inner);
            let cid = chat_id.to_string();
            let name = display;
            gtk4::glib::timeout_add_local_once(
                std::time::Duration::from_secs(15),
                move || {
                    let Some(inner) = inner_w.upgrade() else { return };
                    let mut typers = inner.active_typers.borrow_mut();
                    if let Some(chat_typers) = typers.get_mut(&cid) {
                        chat_typers.retain(|t| *t != name);
                    }
                    let empty = typers.get(&cid).map(|t| t.is_empty()).unwrap_or(true);
                    if empty {
                        typers.remove(&cid);
                    }
                    drop(typers);
                    let rows = inner.rows.borrow();
                    if let Some(row) = rows.get(&cid) {
                        if empty {
                            row.preview_label.set_visible(true);
                            row.typing_box.set_visible(false);
                        } else {
                            let all = inner.active_typers.borrow();
                            if let Some(remaining) = all.get(&cid) {
                                let label = remaining.join(", ");
                                if let Some(first) = row.typing_box.first_child() {
                                    if let Some(name_lbl) = first.downcast_ref::<Label>() {
                                        name_lbl.set_markup(
                                            &format!("<span foreground='#00a884'>{label} </span>")
                                        );
                                    }
                                }
                            }
                        }
                    }
                },
            );
        }
    }

    pub fn update_chat_name(&self, chat_id: &str, name: &str) {
        self.update_chat_name_inner(chat_id, name, false);
    }

    /// Like [`update_chat_name`] but bypasses the downgrade-refusal
    /// heuristic. Use this for AUTHORITATIVE sources (gm conversation
    /// refresh, contact-list sync, user-typed rename) where the new name
    /// is known good — even if it's shorter / fewer words than the
    /// previous one. Without this, a chat that got mis-titled with the
    /// user's own name (e.g. "Jake Steinman") from a sender lookup on
    /// the user's outgoing SMS could never be retitled to the actual
    /// contact name ("Clayton") because the heuristic considers the
    /// shorter name a "downgrade".
    pub fn update_chat_name_authoritative(&self, chat_id: &str, name: &str) {
        self.update_chat_name_inner(chat_id, name, true);
    }

    fn update_chat_name_inner(&self, chat_id: &str, name: &str, authoritative: bool) {
        let mut rows = self.inner.rows.borrow_mut();
        if let Some(row) = rows.get_mut(chat_id) {
            let old = row.chat_name.clone();
            if old == name {
                return;
            }
            // Don't overwrite a fully-alphabetic, multi-word name with a
            // shorter or numeric one. This guards against typing-event /
            // sender_participant updates stomping a properly-resolved
            // contact name (the "Craig Thompson → Jake Steinman" class
            // of bug). The ContactDirectory has the same length-aware
            // ranking; chat-list rows benefit from the same protection.
            // Authoritative callers skip this check.
            if !authoritative {
                let old_words = old.split_whitespace().count();
                let new_words = name.split_whitespace().count();
                let old_alpha = old.chars().any(|c| c.is_alphabetic());
                let new_alpha = name.chars().any(|c| c.is_alphabetic());
                let is_downgrade = match (old_alpha, new_alpha) {
                    (true, false) => true,                // alpha → numeric: never
                    (true, true) => new_words < old_words // multi-word → fewer words
                        || (new_words == old_words && name.len() < old.len()),
                    _ => false,
                };
                if is_downgrade {
                    log::info!(
                        "update_chat_name: {chat_id}: REFUSED downgrade {old:?} → {name:?}"
                    );
                    return;
                }
            }
            log::info!(
                "update_chat_name{auth}: {chat_id}: {old:?} → {name:?}",
                auth = if authoritative { "(auth)" } else { "" }
            );
            row.chat_name = name.to_string();
            row.name_label.set_text(name);
        }
    }

    pub fn set_avatar(&self, chat_id: &str, path: &str) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            if let Some(texture) = crate::ui::texture_cache::texture_from_filename(path) {
                row.avatar.set_custom_image(Some(&texture));
            }
        }
    }

    // ── Context-menu event handlers ───────────────────────────────────────────

    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.is_archived.set(archived);
        }
        drop(rows);
        self.inner.list_box.invalidate_filter();
    }

    pub fn set_chat_muted(&self, chat_id: &str, muted: bool) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.is_muted.set(muted);
            row.mute_indicator.set_visible(muted);
        }
    }

    /// Query whether the given chat is configured to auto-mark-read on incoming messages.
    pub fn is_auto_mark_read(&self, chat_id: &str) -> bool {
        self.inner
            .rows
            .borrow()
            .get(chat_id)
            .map(|r| r.auto_mark_read.get())
            .unwrap_or(false)
    }

    /// Toggle the auto-mark-read flag for a chat. Caller is responsible for
    /// sending the WaCommand::SetAutoMarkRead to persist server-side.
    pub fn set_auto_mark_read(&self, chat_id: &str, enabled: bool) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.auto_mark_read.set(enabled);
            row.auto_mr_indicator.set_visible(enabled);
        }
    }

    pub fn set_chat_pinned(&self, chat_id: &str, pinned: bool) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.is_pinned.set(pinned);
            row.pin_indicator.set_visible(pinned);
        }
        drop(rows);
        self.inner.list_box.invalidate_sort();
    }

    pub fn mark_chat_unread(&self, chat_id: &str) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            if row.unread_count.get() == 0 {
                row.set_unread(1);
            }
        }
        drop(rows);
        self.inner.list_box.invalidate_filter();
    }

    pub fn set_chat_favorite(&self, chat_id: &str, favorite: bool) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.is_favorite.set(favorite);
        }
        drop(rows);
        self.inner.list_box.invalidate_filter();
    }

    pub fn remove_chat(&self, chat_id: &str) {
        let row = self.inner.rows.borrow_mut().remove(chat_id);
        if let Some(row) = row {
            self.inner.list_box.remove(&row.gtk_row);
        }
        self.inner.timestamps.borrow_mut().remove(chat_id);
    }

    /// Update chat list preview after a message was deleted.
    /// Shows "🚫 This message was deleted" as the preview.
    pub fn update_preview_text(&self, chat_id: &str, text: &str) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.preview_label.set_text(text);
        }
    }

    pub fn set_preview_to_previous(&self, chat_id: &str) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.preview_label.set_text("🚫 Message deleted");
        }
    }

    pub fn clear_chat_messages(&self, chat_id: &str) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            row.update_preview("", 0);
        }
    }

    pub fn set_chat_label(&self, chat_id: &str, label: Option<&str>) {
        let rows = self.inner.rows.borrow();
        if let Some(row) = rows.get(chat_id) {
            match label {
                Some(l) => {
                    row.label_badge.set_text(l);
                    row.label_badge.set_visible(true);
                }
                None => {
                    row.label_badge.set_visible(false);
                }
            }
        }
    }

    fn add_chat_row(&self, chat: ChatSummary) {
        let inner = &self.inner;

        // ── Phone-number dedup ──
        // Same phone number = same person, regardless of @lid, :device, etc.
        if !chat.id.ends_with("@g.us") {
            let new_phone = Self::phone_from_jid(&chat.id);
            let dup_id = inner
                .rows
                .borrow()
                .keys()
                .find(|existing_id| {
                    !existing_id.ends_with("@g.us")
                        && Self::phone_from_jid(existing_id) == new_phone
                        && *existing_id != &chat.id
                })
                .cloned();
            if let Some(dup) = dup_id {
                log::info!(
                    "Dedup: {} ({}) — removing existing {} (same phone {})",
                    chat.name, chat.id, dup, new_phone
                );
                self.remove_chat(&dup);
            }
        }

        let row = ChatRow::new(&chat);
        row.gtk_row.set_widget_name(&chat.id);

        // Attach right-click context menu
        attach_context_menu(&row, inner, chat.id.clone());

        // Attach hover-to-stealth-read popup (shows unread messages without marking read)
        attach_stealth_hover(&row, chat.id.clone(), self.inner.recent_messages.clone());

        inner.list_box.append(&row.gtk_row);
        inner
            .timestamps
            .borrow_mut()
            .insert(chat.id.clone(), chat.timestamp);
        inner.rows.borrow_mut().insert(chat.id, row);
        inner.list_box.invalidate_sort();
    }
}

// ── Stealth read hover popup ─────────────────────────────────────────────────

/// Shows a popover with recent unread messages when the user hovers over a
/// chat row for ≥600ms. Does NOT mark messages as read — pure stealth peek.
fn attach_stealth_hover(row: &ChatRow, chat_id: String, recent_cache: Rc<RefCell<HashMap<String, Vec<IncomingMessage>>>>) {
    let hover = gtk4::EventControllerMotion::new();
    let row_widget = row.gtk_row.clone();
    let unread_count = row.unread_count.clone();

    // Timer handle so we can cancel on leave.
    // Wrapped in Rc<RefCell> so both the enter callback and the timer
    // callback itself can clear it (preventing stale remove on leave).
    let timer_id: Rc<RefCell<Option<gtk4::glib::SourceId>>> = Rc::new(RefCell::new(None));
    let popover_ref: Rc<RefCell<Option<Popover>>> = Rc::new(RefCell::new(None));

    // Hover enter — start 600ms timer
    let timer_clone = timer_id.clone();
    let pop_clone = popover_ref.clone();
    let cid = chat_id.clone();
    let rw = row_widget.clone();
    let uc = unread_count.clone();
    hover.connect_enter(move |_, _, _| {
        if uc.get() == 0 {
            return;
        }
        if pop_clone.borrow().is_some() {
            return;
        }
        let cid2 = cid.clone();
        let rw2 = rw.clone();
        let pop2 = pop_clone.clone();
        let timer_clear = timer_clone.clone();
        let cache = recent_cache.clone();
        let id =
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                // Clear the timer ID so leave doesn't try to remove a fired source
                *timer_clear.borrow_mut() = None;
                let popover = build_stealth_popover(&rw2, &cid2, &cache);
                popover.popup();
                *pop2.borrow_mut() = Some(popover);
            });
        *timer_clone.borrow_mut() = Some(id);
    });

    // Hover leave — cancel timer if it hasn't fired, dismiss popover
    let timer_clone2 = timer_id.clone();
    let pop_clone2 = popover_ref.clone();
    hover.connect_leave(move |_| {
        if let Some(id) = timer_clone2.borrow_mut().take() {
            id.remove();
        }
        if let Some(pop) = pop_clone2.borrow_mut().take() {
            pop.popdown();
            pop.unparent();
        }
    });

    row_widget.add_controller(hover);
}

/// Build the stealth read popover — loads messages from disk cache,
/// shows the last N unread ones without sending read receipts.
fn build_stealth_popover(parent: &ListBoxRow, chat_id: &str, recent_cache: &Rc<RefCell<HashMap<String, Vec<IncomingMessage>>>>) -> Popover {
    let popover = Popover::new();
    popover.set_parent(parent);
    popover.add_css_class("stealth-popover");
    popover.set_has_arrow(true);
    popover.set_autohide(false); // we control dismiss on hover leave

    let vbox = Box::new(Orientation::Vertical, 0);

    let header = Label::new(Some("👁 Stealth peek"));
    header.add_css_class("stealth-header");
    header.set_halign(Align::Start);
    vbox.append(&header);

    // Load messages: merge disk cache with in-memory recent messages.
    // In-memory cache has the latest messages that may not be flushed to disk yet.
    let mut messages = crate::ui::runtime::load_messages(chat_id);
    if let Some(recent) = recent_cache.borrow().get(chat_id) {
        for rm in recent {
            if !messages.iter().any(|m| m.id == rm.id) {
                messages.push(rm.clone());
            }
        }
        messages.sort_by_key(|m| m.timestamp);
    }

    if messages.is_empty() {
        let empty = Label::new(Some("No cached messages"));
        empty.add_css_class("stealth-empty");
        vbox.append(&empty);
    } else {
        // Show last 5 messages — priority on most recent (unread) at bottom
        let start = messages.len().saturating_sub(5);
        let scroll = ScrolledWindow::new();
        scroll.set_max_content_height(300);
        scroll.set_propagate_natural_height(true);
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        // Auto-scroll to bottom so the latest message is always visible
        let sc = scroll.clone();
        scroll.connect_map(move |_| {
            let adj = sc.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });

        let msg_box = Box::new(Orientation::Vertical, 0);
        for msg in &messages[start..] {
            let row = Box::new(Orientation::Vertical, 2);
            row.add_css_class("stealth-message");

            // Sender + time header
            let top = Box::new(Orientation::Horizontal, 6);
            let sender_name = if msg.is_from_me {
                "You".to_string()
            } else if !msg.sender_name.is_empty() {
                msg.sender_name.clone()
            } else {
                msg.sender_id.clone()
            };
            let sender = Label::new(Some(&sender_name));
            sender.add_css_class("stealth-sender");
            sender.set_halign(Align::Start);
            top.append(&sender);

            let ts = chrono::DateTime::from_timestamp(msg.timestamp, 0)
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_default();
            let time = Label::new(Some(&ts));
            time.add_css_class("stealth-time");
            time.set_halign(Align::End);
            time.set_hexpand(true);
            top.append(&time);
            row.append(&top);

            // Message text
            let doc_preview2: String;
            let text = msg
                .text
                .as_deref()
                .or(msg.media_caption.as_deref())
                .unwrap_or(match &msg.media_type {
                    Some(crate::bridge::MediaType::Image) => "📷 Photo",
                    Some(crate::bridge::MediaType::Video) => "🎥 Video",
                    Some(crate::bridge::MediaType::Audio) => "🎵 Audio",
                    Some(crate::bridge::MediaType::Document) => {
                        let fname = msg.media_filename.as_deref().unwrap_or("Document");
                        let icon = crate::ui::message_bubble::file_type_icon(fname);
                        let ext = fname.rsplit('.').next().unwrap_or("").to_uppercase();
                        doc_preview2 = if ext.is_empty() || ext == fname.to_uppercase() {
                            format!("{icon} Document")
                        } else {
                            format!("{icon} {ext} File")
                        };
                        &doc_preview2
                    }
                    Some(crate::bridge::MediaType::Sticker) => "✨ Sticker",
                    Some(crate::bridge::MediaType::Gif) => "🎞 GIF",
                    None => "",
                });
            if !text.is_empty() {
                let label = Label::new(Some(text));
                label.add_css_class("stealth-text");
                label.set_halign(Align::Start);
                label.set_wrap(true);
                label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
                label.set_max_width_chars(45);
                row.append(&label);
            }

            msg_box.append(&row);
        }
        scroll.set_child(Some(&msg_box));
        vbox.append(&scroll);
    }

    popover.set_child(Some(&vbox));
    popover
}

// ── Context menu ──────────────────────────────────────────────────────────────

fn attach_context_menu(row: &ChatRow, inner: &Rc<ChatListInner>, chat_id: String) {
    let gesture = GestureClick::new();
    gesture.set_button(3);

    let is_archived = row.is_archived.clone();
    let is_muted = row.is_muted.clone();
    let is_pinned = row.is_pinned.clone();
    let is_favorite = row.is_favorite.clone();
    let auto_mark_read = row.auto_mark_read.clone();
    let auto_mr_indicator = row.auto_mr_indicator.clone();
    let row_widget = row.gtk_row.clone();
    let bridge = inner.bridge.clone();

    gesture.connect_pressed(move |_, _, x, y| {
        show_context_menu(
            &row_widget,
            bridge.clone(),
            chat_id.clone(),
            is_archived.clone(),
            is_muted.clone(),
            is_pinned.clone(),
            is_favorite.clone(),
            auto_mark_read.clone(),
            auto_mr_indicator.clone(),
            x,
            y,
        );
    });

    row.gtk_row.add_controller(gesture);
}

fn show_context_menu(
    row_widget: &ListBoxRow,
    bridge: Arc<Bridge>,
    chat_id: String,
    is_archived: Rc<Cell<bool>>,
    is_muted: Rc<Cell<bool>>,
    is_pinned: Rc<Cell<bool>>,
    is_favorite: Rc<Cell<bool>>,
    auto_mark_read: Rc<Cell<bool>>,
    auto_mr_indicator: Label,
    x: f64,
    y: f64,
) {
    let popover = Popover::new();
    popover.set_parent(row_widget);
    popover.add_css_class("menu");
    popover.set_has_arrow(false);
    let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));

    let vbox = Box::new(Orientation::Vertical, 0);
    vbox.set_width_request(230);

    macro_rules! menu_item {
        ($label:expr, $class:expr) => {{
            let btn = Button::with_label($label);
            btn.set_has_frame(false);
            btn.add_css_class("flat");
            if !$class.is_empty() {
                btn.add_css_class($class);
            }
            // left-align the label inside the button
            if let Some(child) = btn.child() {
                if let Ok(lbl) = child.downcast::<Label>() {
                    lbl.set_halign(Align::Start);
                    lbl.set_hexpand(true);
                    lbl.set_margin_start(4);
                }
            }
            btn
        }};
    }

    // 1. Archive / Unarchive
    let archive_text = if is_archived.get() {
        "Unarchive chat"
    } else {
        "Archive chat"
    };
    let btn_archive = menu_item!(archive_text, "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let is_archived = is_archived.clone();
        let popover = popover.clone();
        btn_archive.connect_clicked(move |_| {
            let new_val = !is_archived.get();
            bridge.send_command(WaCommand::ArchiveChat {
                chat_id: chat_id.clone(),
                archived: new_val,
            });
            popover.popdown();
        });
    }

    // 2. Mute / Unmute
    let mute_text = if is_muted.get() {
        "Unmute notifications"
    } else {
        "Mute notifications"
    };
    let btn_mute = menu_item!(mute_text, "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let is_muted = is_muted.clone();
        let popover = popover.clone();
        btn_mute.connect_clicked(move |_| {
            let new_val = !is_muted.get();
            bridge.send_command(WaCommand::MuteChat {
                chat_id: chat_id.clone(),
                muted: new_val,
            });
            popover.popdown();
        });
    }

    // 3. Pin / Unpin
    let pin_text = if is_pinned.get() {
        "Unpin chat"
    } else {
        "Pin chat"
    };
    let btn_pin = menu_item!(pin_text, "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let is_pinned = is_pinned.clone();
        let popover = popover.clone();
        btn_pin.connect_clicked(move |_| {
            let new_val = !is_pinned.get();
            bridge.send_command(WaCommand::PinChat {
                chat_id: chat_id.clone(),
                pinned: new_val,
            });
            popover.popdown();
        });
    }

    // 4. Label chat
    let btn_label = menu_item!("Label chat", "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let popover = popover.clone();
        let row_widget = row_widget.clone();
        btn_label.connect_clicked(move |_| {
            popover.popdown();
            show_label_dialog(&row_widget, bridge.clone(), chat_id.clone());
        });
    }

    // Auto-mark-read toggle — useful for noisy groups
    let auto_mr_text = if auto_mark_read.get() {
        "Disable auto-mark read"
    } else {
        "Enable auto-mark read"
    };
    let btn_auto_mr = menu_item!(auto_mr_text, "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let popover = popover.clone();
        let auto_mr = auto_mark_read.clone();
        let indicator = auto_mr_indicator.clone();
        btn_auto_mr.connect_clicked(move |_| {
            let new_val = !auto_mr.get();
            auto_mr.set(new_val);
            // Toggle the 👁 indicator immediately — the command updates
            // backing state but doesn't bounce back to this row.
            indicator.set_visible(new_val);
            bridge.send_command(WaCommand::SetAutoMarkRead {
                chat_id: chat_id.clone(),
                enabled: new_val,
            });
            popover.popdown();
        });
    }

    vbox.append(&btn_archive);
    vbox.append(&btn_mute);
    vbox.append(&btn_pin);
    vbox.append(&btn_label);
    vbox.append(&btn_auto_mr);
    vbox.append(&Separator::new(Orientation::Horizontal));

    // 5. Mark as unread
    let btn_unread = menu_item!("Mark as unread", "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let popover = popover.clone();
        btn_unread.connect_clicked(move |_| {
            bridge.send_command(WaCommand::MarkUnread {
                chat_id: chat_id.clone(),
            });
            popover.popdown();
        });
    }

    // 6. Add to / Remove from Favourites
    let fav_text = if is_favorite.get() {
        "Remove from Favourites"
    } else {
        "Add to Favourites"
    };
    let btn_fav = menu_item!(fav_text, "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let is_favorite = is_favorite.clone();
        let popover = popover.clone();
        btn_fav.connect_clicked(move |_| {
            let new_val = !is_favorite.get();
            bridge.send_command(WaCommand::FavoriteChat {
                chat_id: chat_id.clone(),
                favorite: new_val,
            });
            popover.popdown();
        });
    }

    vbox.append(&btn_unread);
    vbox.append(&btn_fav);
    vbox.append(&Separator::new(Orientation::Horizontal));

    // 7. Block
    let btn_block = menu_item!("Block", "");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let popover = popover.clone();
        let row_widget = row_widget.clone();
        btn_block.connect_clicked(move |_| {
            popover.popdown();
            show_confirm_dialog(
                &row_widget,
                "Block contact?",
                "They won't be able to send you messages.",
                "Block",
                true,
                {
                    let bridge = bridge.clone();
                    let chat_id = chat_id.clone();
                    move || {
                        bridge.send_command(WaCommand::BlockContact {
                            chat_id: chat_id.clone(),
                        })
                    }
                },
            );
        });
    }

    // 8. Clear chat
    let btn_clear = menu_item!("Clear chat", "destructive-action");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let popover = popover.clone();
        let row_widget = row_widget.clone();
        btn_clear.connect_clicked(move |_| {
            popover.popdown();
            show_confirm_dialog(
                &row_widget,
                "Clear chat?",
                "All messages will be permanently deleted from this device.",
                "Clear",
                true,
                {
                    let bridge = bridge.clone();
                    let chat_id = chat_id.clone();
                    move || {
                        bridge.send_command(WaCommand::ClearChat {
                            chat_id: chat_id.clone(),
                        })
                    }
                },
            );
        });
    }

    // 9. Delete chat
    let btn_delete = menu_item!("Delete chat", "destructive-action");
    {
        let bridge = bridge.clone();
        let chat_id = chat_id.clone();
        let popover = popover.clone();
        let row_widget = row_widget.clone();
        btn_delete.connect_clicked(move |_| {
            popover.popdown();
            show_confirm_dialog(
                &row_widget,
                "Delete chat?",
                "This chat and all its messages will be permanently deleted.",
                "Delete",
                true,
                {
                    let bridge = bridge.clone();
                    let chat_id = chat_id.clone();
                    move || {
                        bridge.send_command(WaCommand::DeleteChat {
                            chat_id: chat_id.clone(),
                        })
                    }
                },
            );
        });
    }

    vbox.append(&btn_block);
    vbox.append(&btn_clear);
    vbox.append(&btn_delete);

    popover.set_child(Some(&vbox));
    popover.popup();
}

fn show_label_dialog(parent_widget: &ListBoxRow, bridge: Arc<Bridge>, chat_id: String) {
    let dialog = adw::AlertDialog::builder()
        .heading("Label chat")
        .body("Choose or type a label for this chat.")
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("apply", "Apply");
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");

    // Preset label buttons + custom entry
    let vbox = Box::new(Orientation::Vertical, 8);
    vbox.set_margin_top(8);

    let presets_box = Box::new(Orientation::Horizontal, 6);
    presets_box.set_halign(Align::Center);
    for preset in ["Work", "Friends", "Family", "Personal"] {
        let b = Button::with_label(preset);
        b.add_css_class("pill");
        presets_box.append(&b);
    }

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Custom label…"));

    // Clicking a preset fills the entry
    for (i, preset) in ["Work", "Friends", "Family", "Personal"].iter().enumerate() {
        if let Some(child) = presets_box.observe_children().item(i as u32) {
            if let Ok(btn) = child.downcast::<Button>() {
                let entry_clone = entry.clone();
                let preset_str = preset.to_string();
                btn.connect_clicked(move |_| entry_clone.set_text(&preset_str));
            }
        }
    }

    vbox.append(&presets_box);
    vbox.append(&entry);
    dialog.set_extra_child(Some(&vbox));

    let parent_window = parent_widget
        .root()
        .and_then(|r| r.downcast::<gtk4::Window>().ok());
    dialog.present(parent_window.as_ref());

    dialog.connect_response(None, move |_, response| {
        if response == "apply" {
            let text = entry.text().to_string();
            let label = if text.is_empty() { None } else { Some(text) };
            bridge.send_command(WaCommand::LabelChat {
                chat_id: chat_id.clone(),
                label,
            });
        }
    });
}

fn show_confirm_dialog(
    parent_widget: &ListBoxRow,
    heading: &str,
    body: &str,
    confirm_label: &str,
    destructive: bool,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", confirm_label);
    if destructive {
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    }
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let parent_window = parent_widget
        .root()
        .and_then(|r| r.downcast::<gtk4::Window>().ok());
    dialog.present(parent_window.as_ref());

    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_filter_chip(label: &str) -> ToggleButton {
    let btn = ToggleButton::with_label(label);
    btn.add_css_class("filter-chip");
    btn.add_css_class("flat"); // Remove default GTK button chrome (kills 1px ghost border)
    // Use widget-level margins instead of Box spacing to avoid 1px rendering artifacts
    btn.set_margin_start(3);
    btn.set_margin_end(3);
    btn
}

fn wire_filter_chip(btn: &ToggleButton, filter: ChatFilter, inner: &Rc<ChatListInner>) {
    let weak = Rc::downgrade(inner);
    btn.connect_toggled(move |b| {
        if b.is_active() {
            if let Some(inner) = weak.upgrade() {
                *inner.active_filter.borrow_mut() = filter;
                inner.list_box.invalidate_filter();
            }
        }
    });
}

// ── ChatRow ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ChatRow {
    gtk_row: ListBoxRow,
    chat_name: String,
    is_group: bool,
    is_pinned: Rc<Cell<bool>>,
    is_archived: Rc<Cell<bool>>,
    is_muted: Rc<Cell<bool>>,
    is_favorite: Rc<Cell<bool>>,
    auto_mark_read: Rc<Cell<bool>>,
    unread_count: Rc<Cell<u32>>,
    name_label: Label,
    preview_label: Label,
    typing_box: Box,
    time_label: Label,
    unread_badge: Label,
    avatar: adw::Avatar,
    pin_indicator: Label,
    mute_indicator: Label,
    auto_mr_indicator: Label,
    label_badge: Label,
}

impl ChatRow {
    fn new(chat: &ChatSummary) -> Self {
        let gtk_row = ListBoxRow::new();
        gtk_row.set_size_request(-1, 92);
        gtk_row.set_overflow(gtk4::Overflow::Hidden);

        let hbox = Box::new(Orientation::Horizontal, 12);
        hbox.set_margin_top(10);
        hbox.set_margin_bottom(10);
        hbox.set_margin_start(14);
        hbox.set_margin_end(14);

        let avatar_text = chat.name.trim_start_matches('+');
        let avatar = adw::Avatar::new(52, Some(avatar_text), true);

        let vbox = Box::new(Orientation::Vertical, 2);
        vbox.set_hexpand(true);
        vbox.set_valign(Align::Center);

        // Top row: [pin] name [mute] time
        let top_row = Box::new(Orientation::Horizontal, 0);
        top_row.set_hexpand(true);

        let pin_indicator = Label::new(Some("📌"));
        pin_indicator.add_css_class("caption");
        pin_indicator.set_visible(chat.is_pinned);

        let name_label = Label::new(Some(&chat.name));
        name_label.set_halign(Align::Start);
        name_label.set_hexpand(true);
        name_label.add_css_class("heading");
        name_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        name_label.set_max_width_chars(22);

        let mute_indicator = Label::new(Some("🔕"));
        mute_indicator.add_css_class("caption");
        mute_indicator.set_visible(chat.is_muted);

        // 👁 = auto-mark-read is enabled for this chat
        let auto_mr_indicator = Label::new(Some("👁"));
        auto_mr_indicator.add_css_class("caption");
        auto_mr_indicator.add_css_class("dim-label");
        auto_mr_indicator.set_tooltip_text(Some("Auto-mark read on receive"));
        auto_mr_indicator.set_visible(chat.auto_mark_read);

        let time_label = Label::new(Some(&format_timestamp(chat.timestamp)));
        time_label.add_css_class("dim-label");
        time_label.add_css_class("caption");
        time_label.set_halign(Align::End);

        top_row.append(&pin_indicator);
        top_row.append(&name_label);
        top_row.append(&mute_indicator);
        top_row.append(&auto_mr_indicator);
        top_row.append(&time_label);

        // Bottom row: preview [label] unread
        let bottom_row = Box::new(Orientation::Horizontal, 4);
        bottom_row.set_hexpand(true);

        let preview_text = if chat.last_message.is_empty() {
            ""
        } else {
            &chat.last_message
        };
        let preview_label = Label::new(Some(preview_text));
        preview_label.set_halign(Align::Start);
        preview_label.set_hexpand(true);
        preview_label.add_css_class("dim-label");
        preview_label.add_css_class("body");
        preview_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        preview_label.set_max_width_chars(26);
        preview_label.set_lines(1);
        preview_label.set_single_line_mode(true);

        // Typing indicator (3 animated dots) — hidden by default
        let typing_box = Box::new(Orientation::Horizontal, 2);
        typing_box.set_halign(Align::Start);
        typing_box.set_hexpand(true);
        typing_box.set_visible(false);
        let typing_name_lbl = Label::new(None);
        typing_name_lbl.add_css_class("dim-label");
        typing_name_lbl.add_css_class("body");
        typing_box.append(&typing_name_lbl);
        for i in 1..=3 {
            let dot = Label::new(Some("●"));
            dot.add_css_class("typing-dot");
            dot.add_css_class(&format!("typing-dot-{i}"));
            typing_box.append(&dot);
        }

        let label_badge = Label::new(chat.label.as_deref());
        label_badge.add_css_class("caption");
        label_badge.add_css_class("accent");
        label_badge.set_visible(chat.label.is_some());
        label_badge.set_halign(Align::End);
        label_badge.set_valign(Align::Center);

        let unread_badge = Label::new(Some(&chat.unread_count.to_string()));
        unread_badge.add_css_class("unread-badge");
        unread_badge.set_visible(chat.unread_count > 0);
        unread_badge.set_halign(Align::End);
        unread_badge.set_valign(Align::Center);

        bottom_row.append(&preview_label);
        bottom_row.append(&typing_box);
        bottom_row.append(&label_badge);
        bottom_row.append(&unread_badge);

        vbox.append(&top_row);
        vbox.append(&bottom_row);
        hbox.append(&avatar);
        hbox.append(&vbox);
        gtk_row.set_child(Some(&hbox));

        ChatRow {
            gtk_row,
            chat_name: chat.name.clone(),
            is_group: chat.is_group,
            is_pinned: Rc::new(Cell::new(chat.is_pinned)),
            is_archived: Rc::new(Cell::new(chat.is_archived)),
            is_muted: Rc::new(Cell::new(chat.is_muted)),
            is_favorite: Rc::new(Cell::new(chat.is_favorite)),
            auto_mark_read: Rc::new(Cell::new(chat.auto_mark_read)),
            unread_count: Rc::new(Cell::new(chat.unread_count)),
            name_label,
            preview_label,
            typing_box,
            time_label,
            unread_badge,
            avatar,
            pin_indicator,
            mute_indicator,
            auto_mr_indicator,
            label_badge,
        }
    }

    fn update_preview(&self, text: &str, timestamp: i64) {
        self.preview_label.set_text(text);
        self.time_label.set_text(&format_timestamp(timestamp));
    }

    /// Update only the displayed timestamp, leaving preview text alone.
    /// Used when the server sends a fresh `last_message_timestamp` but
    /// no usable `display_content` (e.g. some outgoing RCS).
    fn update_preview_timestamp(&self, timestamp: i64) {
        self.time_label.set_text(&format_timestamp(timestamp));
    }

    fn set_unread(&self, count: u32) {
        self.unread_count.set(count);
        self.unread_badge.set_text(&count.to_string());
        self.unread_badge.set_visible(count > 0);
    }
}

fn format_timestamp(ts: i64) -> String {
    use chrono::{DateTime, Local, Utc};
    let dt: DateTime<Local> =
        DateTime::from(DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default());
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%-I:%M %p").to_string()
    } else if (now.date_naive() - dt.date_naive()).num_days() < 7 {
        dt.format("%a").to_string()
    } else {
        dt.format("%m/%d/%y").to_string()
    }
}

/// Search through locally cached messages for a query string.
/// Returns up to 50 results sorted by timestamp (newest first).
fn search_local_messages(query: &str) -> Vec<crate::bridge::SearchHit> {
    use crate::bridge::SearchHit;
    let mut hits: Vec<SearchHit> = Vec::new();
    let query_lower = query.to_lowercase();

    let cache_dir = std::path::PathBuf::from("wa_messages");
    if !cache_dir.exists() {
        return hits;
    }
    let entries = match std::fs::read_dir(&cache_dir) {
        Ok(e) => e,
        Err(_) => return hits,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Skip the 4-byte BIN_HEADER ("WA02") prepended by write_bin_path
        if data.len() < 4 {
            continue;
        }
        let messages: Vec<IncomingMessage> = match bincode::deserialize(&data[4..]) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Get the real chat_id from a message inside the file — the
        // filename is a file-safe form where '@' and ':' are replaced with
        // '_' (e.g. "120363...@g.us" → "120363..._g.us"), which isn't a
        // valid JID. Using the filename as chat_id broke search-click.
        let real_chat_id = messages
            .first()
            .map(|m| m.chat_id.clone())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string()
            });
        for msg in &messages {
            if let Some(text) = &msg.text {
                if text.to_lowercase().contains(&query_lower) {
                    hits.push(SearchHit {
                        chat_id: real_chat_id.clone(),
                        chat_name: String::new(),
                        msg_id: msg.id.clone(),
                        sender_name: msg.sender_name.clone(),
                        text: text.clone(),
                        timestamp: msg.timestamp,
                    });
                }
            }
        }
    }
    hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    hits.truncate(50);
    hits
}
