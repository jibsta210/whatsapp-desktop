use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, FlowBox, Frame, Label, ListBox, ListBoxRow, Orientation, Picture,
    ScrolledWindow, SearchEntry, Separator,
};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, ChatSummary, GroupMember, WaCommand};

#[derive(Clone)]
pub struct ProfilePanel {
    inner: Rc<ProfileInner>,
}

struct ProfileInner {
    root: Box,
    header_title: Label,
    avatar: adw::Avatar,
    name_label: Label,
    group_name_entry: gtk4::Entry,
    group_name_row: Box,
    subtitle_label: Label,
    about_label: Label,
    about_section: Box,
    // Media grid
    media_section: Box,
    media_grid: FlowBox,
    media_count_label: Label,
    links_list: ListBox,
    docs_list: ListBox,
    // Groups in common (contact profile)
    groups_section: Box,
    groups_list: ListBox,
    // Members (group profile)
    members_section: Box,
    members_search: SearchEntry,
    members_list: ListBox,
    members_add_btn: Button,
    leave_group_btn: Button,
    invite_section: Box,
    invite_label: Label,
    disappearing_section: Box,
    disappearing_dropdown: gtk4::DropDown,
    bridge: Arc<Bridge>,
    share_btn: Button,
    current_chat_id: RefCell<Option<String>>,
    /// Last group subject loaded into the entry, used to skip re-sending
    /// SetGroupSubject on focus-out when the text is unchanged.
    loaded_group_subject: RefCell<String>,
    /// Cached group profiles to avoid re-fetching on reopen
    cached_group_profiles: RefCell<
        std::collections::HashMap<String, (String, Option<String>, Vec<GroupMember>, bool)>,
    >,
    on_chat_selected: RefCell<Option<std::boxed::Box<dyn Fn(String, String)>>>,
}

impl ProfilePanel {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        let root = Box::new(Orientation::Vertical, 0);
        root.set_width_request(425);
        root.set_vexpand(true);

        // Header with close button
        let header = adw::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        header.set_show_start_title_buttons(false);
        let header_title = Label::new(Some("Profile"));
        header_title.add_css_class("title");
        header.set_title_widget(Some(&header_title));

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);

        let content = Box::new(Orientation::Vertical, 0);
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.set_margin_top(16);

        // ── Avatar ──
        let avatar = adw::Avatar::new(96, None, true);
        avatar.set_halign(Align::Center);
        avatar.set_margin_bottom(12);

        let name_label = Label::new(Some(""));
        name_label.add_css_class("title-1");
        name_label.set_halign(Align::Center);
        name_label.set_margin_bottom(4);
        name_label.set_wrap(true);

        // Editable group name row (hidden for individual chats)
        let group_name_row = Box::new(Orientation::Horizontal, 6);
        group_name_row.set_halign(Align::Center);
        group_name_row.set_margin_bottom(4);
        group_name_row.set_visible(false);

        let group_name_entry = gtk4::Entry::new();
        group_name_entry.set_width_chars(24);
        group_name_entry.set_halign(Align::Center);
        group_name_entry.add_css_class("title-2");
        group_name_entry.set_placeholder_text(Some("Group name"));

        // No explicit save button — auto-save on focus-out or Enter
        group_name_row.append(&group_name_entry);

        let subtitle_label = Label::new(Some(""));
        subtitle_label.add_css_class("dim-label");
        subtitle_label.set_halign(Align::Center);
        subtitle_label.set_margin_bottom(16);
        subtitle_label.set_selectable(true);

        content.append(&avatar);
        content.append(&name_label);
        content.append(&group_name_row);
        content.append(&subtitle_label);

        // Share/forward contact button
        let share_btn = Button::with_label("Share Contact");
        share_btn.add_css_class("flat");
        share_btn.set_halign(Align::Center);
        share_btn.set_margin_bottom(12);
        content.append(&share_btn);
        content.append(&Separator::new(Orientation::Horizontal));

        // ── Invite link section (group profile) ──
        let invite_section = Box::new(Orientation::Vertical, 4);
        invite_section.set_margin_top(12);
        invite_section.set_margin_bottom(12);
        invite_section.set_visible(false);

        let invite_heading = Label::new(Some("Group invite link"));
        invite_heading.add_css_class("heading");
        invite_heading.set_halign(Align::Start);

        let invite_row = Box::new(Orientation::Horizontal, 8);
        let invite_label = Label::new(Some(""));
        invite_label.set_halign(Align::Start);
        invite_label.set_hexpand(true);
        invite_label.set_selectable(true);
        invite_label.set_wrap(true);
        invite_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        invite_label.add_css_class("dim-label");

        let invite_copy_btn = Button::from_icon_name("edit-copy-symbolic");
        invite_copy_btn.add_css_class("flat");
        invite_copy_btn.set_tooltip_text(Some("Copy invite link"));
        invite_copy_btn.set_valign(Align::Start);

        invite_row.append(&invite_label);
        invite_row.append(&invite_copy_btn);

        invite_section.append(&invite_heading);
        invite_section.append(&invite_row);
        content.append(&invite_section);
        content.append(&Separator::new(Orientation::Horizontal));

        // ── Disappearing messages section (group profile) ──
        let disappearing_section = Box::new(Orientation::Vertical, 4);
        disappearing_section.set_margin_top(12);
        disappearing_section.set_margin_bottom(12);
        disappearing_section.set_margin_start(12);
        disappearing_section.set_margin_end(12);
        disappearing_section.set_visible(false);

        let disappearing_heading = Label::new(Some("Disappearing messages"));
        disappearing_heading.add_css_class("heading");
        disappearing_heading.set_halign(Align::Start);
        disappearing_section.append(&disappearing_heading);

        let disappearing_desc = Label::new(Some(
            "When enabled, new messages will disappear after the selected duration.",
        ));
        disappearing_desc.add_css_class("dim-label");
        disappearing_desc.add_css_class("caption");
        disappearing_desc.set_halign(Align::Start);
        disappearing_desc.set_wrap(true);
        disappearing_section.append(&disappearing_desc);

        let disappearing_dropdown =
            gtk4::DropDown::from_strings(&["Off", "24 hours", "7 days", "90 days"]);
        disappearing_dropdown.set_margin_top(4);
        disappearing_section.append(&disappearing_dropdown);

        content.append(&disappearing_section);
        content.append(&Separator::new(Orientation::Horizontal));

        // ── About section ──
        let about_section = Box::new(Orientation::Vertical, 4);
        about_section.set_margin_top(12);
        about_section.set_margin_bottom(12);
        about_section.set_visible(false);

        let about_heading = Label::new(Some("About"));
        about_heading.add_css_class("heading");
        about_heading.set_halign(Align::Start);

        let about_label = Label::new(Some(""));
        about_label.set_halign(Align::Start);
        about_label.set_wrap(true);
        about_label.set_selectable(true);

        about_section.append(&about_heading);
        about_section.append(&about_label);
        content.append(&about_section);

        // ── Media / Links / Docs tabbed section ──
        let media_section = Box::new(Orientation::Vertical, 4);
        media_section.set_margin_top(8);
        media_section.set_margin_bottom(12);

        let media_heading = Label::new(Some("Media, links and docs"));
        media_heading.add_css_class("heading");
        media_heading.set_halign(Align::Start);
        media_section.append(&media_heading);

        let media_count_label = Label::new(Some(""));
        media_count_label.add_css_class("dim-label");

        let notebook = gtk4::Notebook::new();
        notebook.set_show_border(false);

        // Tab 1: Media (images/videos/gifs)
        let media_grid = FlowBox::new();
        media_grid.set_max_children_per_line(3);
        media_grid.set_min_children_per_line(3);
        media_grid.set_column_spacing(4);
        media_grid.set_row_spacing(4);
        media_grid.set_selection_mode(gtk4::SelectionMode::None);
        media_grid.set_homogeneous(true);
        notebook.append_page(&media_grid, Some(&Label::new(Some("Media"))));

        // Tab 2: Links
        let links_list = ListBox::new();
        links_list.set_selection_mode(gtk4::SelectionMode::None);
        links_list.add_css_class("boxed-list");
        let links_scroll = ScrolledWindow::new();
        links_scroll.set_child(Some(&links_list));
        links_scroll.set_vexpand(false);
        links_scroll.set_max_content_height(300);
        links_scroll.set_propagate_natural_height(true);
        notebook.append_page(&links_scroll, Some(&Label::new(Some("Links"))));

        // Tab 3: Docs
        let docs_list = ListBox::new();
        docs_list.set_selection_mode(gtk4::SelectionMode::None);
        docs_list.add_css_class("boxed-list");
        let docs_scroll = ScrolledWindow::new();
        docs_scroll.set_child(Some(&docs_list));
        docs_scroll.set_vexpand(false);
        docs_scroll.set_max_content_height(300);
        docs_scroll.set_propagate_natural_height(true);
        notebook.append_page(&docs_scroll, Some(&Label::new(Some("Docs"))));

        media_section.append(&notebook);
        media_section.append(&media_count_label);
        content.append(&media_section);
        content.append(&Separator::new(Orientation::Horizontal));

        // ── Groups in common (contact profile) ──
        let groups_section = Box::new(Orientation::Vertical, 4);
        groups_section.set_margin_top(12);
        groups_section.set_margin_bottom(12);
        groups_section.set_visible(false);

        let groups_heading = Label::new(Some("Groups in common"));
        groups_heading.add_css_class("heading");
        groups_heading.set_halign(Align::Start);

        let groups_list = ListBox::new();
        groups_list.set_selection_mode(gtk4::SelectionMode::Single);
        groups_list.add_css_class("boxed-list");

        groups_section.append(&groups_heading);
        groups_section.append(&groups_list);
        content.append(&groups_section);

        // ── Members section (group profile) ──
        let members_section = Box::new(Orientation::Vertical, 4);
        members_section.set_margin_top(12);
        members_section.set_visible(false);

        let members_header = Box::new(Orientation::Horizontal, 8);
        let members_heading = Label::new(Some("Members"));
        members_heading.add_css_class("heading");
        members_heading.set_halign(Align::Start);
        members_heading.set_hexpand(true);

        let members_add_btn = Button::from_icon_name("list-add-symbolic");
        members_add_btn.add_css_class("flat");
        members_add_btn.set_tooltip_text(Some("Add member"));

        members_header.append(&members_heading);
        members_header.append(&members_add_btn);

        let members_search = SearchEntry::new();
        members_search.set_placeholder_text(Some("Search members"));
        members_search.set_margin_top(4);
        members_search.set_margin_bottom(4);

        let members_list = ListBox::new();
        members_list.set_selection_mode(gtk4::SelectionMode::None);
        members_list.add_css_class("boxed-list");

        // Member search filter
        {
            let search_ref = members_search.clone();
            members_list.set_filter_func(move |row| {
                let q = search_ref.text().to_lowercase();
                if q.is_empty() {
                    return true;
                }
                row.child()
                    .and_then(|hb| hb.first_child()) // skip avatar, get to name area
                    .and_then(|c| c.next_sibling()) // the name label
                    .and_then(|c| c.downcast::<Label>().ok())
                    .map(|l| l.text().to_lowercase().contains(&q))
                    .unwrap_or(true)
            });
            let list_ref = members_list.clone();
            members_search.connect_search_changed(move |_| list_ref.invalidate_filter());
        }

        members_section.append(&members_header);
        members_section.append(&members_search);
        members_section.append(&members_list);

        content.append(&members_section);

        // Leave group button — placed after members section with generous padding
        let leave_group_btn = Button::with_label("Leave group");
        leave_group_btn.add_css_class("destructive-action");
        leave_group_btn.set_margin_top(16);
        leave_group_btn.set_margin_bottom(24);
        leave_group_btn.set_halign(Align::Center);
        leave_group_btn.set_visible(false); // shown only for groups
        content.append(&leave_group_btn);

        scroll.set_child(Some(&content));
        root.append(&header);
        root.append(&scroll);

        let inner = Rc::new(ProfileInner {
            root,
            header_title,
            avatar,
            name_label,
            group_name_entry: group_name_entry.clone(),
            group_name_row: group_name_row.clone(),
            subtitle_label,
            about_label,
            about_section,
            media_section,
            media_grid,
            media_count_label,
            links_list,
            docs_list,
            groups_section,
            groups_list,
            members_section,
            members_search,
            members_list,
            members_add_btn,
            leave_group_btn,
            invite_section,
            invite_label,
            disappearing_section,
            disappearing_dropdown,
            bridge,
            share_btn,
            current_chat_id: RefCell::new(None),
            loaded_group_subject: RefCell::new(String::new()),
            cached_group_profiles: RefCell::new(std::collections::HashMap::new()),
            on_chat_selected: RefCell::new(None),
        });

        // Auto-save group name when the user presses Enter or leaves the field.
        // Uses a shared closure so both paths run the same logic.
        {
            let save_name = {
                let inner_c = inner.clone();
                move || {
                    let new_name = inner_c.group_name_entry.text().to_string();
                    let trimmed = new_name.trim();
                    if trimmed.is_empty() {
                        return;
                    }
                    // Only send if the subject actually changed vs what was loaded,
                    // so merely focusing in/out of the field doesn't re-send it.
                    if trimmed == inner_c.loaded_group_subject.borrow().trim() {
                        return;
                    }
                    if let Some(chat_id) = inner_c.current_chat_id.borrow().clone() {
                        inner_c.bridge.send_command(WaCommand::SetGroupSubject {
                            chat_id,
                            subject: trimmed.to_string(),
                        });
                        // Treat the new value as loaded so a subsequent focus-out
                        // without further edits doesn't send it again.
                        *inner_c.loaded_group_subject.borrow_mut() = trimmed.to_string();
                    }
                }
            };
            let save_enter = save_name.clone();
            group_name_entry.connect_activate(move |_| save_enter());

            let focus_ctrl = gtk4::EventControllerFocus::new();
            let save_blur = save_name.clone();
            focus_ctrl.connect_leave(move |_| save_blur());
            group_name_entry.add_controller(focus_ctrl);
        }

        // Disappearing messages dropdown
        {
            let inner_c = inner.clone();
            inner
                .disappearing_dropdown
                .connect_selected_notify(move |dd| {
                    let duration = match dd.selected() {
                        0 => 0u32,    // Off
                        1 => 86400,   // 24 hours
                        2 => 604800,  // 7 days
                        3 => 7776000, // 90 days
                        _ => 0,
                    };
                    if let Some(chat_id) = inner_c.current_chat_id.borrow().clone() {
                        inner_c.bridge.send_command(WaCommand::SetDisappearing {
                            chat_id,
                            duration_secs: duration,
                        });
                    }
                });
        }

        // Leave group button
        {
            let inner_c = inner.clone();
            inner.leave_group_btn.connect_clicked(move |_| {
                if let Some(chat_id) = inner_c.current_chat_id.borrow().clone() {
                    inner_c
                        .bridge
                        .send_command(WaCommand::LeaveGroup { chat_id });
                }
            });
        }

        // Copy invite link button
        {
            let inner_c = inner.clone();
            invite_copy_btn.connect_clicked(move |_| {
                let text = inner_c.invite_label.text().to_string();
                if !text.is_empty() {
                    if let Some(display) = gtk4::gdk::Display::default() {
                        display.clipboard().set_text(&text);
                    }
                }
            });
        }

        // Groups list row click → jump to that group
        {
            let inner_c = inner.clone();
            inner.groups_list.connect_row_activated(move |_, row| {
                let gid = row.widget_name().to_string();
                if gid == "__create_group__" {
                    // Create group with this contact
                    let contact_jid = inner_c.current_chat_id.borrow().clone().unwrap_or_default();
                    let contact_name = inner_c.name_label.text().to_string();
                    let bridge = inner_c.bridge.clone();
                    let parent = inner_c
                        .root
                        .root()
                        .and_then(|r| r.downcast::<gtk4::Window>().ok());

                    let dialog = libadwaita::AlertDialog::builder()
                        .heading("New Group")
                        .body(&format!("Create a group with {contact_name}"))
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("create", "Create");
                    dialog.set_response_appearance(
                        "create",
                        libadwaita::ResponseAppearance::Suggested,
                    );
                    dialog.set_default_response(Some("create"));
                    let entry = gtk4::Entry::new();
                    entry.set_placeholder_text(Some("Group subject"));
                    dialog.set_extra_child(Some(&entry));
                    dialog.connect_response(None, move |_, resp| {
                        if resp != "create" {
                            return;
                        }
                        let subject = entry.text().to_string();
                        if subject.is_empty() {
                            return;
                        }
                        bridge.send_command(crate::bridge::WaCommand::CreateGroup {
                            subject,
                            participants: vec![contact_jid.clone()],
                        });
                    });
                    dialog.present(parent.as_ref());
                    return;
                }
                let name = row
                    .child()
                    .and_then(|hb| hb.last_child())
                    .and_then(|c| c.downcast::<Label>().ok())
                    .map(|l| l.text().to_string())
                    .unwrap_or_default();
                if let Some(cb) = inner_c.on_chat_selected.borrow().as_ref() {
                    cb(gid, name);
                }
            });
        }

        // Share contact button — uses universal picker
        {
            let inner_c = inner.clone();
            inner.share_btn.connect_clicked(move |_| {
                let contact_name = inner_c.name_label.text().to_string();
                let contact_phone = inner_c.subtitle_label.text().to_string();
                if contact_name.is_empty() {
                    return;
                }
                let parent = inner_c
                    .root
                    .root()
                    .and_then(|r| r.downcast::<gtk4::Window>().ok());
                let bridge = inner_c.bridge.clone();
                crate::ui::chat_picker::show_chat_picker(
                    &format!("Share {contact_name}"),
                    false,
                    parent.as_ref(),
                    move |selected| {
                        if let Some(to_chat_id) = selected.first() {
                            let tmp_id = format!(
                                "tmp-contact-{:08x}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .subsec_nanos()
                            );
                            bridge.send_command(WaCommand::SendContact {
                                to_chat_id: to_chat_id.clone(),
                                contact_name: contact_name.clone(),
                                contact_phone: contact_phone.clone(),
                                tmp_id,
                            });
                        }
                    },
                );
            });
        }

        // Add member button
        {
            let inner_c = inner.clone();
            inner.members_add_btn.connect_clicked(move |_| {
                if let Some(chat_id) = inner_c.current_chat_id.borrow().clone() {
                    show_add_member_dialog(&inner_c, &chat_id);
                }
            });
        }

        ProfilePanel { inner }
    }

    pub fn widget(&self) -> &Box {
        &self.inner.root
    }

    /// Set callback for when a chat is selected (e.g. clicking a group in common)
    pub fn connect_chat_selected(&self, cb: impl Fn(String, String) + 'static) {
        *self.inner.on_chat_selected.borrow_mut() = Some(std::boxed::Box::new(cb));
    }

    /// Update the displayed name if this chat is currently open in the panel.
    /// Populate own profile fields with data from the server.
    pub fn set_own_profile(
        &self,
        name: &str,
        about: &str,
        description: &str,
        email: &str,
        website: &str,
        address: &str,
        category: &str,
    ) {
        let is_self = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == "self")
            .unwrap_or(false);
        if !is_self {
            return;
        }

        self.inner.name_label.set_text(name);
        self.inner.subtitle_label.set_text(if category.is_empty() {
            "Personal Account"
        } else {
            category
        });
        if !about.is_empty() {
            self.inner.about_label.set_text(about);
            self.inner.about_section.set_visible(true);
        }
        // TODO: add editable fields for description, email, website, address, category
        // For now, show them in the about section
        let mut info_parts = Vec::new();
        if !about.is_empty() {
            info_parts.push(about.to_string());
        }
        if !description.is_empty() {
            info_parts.push(format!("📝 {description}"));
        }
        if !email.is_empty() {
            info_parts.push(format!("📧 {email}"));
        }
        if !website.is_empty() {
            info_parts.push(format!("🌐 {website}"));
        }
        if !address.is_empty() {
            info_parts.push(format!("📍 {address}"));
        }
        if !info_parts.is_empty() {
            self.inner.about_label.set_text(&info_parts.join("\n"));
            self.inner.about_section.set_visible(true);
        }
    }

    pub fn update_name(&self, chat_id: &str, name: &str) {
        let is_current = self
            .inner
            .current_chat_id
            .borrow()
            .as_deref()
            .map(|id| id == chat_id)
            .unwrap_or(false);
        if is_current {
            self.inner.name_label.set_text(name);
            self.inner.group_name_entry.set_text(name);
            // Keep the dirty-check baseline in sync with the server-pushed name
            // so a live rename isn't echoed back on the next focus-out.
            *self.inner.loaded_group_subject.borrow_mut() = name.to_string();
        }
    }

    pub fn open(&self, chat_id: &str, chat_name: &str) {
        let inner = &self.inner;
        *inner.current_chat_id.borrow_mut() = Some(chat_id.to_string());
        inner.name_label.set_text(chat_name);
        inner.avatar.set_text(Some(chat_name));
        inner.members_search.set_text("");

        // Load cached avatar
        let safe = chat_id.replace(['/', '\\', '@', ':'], "_");
        let path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
        if path.exists() {
            if let Some(tex) = crate::ui::texture_cache::texture_from_filename(&path) {
                inner.avatar.set_custom_image(Some(&tex));
            }
        } else {
            inner.avatar.set_custom_image(None::<&gtk4::gdk::Paintable>);
        }

        // Load media thumbnails for this chat
        self.load_media_grid(chat_id);

        // Clear dynamic sections
        while let Some(child) = inner.members_list.first_child() {
            inner.members_list.remove(&child);
        }
        while let Some(child) = inner.groups_list.first_child() {
            inner.groups_list.remove(&child);
        }

        // Hide group-only sections by default
        inner.leave_group_btn.set_visible(false);
        inner.disappearing_section.set_visible(false);

        // Share Contact only makes sense for an individual contact — not for the
        // own profile or a group (their name/subtitle aren't a real phone number,
        // which would produce a broken vCard). Shown again in the individual branch.
        inner.share_btn.set_visible(false);

        if chat_id == "self" {
            // Own profile — editable
            inner.header_title.set_text("Your Profile");
            inner.name_label.set_visible(true);
            inner.group_name_row.set_visible(false);
            inner.members_section.set_visible(false);
            inner.groups_section.set_visible(false);
            inner.about_section.set_visible(true);
            inner.subtitle_label.set_text("Loading profile…");
            // Request own profile data (name, about, business info)
            inner.bridge.send_command(WaCommand::GetOwnProfile);
        } else if chat_id.ends_with("@g.us") {
            inner.header_title.set_text("Group Info");
            inner.name_label.set_visible(false);
            inner.group_name_row.set_visible(true);
            inner.group_name_entry.set_text(chat_name);
            *inner.loaded_group_subject.borrow_mut() = chat_name.to_string();
            inner.subtitle_label.set_text("Group");
            inner.members_section.set_visible(true);
            inner.groups_section.set_visible(false);
            inner.about_section.set_visible(false);
            inner.invite_section.set_visible(false); // shown when link arrives
            inner.disappearing_section.set_visible(true);
            // Request invite link
            inner.bridge.send_command(WaCommand::GetGroupInviteLink {
                chat_id: chat_id.to_string(),
            });
            // Use cache if available, otherwise fetch
            let cached = inner.cached_group_profiles.borrow().get(chat_id).cloned();
            if let Some((subject, desc, participants, is_admin)) = cached {
                self.set_group_profile(&subject, desc.as_deref(), &participants, is_admin);
            } else {
                inner.bridge.send_command(WaCommand::GetGroupInfo {
                    chat_id: chat_id.to_string(),
                });
            }
        } else {
            inner.header_title.set_text("Profile");
            inner.name_label.set_visible(true);
            inner.group_name_row.set_visible(false);
            inner.share_btn.set_visible(true);
            inner.members_section.set_visible(false);
            inner.groups_section.set_visible(true);
            inner.about_section.set_visible(true);
            inner.disappearing_section.set_visible(true);
            // If chat_id is a LID, resolve to the real phone number first
            // so the profile shows a proper +1... phone, not a raw LID like
            // "156753471783022:8@lid".
            let phone = {
                let lid_map = crate::ui::runtime::load_lid_phone_map();
                if chat_id.ends_with("@lid") {
                    // Strip device suffix before lookup (lid_to_phone keys are non-AD)
                    let base = chat_id
                        .split(':')
                        .next()
                        .map(|b| if b.ends_with("@lid") { b.to_string() } else { format!("{b}@lid") })
                        .unwrap_or_else(|| chat_id.to_string());
                    if let Some(pn) = lid_map.get(&base).cloned() {
                        crate::ui::runtime::display_name_from_jid(&pn)
                    } else {
                        crate::ui::runtime::display_name_from_jid(chat_id)
                    }
                } else {
                    crate::ui::runtime::display_name_from_jid(chat_id)
                }
            };
            inner.subtitle_label.set_text(&phone);
            inner.bridge.send_command(WaCommand::GetContactProfile {
                chat_id: chat_id.to_string(),
            });
        }
    }

    fn load_media_grid(&self, chat_id: &str) {
        let grid = &self.inner.media_grid;
        grid.remove_all();

        // Clear links and docs lists
        while let Some(child) = self.inner.links_list.first_child() {
            self.inner.links_list.remove(&child);
        }
        while let Some(child) = self.inner.docs_list.first_child() {
            self.inner.docs_list.remove(&child);
        }

        // Load messages for this chat
        let safe = chat_id.replace(['/', '\\', '@', ':'], "_");
        let msg_path = std::path::PathBuf::from("wa_messages").join(format!("{safe}.json"));
        let msgs: Vec<crate::bridge::IncomingMessage> =
            if let Ok(data) = std::fs::read_to_string(&msg_path) {
                serde_json::from_str(&data).unwrap_or_default()
            } else {
                vec![]
            };

        // Separate into media (images/videos), links, and documents
        let mut media_count = 0u32;
        let mut link_count = 0u32;
        let mut doc_count = 0u32;

        for msg in msgs.iter().rev() {
            // Media: images/videos/gifs/stickers with local paths
            if let Some(path) = &msg.media_local_path {
                let lower = path.to_lowercase();
                let is_image = lower.ends_with(".jpg")
                    || lower.ends_with(".jpeg")
                    || lower.ends_with(".png")
                    || lower.ends_with(".webp");
                let is_video =
                    lower.ends_with(".mp4") || lower.ends_with(".mov") || lower.ends_with(".gif");

                if (is_image || is_video) && media_count < 9 {
                    if let Some(tex) = crate::ui::texture_cache::texture_from_filename(path) {
                        let pic = Picture::for_paintable(&tex);
                        pic.set_size_request(90, 90);
                        pic.set_content_fit(gtk4::ContentFit::Cover);
                        pic.set_overflow(gtk4::Overflow::Hidden);
                        let frame = Frame::new(None);
                        frame.set_child(Some(&pic));
                        grid.append(&frame);
                    }
                    media_count += 1;
                } else if !is_image && !is_video {
                    // Document file
                    if doc_count < 20 {
                        let filename = msg.media_filename.as_deref().unwrap_or("Document");
                        let row = ListBoxRow::new();
                        let hbox = Box::new(Orientation::Horizontal, 8);
                        hbox.set_margin_top(6);
                        hbox.set_margin_bottom(6);
                        hbox.set_margin_start(8);
                        hbox.set_margin_end(8);
                        let icon = Label::new(Some("📄"));
                        let name_label = Label::new(Some(filename));
                        name_label.set_halign(Align::Start);
                        name_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                        name_label.set_hexpand(true);
                        hbox.append(&icon);
                        hbox.append(&name_label);
                        row.set_child(Some(&hbox));
                        self.inner.docs_list.append(&row);
                        doc_count += 1;
                    }
                }
            }

            // Links: messages with link preview data
            if let Some(url) = &msg.link_url {
                if link_count < 20 {
                    let row = ListBoxRow::new();
                    let vbox = Box::new(Orientation::Vertical, 2);
                    vbox.set_margin_top(6);
                    vbox.set_margin_bottom(6);
                    vbox.set_margin_start(8);
                    vbox.set_margin_end(8);
                    if let Some(title) = &msg.link_title {
                        let title_label = Label::new(Some(title));
                        title_label.set_halign(Align::Start);
                        title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                        title_label.add_css_class("caption");
                        vbox.append(&title_label);
                    }
                    let url_label = Label::new(Some(url));
                    url_label.set_halign(Align::Start);
                    url_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    url_label.add_css_class("dim-label");
                    url_label.add_css_class("caption");
                    url_label.set_selectable(true);
                    vbox.append(&url_label);
                    row.set_child(Some(&vbox));
                    self.inner.links_list.append(&row);
                    link_count += 1;
                }
            }
        }

        let total = media_count + link_count + doc_count;
        self.inner
            .media_count_label
            .set_text(&format!("{total} items"));
    }

    pub fn set_contact_profile(&self, about: Option<&str>, avatar_path: Option<&str>) {
        if let Some(text) = about {
            self.inner.about_label.set_text(text);
            self.inner.about_section.set_visible(true);
        }
        if let Some(path) = avatar_path {
            if let Some(tex) = crate::ui::texture_cache::texture_from_filename(path) {
                self.inner.avatar.set_custom_image(Some(&tex));
            }
        }
    }

    /// Display the group invite link
    pub fn set_invite_link(&self, link: &str) {
        self.inner.invite_label.set_text(link);
        self.inner.invite_section.set_visible(true);
    }

    /// Set groups in common for a contact profile
    pub fn set_groups_in_common(&self, groups: &[ChatSummary]) {
        let list = &self.inner.groups_list;
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        self.inner.groups_section.set_visible(true);

        // "Create group with..." row at top
        {
            let row = ListBoxRow::new();
            let hbox = Box::new(Orientation::Horizontal, 8);
            hbox.set_margin_start(8);
            hbox.set_margin_end(8);
            hbox.set_margin_top(6);
            hbox.set_margin_bottom(6);
            let icon = Label::new(Some("👥"));
            icon.set_size_request(32, 32);
            let lbl = Label::new(Some("Create group with this contact"));
            lbl.set_hexpand(true);
            lbl.set_halign(Align::Start);
            hbox.append(&icon);
            hbox.append(&lbl);
            row.set_child(Some(&hbox));
            row.set_widget_name("__create_group__");
            row.set_cursor_from_name(Some("pointer"));
            list.append(&row);
        }

        for g in groups {
            let row = ListBoxRow::new();
            let hbox = Box::new(Orientation::Horizontal, 8);
            hbox.set_margin_start(8);
            hbox.set_margin_end(8);
            hbox.set_margin_top(6);
            hbox.set_margin_bottom(6);

            let av = adw::Avatar::new(32, Some(&g.name), true);
            let safe = g.id.replace(['/', '\\', '@', ':'], "_");
            let av_path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
            if av_path.exists() {
                if let Some(tex) = crate::ui::texture_cache::texture_from_filename(&av_path) {
                    av.set_custom_image(Some(&tex));
                }
            }

            let lbl = Label::new(Some(&g.name));
            lbl.set_hexpand(true);
            lbl.set_halign(Align::Start);
            lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);

            hbox.append(&av);
            hbox.append(&lbl);
            row.set_child(Some(&hbox));
            row.set_widget_name(&g.id);
            row.set_cursor_from_name(Some("pointer"));
            list.append(&row);
        }
    }

    pub fn set_group_profile(
        &self,
        subject: &str,
        description: Option<&str>,
        participants: &[GroupMember],
        i_am_admin: bool,
    ) {
        // Cache for fast reopen
        if let Some(chat_id) = self.inner.current_chat_id.borrow().clone() {
            self.inner.cached_group_profiles.borrow_mut().insert(
                chat_id,
                (
                    subject.to_string(),
                    description.map(|s| s.to_string()),
                    participants.to_vec(),
                    i_am_admin,
                ),
            );
        }
        self.inner.name_label.set_text(subject);
        self.inner.group_name_entry.set_text(subject);
        *self.inner.loaded_group_subject.borrow_mut() = subject.to_string();
        self.inner
            .subtitle_label
            .set_text(&format!("{} members", participants.len()));

        if let Some(desc) = description.filter(|d| !d.is_empty()) {
            self.inner.about_label.set_text(desc);
            self.inner.about_section.set_visible(true);
        }

        while let Some(child) = self.inner.members_list.first_child() {
            self.inner.members_list.remove(&child);
        }

        // Click member → popover with options
        self.inner
            .members_list
            .set_selection_mode(gtk4::SelectionMode::Single);
        // Show/hide add member button based on admin status
        self.inner.members_add_btn.set_visible(i_am_admin);
        self.inner.leave_group_btn.set_visible(true);

        for member in participants {
            let row = ListBoxRow::new();
            row.set_cursor_from_name(Some("pointer"));
            let hbox = Box::new(Orientation::Horizontal, 8);
            hbox.set_margin_start(8);
            hbox.set_margin_end(8);
            hbox.set_margin_top(6);
            hbox.set_margin_bottom(6);

            let av = adw::Avatar::new(32, Some(&member.name), true);
            let safe = member.jid.replace(['/', '\\', '@', ':'], "_");
            let avatar_path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
            if avatar_path.exists() {
                if let Some(tex) = crate::ui::texture_cache::texture_from_filename(&avatar_path) {
                    av.set_custom_image(Some(&tex));
                }
            }

            let name_lbl = Label::new(Some(&member.name));
            name_lbl.set_hexpand(true);
            name_lbl.set_halign(Align::Start);
            name_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            name_lbl.set_max_width_chars(25);

            hbox.append(&av);
            hbox.append(&name_lbl);

            if member.is_admin {
                let admin_badge = Label::new(Some("Admin"));
                admin_badge.add_css_class("caption");
                admin_badge.add_css_class("accent");
                hbox.append(&admin_badge);
            }

            // Click → popover with Message / Remove options
            {
                let inner_c = self.inner.clone();
                let m_jid = member.jid.clone();
                let m_name = member.name.clone();
                let is_admin = i_am_admin;
                let gesture = gtk4::GestureClick::new();
                gesture.set_button(0); // any button
                gesture.connect_released(move |_, _, x, y| {
                    let popover = gtk4::Popover::new();
                    popover.set_parent(&inner_c.members_list);
                    popover.set_has_arrow(false);
                    let rect = gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
                    popover.set_pointing_to(Some(&rect));

                    let vbox = Box::new(Orientation::Vertical, 0);
                    vbox.set_width_request(180);

                    // Message user
                    let btn_msg = Button::with_label(&format!("Message {}", m_name));
                    btn_msg.set_has_frame(false);
                    btn_msg.add_css_class("flat");
                    if let Some(child) = btn_msg.child() {
                        if let Ok(lbl) = child.downcast::<Label>() {
                            lbl.set_halign(Align::Start);
                            lbl.set_margin_start(4);
                            lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                        }
                    }
                    let jid_c = m_jid.clone();
                    let name_c = m_name.clone();
                    let inner_cc = inner_c.clone();
                    let pop = popover.clone();
                    btn_msg.connect_clicked(move |_| {
                        if let Some(cb) = inner_cc.on_chat_selected.borrow().as_ref() {
                            cb(jid_c.clone(), name_c.clone());
                        }
                        pop.popdown();
                    });
                    vbox.append(&btn_msg);

                    // Admin actions: promote/demote + remove
                    if is_admin {
                        vbox.append(&Separator::new(Orientation::Horizontal));

                        // Promote / Demote admin toggle
                        let is_member_admin = inner_c
                            .cached_group_profiles
                            .borrow()
                            .get(&inner_c.current_chat_id.borrow().clone().unwrap_or_default())
                            .and_then(|(_, _, members, _)| {
                                members.iter().find(|m| m.jid == m_jid).map(|m| m.is_admin)
                            })
                            .unwrap_or(false);

                        if is_member_admin {
                            let btn_demote = Button::with_label("Demote from admin");
                            btn_demote.set_has_frame(false);
                            btn_demote.add_css_class("flat");
                            if let Some(child) = btn_demote.child() {
                                if let Ok(lbl) = child.downcast::<Label>() {
                                    lbl.set_halign(Align::Start);
                                    lbl.set_margin_start(4);
                                }
                            }
                            let bridge = inner_c.bridge.clone();
                            let chat_id =
                                inner_c.current_chat_id.borrow().clone().unwrap_or_default();
                            let jid_c2 = m_jid.clone();
                            let pop = popover.clone();
                            btn_demote.connect_clicked(move |_| {
                                bridge.send_command(WaCommand::DemoteGroupAdmin {
                                    chat_id: chat_id.clone(),
                                    jid: jid_c2.clone(),
                                });
                                pop.popdown();
                            });
                            vbox.append(&btn_demote);
                        } else {
                            let btn_promote = Button::with_label("Promote to admin");
                            btn_promote.set_has_frame(false);
                            btn_promote.add_css_class("flat");
                            if let Some(child) = btn_promote.child() {
                                if let Ok(lbl) = child.downcast::<Label>() {
                                    lbl.set_halign(Align::Start);
                                    lbl.set_margin_start(4);
                                }
                            }
                            let bridge = inner_c.bridge.clone();
                            let chat_id =
                                inner_c.current_chat_id.borrow().clone().unwrap_or_default();
                            let jid_c2 = m_jid.clone();
                            let pop = popover.clone();
                            btn_promote.connect_clicked(move |_| {
                                bridge.send_command(WaCommand::PromoteGroupAdmin {
                                    chat_id: chat_id.clone(),
                                    jid: jid_c2.clone(),
                                });
                                pop.popdown();
                            });
                            vbox.append(&btn_promote);
                        }

                        // Remove from group
                        let btn_rm = Button::with_label("Remove from group");
                        btn_rm.set_has_frame(false);
                        btn_rm.add_css_class("flat");
                        btn_rm.add_css_class("destructive-action");
                        if let Some(child) = btn_rm.child() {
                            if let Ok(lbl) = child.downcast::<Label>() {
                                lbl.set_halign(Align::Start);
                                lbl.set_margin_start(4);
                            }
                        }
                        let bridge = inner_c.bridge.clone();
                        let chat_id = inner_c.current_chat_id.borrow().clone().unwrap_or_default();
                        let jid_c = m_jid.clone();
                        let pop = popover.clone();
                        btn_rm.connect_clicked(move |_| {
                            bridge.send_command(WaCommand::RemoveGroupParticipant {
                                chat_id: chat_id.clone(),
                                jid: jid_c.clone(),
                            });
                            pop.popdown();
                        });
                        vbox.append(&btn_rm);
                    }

                    popover.set_child(Some(&vbox));
                    popover.popup();
                });
                row.add_controller(gesture);
            }

            row.set_child(Some(&hbox));
            row.set_widget_name(&member.jid);
            self.inner.members_list.append(&row);
        }
        self.inner.members_section.set_visible(true);
    }
}

fn show_add_member_dialog(panel: &Rc<ProfileInner>, chat_id: &str) {
    let parent = panel
        .root
        .root()
        .and_then(|r| r.downcast::<gtk4::Window>().ok());
    let bridge = panel.bridge.clone();
    let cid = chat_id.to_string();
    crate::ui::chat_picker::show_chat_picker(
        "Add member",
        true,
        parent.as_ref(),
        move |selected| {
            for jid in &selected {
                // Extract phone number from JID (e.g. "1234567890@s.whatsapp.net" → "1234567890")
                let phone = jid.split('@').next().unwrap_or(jid).to_string();
                bridge.send_command(WaCommand::AddGroupParticipant {
                    chat_id: cid.clone(),
                    phone,
                });
            }
        },
    );
}
