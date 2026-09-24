use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Box, Button, Orientation, Revealer, RevealerTransitionType, Stack};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, WaCommand, WaEvent};
use crate::ui::chat_list::ChatListPanel;
use crate::ui::chat_view::ChatViewPanel;
use crate::ui::login::LoginScreen;
use crate::ui::new_chat_panel::NewChatPanel;
use crate::ui::profile_panel::ProfilePanel;
use crate::ui::settings::SettingsHandle;

/// Matches the comfortable width reached by adjusting the live layout: wide
/// enough for the 52px avatars and row metadata without stealing space from
/// message bubbles on the user's scaled desktop.
const DEFAULT_SIDEBAR_WIDTH: i32 = 400;
const MIN_SIDEBAR_WIDTH: i32 = 320;
const MAX_SIDEBAR_WIDTH: i32 = 640;
/// Preserve a useful message surface when a previously-wide window is made
/// narrow. The preferred sidebar width itself is retained and restored when
/// room returns.
const MIN_CHAT_WIDTH: i32 = 360;

fn sanitize_sidebar_width(saved: Option<i32>) -> i32 {
    match saved {
        Some(width) if (MIN_SIDEBAR_WIDTH..=MAX_SIDEBAR_WIDTH).contains(&width) => width,
        _ => DEFAULT_SIDEBAR_WIDTH,
    }
}

fn effective_sidebar_width(preferred: i32, available_width: i32) -> i32 {
    let preferred = preferred.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    if available_width <= 0 {
        return preferred;
    }

    // On very small windows the sidebar's minimum wins, preventing GTK from
    // shrinking it below the point where row avatars are visibly clipped.
    let responsive_max = (available_width - MIN_CHAT_WIDTH).max(MIN_SIDEBAR_WIDTH);
    preferred.min(responsive_max)
}

fn queue_sidebar_width_save(
    settings: &SettingsHandle,
    pending: &Rc<RefCell<Option<gtk4::glib::SourceId>>>,
    width: i32,
) {
    if let Some(source) = pending.borrow_mut().take() {
        source.remove();
    }

    let settings = settings.clone();
    let pending_done = pending.clone();
    let source = gtk4::glib::timeout_add_local_once(Duration::from_millis(180), move || {
        pending_done.borrow_mut().take();
        settings.set_sidebar_width(width);
    });
    *pending.borrow_mut() = Some(source);
}

fn configure_sidebar_pane(paned: &gtk4::Paned, settings: &SettingsHandle) {
    let preferred = Rc::new(Cell::new(sanitize_sidebar_width(
        settings.get().sidebar_width,
    )));
    let applying_layout = Rc::new(Cell::new(false));
    let pointer_adjusting = Rc::new(Cell::new(false));
    let pending_save: Rc<RefCell<Option<gtk4::glib::SourceId>>> = Rc::new(RefCell::new(None));

    paned.set_position(preferred.get());

    // Only a real divider interaction updates the preference. Layout-driven
    // position changes (for example when a window is temporarily narrowed)
    // must not overwrite the width the user chose on a roomy window.
    // Observe raw button events instead of adding a second gesture recognizer:
    // a competing GestureClick can steal the sequence from Paned's own drag.
    let pointer = gtk4::EventControllerLegacy::new();
    pointer.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let paned = paned.clone();
        let pointer_adjusting = pointer_adjusting.clone();
        let preferred = preferred.clone();
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        pointer.connect_event(move |_, event| {
            let Some(button) = event.downcast_ref::<gtk4::gdk::ButtonEvent>() else {
                return gtk4::glib::Propagation::Proceed;
            };
            if button.button() != 1 {
                return gtk4::glib::Propagation::Proceed;
            }

            match event.event_type() {
                gtk4::gdk::EventType::ButtonPress => {
                    // GDK reports surface coordinates, while Paned::position
                    // is local to the Paned (which starts after the icon rail).
                    let on_separator = event
                        .position()
                        .and_then(|(x, y)| {
                            let root = paned.root()?;
                            root.compute_point(
                                &paned,
                                &gtk4::graphene::Point::new(x as f32, y as f32),
                            )
                        })
                        .map(|point| {
                            (f64::from(point.x()) - f64::from(paned.position())).abs() <= 18.0
                        })
                        .unwrap_or(false);
                    pointer_adjusting.set(on_separator);
                }
                gtk4::gdk::EventType::ButtonRelease => {
                    if pointer_adjusting.replace(false) {
                        let width = paned.position().clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
                        preferred.set(width);
                        if let Some(source) = pending_save.borrow_mut().take() {
                            source.remove();
                        }
                        settings.set_sidebar_width(width);
                    }
                }
                _ => {}
            }

            // Passive observation only; Paned remains the sole gesture owner.
            gtk4::glib::Propagation::Proceed
        });
    }
    paned.add_controller(pointer);

    {
        let preferred = preferred.clone();
        let applying_layout = applying_layout.clone();
        let pointer_adjusting = pointer_adjusting.clone();
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        paned.connect_position_notify(move |paned| {
            if applying_layout.get() || !pointer_adjusting.get() {
                return;
            }
            let width = paned.position().clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
            preferred.set(width);
            queue_sidebar_width_save(&settings, &pending_save, width);
        });
    }

    // Keyboard divider movement goes through this action signal. Read the
    // resulting value on the next main-loop turn, after GTK's default handler.
    {
        let paned_weak = paned.downgrade();
        let preferred = preferred.clone();
        let settings = settings.clone();
        let pending_save = pending_save.clone();
        paned.connect_move_handle(move |_, _| {
            let paned_weak = paned_weak.clone();
            let preferred = preferred.clone();
            let settings = settings.clone();
            let pending_save = pending_save.clone();
            gtk4::glib::idle_add_local_once(move || {
                let Some(paned) = paned_weak.upgrade() else {
                    return;
                };
                let width = paned.position().clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
                preferred.set(width);
                queue_sidebar_width_save(&settings, &pending_save, width);
            });
            false
        });
    }

    // `max-position` changes whenever the Paned allocation changes. Apply a
    // temporary effective width that leaves room for messages; because the
    // preferred value lives separately, growing the window restores it.
    {
        let preferred = preferred.clone();
        let applying_layout = applying_layout.clone();
        let pointer_adjusting = pointer_adjusting.clone();
        paned.connect_max_position_notify(move |paned| {
            if pointer_adjusting.get() {
                return;
            }
            let target = effective_sidebar_width(preferred.get(), paned.width());
            if paned.position() != target {
                applying_layout.set(true);
                paned.set_position(target);
                applying_layout.set(false);
            }
        });
    }
}

#[derive(Clone)]
pub struct MainWindow {
    inner: Rc<MainWindowInner>,
}

struct MainWindowInner {
    window: adw::ApplicationWindow,
    gtk_app: adw::Application,
    stack: Stack,
    login_screen: LoginScreen,
    chat_list: ChatListPanel,
    chat_view: ChatViewPanel,
    profile_panel: ProfilePanel,
    new_chat_panel: NewChatPanel,
    profile_revealer: Revealer,
    sidebar_stack: Stack,
    sync_revealer: Revealer,
    sync_progress: gtk4::ProgressBar,
    /// In-flight sync counter. SyncProgress(bool) producers are uncoordinated
    /// (reconnect resync, offline-sync preview/complete), so a plain boolean
    /// let one producer's `false` hide the bar while another sync was still
    /// running. We reference-count instead: reveal while > 0.
    sync_count: std::cell::Cell<u32>,
    bridge: Arc<Bridge>,
    debug_buf: gtk4::TextBuffer,
    debug_revealer: Revealer,
    own_avatar: libadwaita::Avatar,
    own_jid: RefCell<String>,
    rail_favourites: Box,
    settings: SettingsHandle,
    /// Cached own profile data (populated by GetOwnProfile response)
    own_profile_data: RefCell<Option<OwnProfileData>>,
    /// Cached chat list for multi-send
    cached_chats: RefCell<Vec<crate::bridge::ChatSummary>>,
    /// App-wide toast overlay for surfacing errors + confirmations.
    toast_overlay: adw::ToastOverlay,
    /// Most recent toast and display time. Reconnect loops are deduplicated,
    /// but the same useful error may be shown again after a short quiet period.
    last_toast: RefCell<Option<(String, Instant)>>,
}

#[derive(Clone)]
struct OwnProfileData {
    name: String,
    about: String,
    description: String,
    email: String,
    website: String,
    address: String,
    category: String,
}

impl MainWindow {
    pub fn new(app: &adw::Application, bridge: Arc<Bridge>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("WhatsApp")
            .default_width(1200)
            .default_height(800)
            .icon_name("com.whatsapp.desktop")
            .build();

        // Top-level stack: login screen vs main chat layout
        let stack = Stack::new();

        // Login screen (QR code)
        let login_screen = LoginScreen::new(bridge.clone());

        // Main layout: sidebar (chat list / new chat) + chat view + profile panel
        let chat_view = ChatViewPanel::new(bridge.clone());
        let profile_panel = ProfilePanel::new(bridge.clone());

        // Profile panel on the right, hidden by default
        let profile_revealer = Revealer::new();
        profile_revealer.set_transition_type(RevealerTransitionType::SlideRight);
        profile_revealer.set_child(Some(profile_panel.widget()));
        profile_revealer.set_reveal_child(false);
        profile_revealer.set_hexpand(false);

        // Present profile info as an end-aligned side sheet. Keeping it in an
        // overlay prevents opening a 360px panel from collapsing the message
        // view on split-screen/narrow windows.
        let chat_area = gtk4::Overlay::new();
        chat_view.widget().set_hexpand(true);
        chat_area.set_child(Some(chat_view.widget()));
        profile_revealer.set_halign(gtk4::Align::End);
        profile_revealer.set_valign(gtk4::Align::Fill);
        profile_revealer.set_vexpand(true);
        chat_area.add_overlay(&profile_revealer);
        chat_area.set_measure_overlay(&profile_revealer, false);

        let chat_view_for_list = chat_view.clone();
        let bridge_for_list = bridge.clone();
        let profile_rev_for_list = profile_revealer.clone();
        let app_for_list = app.clone();
        let chat_list = ChatListPanel::new(bridge.clone(), move |chat_id, chat_name| {
            activate_chat(&chat_view_for_list, &bridge_for_list, &chat_id, &chat_name);
            // Dismiss any notifications for this chat
            withdraw_chat_notification(&app_for_list, &chat_id);
            // Close profile panel when switching chats
            profile_rev_for_list.set_reveal_child(false);
        });

        // Sidebar stack: chat list vs new chat panel
        let sidebar_stack = Stack::new();
        sidebar_stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
        sidebar_stack.add_named(chat_list.widget(), Some("chats"));

        // New chat panel — selecting a contact switches back to chat list
        let sidebar_stack_for_new = sidebar_stack.clone();
        let chat_view_for_new = chat_view.clone();
        let bridge_for_new = bridge.clone();
        let chat_list_for_new = chat_list.clone();
        let profile_rev_for_new = profile_revealer.clone();
        let app_for_new = app.clone();
        let new_chat_panel = NewChatPanel::new(bridge.clone(), move |jid, name| {
            // Switch back to chat list and open the selected chat
            sidebar_stack_for_new.set_visible_child_name("chats");
            activate_chat(&chat_view_for_new, &bridge_for_new, &jid, &name);
            chat_list_for_new.select_chat(&jid);
            profile_rev_for_new.set_reveal_child(false);
            withdraw_chat_notification(&app_for_new, &jid);
        });
        sidebar_stack.add_named(new_chat_panel.widget(), Some("new-chat"));

        // ── Send Groups panel ──
        let send_groups_panel = build_send_groups_panel(
            bridge.clone(),
            &sidebar_stack,
            chat_view.clone(),
            &profile_revealer,
        );
        sidebar_stack.add_named(&send_groups_panel, Some("send-groups"));

        sidebar_stack.set_visible_child_name("chats");

        let settings = SettingsHandle::new();

        let paned = gtk4::Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(&sidebar_stack));
        paned.set_end_child(Some(&chat_area));
        paned.set_shrink_start_child(true);
        paned.set_shrink_end_child(true);
        // When the WINDOW is resized (drag corner / maximize), the sidebar
        // stays at whatever width the user dragged it to via the internal
        // divider — only the message panel grows. Without this, both panes
        // share extra space proportionally, which made the chat list eat
        // pixels it doesn't need on wide displays.
        // The user can still resize the sidebar by dragging the divider.
        paned.set_resize_start_child(false);
        paned.set_resize_end_child(true);
        configure_sidebar_pane(&paned, &settings);

        // Sync banner — pulsing progress bar shown while history syncs.
        // A progress bar pulses smoothly via the GTK animation framework,
        // which doesn't freeze like a Spinner does during heavy widget work.
        let sync_bar = Box::new(Orientation::Vertical, 4);
        sync_bar.set_margin_start(0);
        sync_bar.set_margin_end(0);

        let sync_progress = gtk4::ProgressBar::new();
        sync_progress.add_css_class("sync-progress");
        sync_progress.set_show_text(false);
        sync_bar.append(&sync_progress);

        let sync_revealer = Revealer::new();
        sync_revealer.set_transition_type(RevealerTransitionType::SlideDown);
        sync_revealer.set_child(Some(&sync_bar));
        sync_revealer.set_reveal_child(false);

        // Debug buffer (kept for log_debug but not shown in UI)
        let debug_buf = gtk4::TextBuffer::new(None::<&gtk4::TextTagTable>);
        let debug_revealer = Revealer::new();

        // ── Icon rail (thin left panel like WhatsApp Web) ──
        let icon_rail = Box::new(Orientation::Vertical, 0);
        icon_rail.set_width_request(64);
        icon_rail.add_css_class("icon-rail");
        icon_rail.set_vexpand(true);

        // Pinned favourites section (populated from ChatsLoaded)
        let rail_favourites = Box::new(Orientation::Vertical, 12);
        rail_favourites.set_valign(gtk4::Align::Start);
        rail_favourites.set_margin_top(14);
        rail_favourites.set_halign(gtk4::Align::Center);

        let rail_top = Box::new(Orientation::Vertical, 0);
        rail_top.set_vexpand(true);
        rail_top.set_valign(gtk4::Align::Start);
        rail_top.set_margin_top(8);
        rail_top.append(&rail_favourites);

        // Bottom: settings button + own avatar (clickable → opens own profile editor)
        let rail_bottom = Box::new(Orientation::Vertical, 8);
        rail_bottom.set_valign(gtk4::Align::End);
        rail_bottom.set_margin_bottom(12);
        rail_bottom.set_halign(gtk4::Align::Center);

        // Send Groups button (opens send-groups panel in sidebar)
        let multi_send_btn = Button::from_icon_name("mail-send-symbolic");
        multi_send_btn.add_css_class("flat");
        multi_send_btn.add_css_class("circular");
        multi_send_btn.set_tooltip_text(Some("Send Groups"));
        rail_bottom.append(&multi_send_btn);

        let settings_btn = Button::from_icon_name("preferences-system-symbolic");
        settings_btn.add_css_class("flat");
        settings_btn.add_css_class("circular");
        settings_btn.set_tooltip_text(Some("Settings"));
        rail_bottom.append(&settings_btn);

        let own_avatar = libadwaita::Avatar::new(36, Some("Me"), true);
        own_avatar.set_cursor_from_name(Some("pointer"));
        own_avatar.set_tooltip_text(Some("Your profile"));

        // Wire after inner is created (need inner for profile data)
        // Done below after inner is constructed

        let own_avatar_btn = Button::new();
        own_avatar_btn.add_css_class("flat");
        own_avatar_btn.add_css_class("circular");
        own_avatar_btn.set_tooltip_text(Some("Your profile"));
        own_avatar_btn.set_child(Some(&own_avatar));
        rail_bottom.append(&own_avatar_btn);

        icon_rail.append(&rail_top);
        icon_rail.append(&rail_bottom);

        // Wrap paned with icon rail
        let content_hbox = Box::new(Orientation::Horizontal, 0);
        content_hbox.append(&icon_rail);
        content_hbox.append(&paned);

        let main_box = Box::new(Orientation::Vertical, 0);
        main_box.append(&sync_revealer);
        main_box.append(&content_hbox);

        stack.add_named(login_screen.widget(), Some("login"));
        stack.add_named(&main_box, Some("main"));
        stack.set_visible_child_name("login");

        // Top-level header bar — system title bar above everything
        let top_header = adw::HeaderBar::new();
        let app_title = gtk4::Label::new(Some("WhatsApp"));
        app_title.add_css_class("title");
        top_header.set_title_widget(Some(&app_title));

        // Single app-wide toast overlay. Without this, WaEvent::ErrorToast (and
        // every "failed to …" the runtime emits) vanished into the log — the
        // user never learned anything failed. Everything routes through here.
        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&stack));

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&top_header);
        toolbar_view.set_content(Some(&toast_overlay));

        window.set_content(Some(&toolbar_view));

        // Compact breakpoints retain all primary actions while reclaiming
        // horizontal room: favourites collapse first, then the sidebar/profile
        // settle at smaller but still usable widths.
        let compact = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            900.0,
            adw::LengthUnit::Sp,
        ));
        compact.add_setter(&rail_top, "visible", Some(&false.to_value()));
        compact.add_setter(&icon_rail, "width-request", Some(&52i32.to_value()));
        window.add_breakpoint(compact);

        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            700.0,
            adw::LengthUnit::Sp,
        ));
        narrow.add_setter(
            profile_panel.widget(),
            "width-request",
            Some(&300i32.to_value()),
        );
        window.add_breakpoint(narrow);

        let inner = Rc::new(MainWindowInner {
            window,
            gtk_app: app.clone(),
            stack,
            login_screen,
            chat_list: chat_list.clone(),
            chat_view: chat_view.clone(),
            profile_panel,
            new_chat_panel,
            profile_revealer,
            sidebar_stack,
            sync_revealer,
            sync_progress,
            sync_count: std::cell::Cell::new(0),
            bridge,
            debug_buf,
            debug_revealer,
            own_avatar,
            own_jid: RefCell::new(String::new()),
            rail_favourites,
            settings,
            own_profile_data: RefCell::new(None),
            cached_chats: RefCell::new(Vec::new()),
            toast_overlay,
            last_toast: RefCell::new(None),
        });

        // Wire own avatar click → open profile window with cached data
        {
            let inner_c = inner.clone();
            own_avatar_btn.connect_clicked(move |_| {
                let name = inner_c
                    .own_avatar
                    .text()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let data = inner_c.own_profile_data.borrow().clone();
                let jid = inner_c.own_jid.borrow().clone();
                let parent = inner_c.window.upcast_ref::<gtk4::Window>();
                open_own_profile_window(Some(parent), &inner_c.bridge, &name, data.as_ref(), &jid);
            });
        }

        // Wire settings button
        {
            let inner_c = inner.clone();
            settings_btn.connect_clicked(move |_| {
                let parent = inner_c.window.upcast_ref::<gtk4::Window>();
                crate::ui::settings::show_settings_window(
                    &inner_c.settings,
                    Some(parent),
                    &inner_c.bridge,
                );
            });
        }

        // Wire send-groups button → switch sidebar to send-groups panel
        {
            let stack = inner.sidebar_stack.clone();
            multi_send_btn.connect_clicked(move |_| {
                if stack.visible_child_name().as_deref() == Some("send-groups") {
                    stack.set_visible_child_name("chats");
                } else {
                    stack.set_visible_child_name("send-groups");
                }
            });
        }

        // Wire new-chat button to switch sidebar stack
        {
            let stack = inner.sidebar_stack.clone();
            inner.chat_list.connect_new_chat(move || {
                stack.set_visible_child_name("new-chat");
            });
        }

        // Wire new-chat panel back button to return to chat list
        {
            let stack = inner.sidebar_stack.clone();
            inner.new_chat_panel.connect_back(move || {
                stack.set_visible_child_name("chats");
            });
        }

        // Wire header name click to toggle profile panel
        inner.chat_view.connect_header_click({
            let profile_panel = inner.profile_panel.clone();
            let profile_rev = inner.profile_revealer.clone();
            move |chat_id, chat_name| {
                let showing = profile_rev.reveals_child();
                if showing {
                    profile_rev.set_reveal_child(false);
                } else {
                    profile_panel.open(&chat_id, &chat_name);
                    profile_rev.set_reveal_child(true);
                }
            }
        });

        // Avatar click in messages → open profile
        inner.chat_view.connect_profile_open({
            let profile_panel = inner.profile_panel.clone();
            let profile_rev = inner.profile_revealer.clone();
            move |chat_id, chat_name| {
                profile_panel.open(&chat_id, &chat_name);
                profile_rev.set_reveal_child(true);
            }
        });

        inner.profile_panel.connect_close_requested({
            let profile_rev = inner.profile_revealer.clone();
            move || profile_rev.set_reveal_child(false)
        });

        // Escape dismisses the profile side panel. Previously the only close
        // paths were re-clicking the chat header or switching chats — a keyboard
        // user couldn't dismiss it at all. Capture phase so it fires even when a
        // child widget (e.g. the group-name entry) holds focus.
        {
            let key = gtk4::EventControllerKey::new();
            key.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let profile_rev = inner.profile_revealer.clone();
            key.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == gtk4::gdk::Key::Escape && profile_rev.reveals_child() {
                    profile_rev.set_reveal_child(false);
                    gtk4::glib::Propagation::Stop
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            });
            inner.profile_panel.widget().add_controller(key);
        }

        // Clicking a group in profile panel jumps to that chat
        inner.profile_panel.connect_chat_selected({
            let chat_view = inner.chat_view.clone();
            let bridge_ref = inner.bridge.clone();
            let chat_list = inner.chat_list.clone();
            let sidebar_stack = inner.sidebar_stack.clone();
            let profile_rev = inner.profile_revealer.clone();
            let app = inner.gtk_app.clone();
            move |chat_id, chat_name| {
                activate_chat(&chat_view, &bridge_ref, &chat_id, &chat_name);
                sidebar_stack.set_visible_child_name("chats");
                chat_list.select_chat(&chat_id);
                profile_rev.set_reveal_child(false);
                withdraw_chat_notification(&app, &chat_id);
            }
        });

        // ── Close-to-tray: hide window, show tray icon ──
        {
            let settings_c = inner.settings.clone();
            let gtk_app_c = inner.gtk_app.clone();
            let win_for_tray = inner.window.clone();
            let inner_tray = inner.clone();
            // Track whether we've already spawned a tray icon
            let tray_spawned = std::cell::Cell::new(false);
            inner.window.connect_close_request(move |win| {
                if settings_c.get().close_to_tray {
                    win.set_visible(false);
                    // No chat is "actively viewed" while hidden in the tray.
                    // Clearing the active chat stops the runtime from marking
                    // incoming messages read (silent blue-ticks) and from
                    // suppressing their unread badge bump while the window is
                    // not on screen.
                    inner_tray
                        .bridge
                        .send_command(crate::bridge::WaCommand::SetActiveChat { chat_id: None });
                    // Hold the application open even with no visible windows.
                    std::mem::forget(gtk_app_c.hold());

                    // Spawn a StatusNotifierItem tray icon (once) and poll it
                    if !tray_spawned.get() {
                        tray_spawned.set(true);
                        let tray_handle = crate::ui::tray::spawn_tray_icon();
                        let win_poll = win_for_tray.clone();
                        let app_poll = gtk_app_c.clone();
                        // Poll the tray handle every 250ms on the GTK main loop
                        gtk4::glib::timeout_add_local(
                            std::time::Duration::from_millis(250),
                            move || {
                                if tray_handle
                                    .show_requested
                                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                                {
                                    win_poll.set_visible(true);
                                    win_poll.present();
                                }
                                if tray_handle
                                    .quit_requested
                                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                                {
                                    app_poll.quit();
                                    return gtk4::glib::ControlFlow::Break;
                                }
                                gtk4::glib::ControlFlow::Continue
                            },
                        );
                    }

                    gtk4::glib::Propagation::Stop
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            });
        }

        // ── Re-sync active chat on restore ──
        // The close-to-tray path clears the runtime's active chat so
        // hidden-window messages aren't silently blue-ticked. When the window
        // becomes active again with a chat still open, tell the runtime that
        // chat is active once more so its unread badge is cleared and read
        // receipts resume normally. Covers every restore path uniformly
        // (tray click, taskbar, alt-tab, show-window action).
        {
            let inner_active = inner.clone();
            inner.window.connect_is_active_notify(move |win| {
                if win.is_active() {
                    reassert_2fa_clipboard();
                }
                if win.is_active()
                    && let Some(chat_id) = inner_active.chat_view.current_chat_id()
                {
                    inner_active
                        .bridge
                        .send_command(crate::bridge::WaCommand::SetActiveChat {
                            chat_id: Some(chat_id),
                        });
                }
            });
        }

        // Register "show-window" action (for notification click → show window)
        {
            let win_c = inner.window.clone();
            let action = gtk4::gio::SimpleAction::new("show-window", None);
            action.connect_activate(move |_, _| {
                win_c.set_visible(true);
                win_c.present();
            });
            inner.gtk_app.add_action(&action);
        }

        // Register "open-chat" action: clicking a message notification raises the
        // window AND opens the originating chat (previously the click did nothing).
        {
            let inner_c = inner.clone();
            let action =
                gtk4::gio::SimpleAction::new("open-chat", Some(gtk4::glib::VariantTy::STRING));
            action.connect_activate(move |_, param| {
                let Some(chat_id) = param.and_then(|p| p.get::<String>()) else {
                    return;
                };
                inner_c.window.set_visible(true);
                inner_c.window.present();
                let chat_name = inner_c
                    .chat_list
                    .chat_name(&chat_id)
                    .unwrap_or_else(|| chat_id.clone());
                activate_chat(&inner_c.chat_view, &inner_c.bridge, &chat_id, &chat_name);
                // Make the sidebar selection follow: force the chat-list page
                // and highlight the opened row so the sidebar isn't out of sync
                // with the message panel after a notification click.
                inner_c.sidebar_stack.set_visible_child_name("chats");
                inner_c.chat_list.select_chat(&chat_id);
                inner_c.profile_revealer.set_reveal_child(false);
                withdraw_chat_notification(&inner_c.gtk_app, &chat_id);
            });
            inner.gtk_app.add_action(&action);
        }

        MainWindow { inner }
    }

    pub fn present(&self) {
        self.inner.window.present();
        // If the window was hidden (close-to-tray), restore it
        if !self.inner.window.is_visible() {
            self.inner.window.set_visible(true);
        }
    }

    /// Return the underlying GTK window for app-level re-activation.
    pub fn gtk_window(&self) -> Option<gtk4::Window> {
        Some(self.inner.window.clone().upcast())
    }

    fn log_debug(&self, msg: &str) {
        let buf = &self.inner.debug_buf;
        let mut end = buf.end_iter();
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        buf.insert(&mut end, &format!("[{ts}] {msg}\n"));
        // Keep buffer from growing unbounded — trim to last 500 lines
        let line_count = buf.line_count();
        if line_count > 500 {
            let mut start = buf.start_iter();
            let mut trim_end = buf.iter_at_line(line_count - 500).unwrap_or(start);
            buf.delete(&mut start, &mut trim_end);
        }
    }

    pub fn handle_event(&self, event: WaEvent) {
        let inner = &self.inner;
        // Log event to debug console
        let debug_line = match &event {
            WaEvent::QrCode(_) => "QrCode received".to_string(),
            WaEvent::Connected { .. } => "Connected".to_string(),
            WaEvent::Disconnected(r) => format!("Disconnected: {r}"),
            WaEvent::LoggedOut(r) => format!("LoggedOut: {r}"),
            WaEvent::ChatsLoaded(c) => format!("ChatsLoaded: {} chats", c.len()),
            WaEvent::ChatAdded(c) => format!("ChatAdded: {} ({})", c.name, c.id),
            WaEvent::MessageReceived(m) => format!(
                "MessageReceived: chat={} id={} text={:?}",
                m.chat_id,
                m.id,
                m.text
                    .as_deref()
                    .unwrap_or("[media]")
                    .chars()
                    .take(40)
                    .collect::<String>()
            ),
            WaEvent::MessageConfirmed {
                tmp_id, real_id, ..
            } => format!("MessageConfirmed: {tmp_id} → {real_id}"),
            WaEvent::MessageFailed { msg_id, .. } => format!("MessageFailed: {msg_id}"),
            WaEvent::HistoryMessages {
                chat_id, messages, ..
            } => format!("HistoryMessages: chat={chat_id} count={}", messages.len()),
            WaEvent::TypingIndicator {
                chat_id, is_typing, ..
            } => format!("Typing: {chat_id} {is_typing}"),
            WaEvent::ReceiptUpdate { msg_id, .. } => format!("Receipt: {msg_id}"),
            WaEvent::ChatReadOnOtherDevice { chat_id } => format!("ReadOnOtherDevice: {chat_id}"),
            WaEvent::OwnProfile { .. } => "OwnProfile loaded".to_string(),
            WaEvent::PollVoteUpdate {
                poll_msg_id,
                all_votes,
                ..
            } => format!("PollVote: {} voters on {poll_msg_id}", all_votes.len()),
            WaEvent::SyncProgress(s) => format!("SyncProgress: {s}"),
            WaEvent::ChatNameUpdated { chat_id, name } => {
                format!("NameUpdated: {chat_id} → {name}")
            }
            WaEvent::ChatRowChanged(c) => format!(
                "ChatRowChanged: {} unread={} ts={} preview={:?}",
                c.id,
                c.unread_count,
                c.timestamp,
                c.last_message.chars().take(40).collect::<String>()
            ),
            WaEvent::MediaReady {
                msg_id, chat_id, ..
            } => format!("MediaReady: {msg_id} in {chat_id}"),
            WaEvent::AvatarReady { chat_id, .. } => format!("AvatarReady: {chat_id}"),
            WaEvent::ReactionUpdated {
                msg_id, reactions, ..
            } => {
                format!("Reaction: {msg_id} ({} total)", reactions.len())
            }
            WaEvent::MessageStarred {
                msg_id, starred, ..
            } => format!("Star: {msg_id} {starred}"),
            WaEvent::MessagePinned { msg_id, .. } => format!("Pin: {msg_id}"),
            WaEvent::MessageDeletedLocal { msg_id, .. } => format!("DeleteLocal: {msg_id}"),
            WaEvent::MessageEdited { msg_id, .. } => format!("MessageEdited: {msg_id}"),
            WaEvent::EditFailed { msg_id, .. } => format!("EditFailed: {msg_id}"),
            WaEvent::SenderNameResolved { sender_id, .. } => {
                format!("SenderNameResolved: {sender_id}")
            }
            WaEvent::ErrorToast(msg) => format!("ErrorToast: {msg}"),
            WaEvent::InfoToast(msg) => format!("InfoToast: {msg}"),
            WaEvent::UpdateReadyToast(msg) => format!("UpdateReady: {msg}"),
            WaEvent::TwoFactorCodeDetected { msg_id, .. } => {
                format!("TwoFactorCodeDetected({msg_id})")
            }
            WaEvent::GroupInviteLink { link, .. } => format!("InviteLink: {link}"),
            WaEvent::ForwardComplete { count, .. } => format!("Forwarded: {count} msgs"),
            WaEvent::ChatListForPicker(c) => format!("ChatListForPicker: {} chats", c.len()),
            WaEvent::ContactProfile { chat_id, .. } => format!("ContactProfile: {chat_id}"),
            WaEvent::GroupProfile {
                chat_id, subject, ..
            } => format!("GroupProfile: {chat_id} {subject}"),
            WaEvent::PhoneLookupResult {
                phone,
                is_registered,
                ..
            } => format!("PhoneLookup: {phone} registered={is_registered}"),
            _ => "Event".to_string(),
        };
        self.log_debug(&debug_line);

        match event {
            WaEvent::QrCode(qr) => {
                inner.login_screen.show_qr(&qr);
            }
            WaEvent::Connected { phone, name } => {
                log::info!("Connected as {} ({})", name, phone);
                inner.login_screen.show_status("Loading chats…");
                if !phone.is_empty() {
                    inner.chat_view.set_own_jid(phone.clone());
                    *inner.own_jid.borrow_mut() = phone.clone();
                    // Load own avatar on the icon rail
                    let safe = phone.replace(['/', '\\', '@', ':'], "_");
                    let path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
                    if path.exists() {
                        if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(&path, 128) {
                            inner.own_avatar.set_custom_image(Some(&tex));
                        }
                    }
                }
                if !name.is_empty() {
                    inner.chat_view.set_own_name(name.clone());
                    inner.own_avatar.set_text(Some(&name));
                }
                // Pre-fetch own profile data so it's ready when user opens profile
                inner
                    .bridge
                    .send_command(crate::bridge::WaCommand::GetOwnProfile);
            }
            WaEvent::Disconnected(reason) => {
                log::warn!("Disconnected: {}", reason);
                inner.stack.set_visible_child_name("login");
                inner
                    .login_screen
                    .show_status(&format!("Disconnected: {reason}"));
            }
            WaEvent::LoggedOut(reason) => {
                log::warn!("LoggedOut: {}", reason);
                inner.stack.set_visible_child_name("login");
                inner.login_screen.show_loading(&reason);
            }
            WaEvent::ChatsLoaded(mut chats) => {
                // Switch to main view now that we have data to show
                inner.stack.set_visible_child_name("main");

                // Load contact data ONCE (avoid duplicate disk reads)
                let contacts = crate::ui::runtime::load_contact_names();
                let lid_map = crate::ui::runtime::load_lid_phone_map();

                // Resolve unresolved chat names against ALL the contact
                // sources we have: WhatsApp's contact map (keyed by JID),
                // the LID→phone resolution map, and the cross-protocol
                // global directory. WhatsApp gives us anonymous LID JIDs
                // like `137340286709870@lid` for first-time-message
                // contacts; without LID→phone translation those rows
                // stay labeled with the raw LID forever.
                for c in &mut chats {
                    let needs_resolve = !c.id.ends_with("@g.us")
                        && (c.name.starts_with('+')
                            || c.name.contains("@lid")
                            || c.name.contains("@s.whatsapp.net")
                            || c.name.chars().all(|ch| !ch.is_alphabetic())
                            || c.name == c.id);
                    if !needs_resolve {
                        continue;
                    }
                    // 1. Direct lookup by chat_id (works for phone JIDs).
                    if let Some(name) = contacts.get(&c.id) {
                        c.name = name.clone();
                        continue;
                    }
                    // 2. LID → phone JID → contact name. Used when WhatsApp
                    //    hands us an anonymous LID for a saved contact.
                    if c.id.ends_with("@lid")
                        && let Some(phone_jid) = lid_map.get(&c.id)
                        && let Some(name) = contacts.get(phone_jid)
                    {
                        log::debug!(
                            "ChatsLoaded: resolved {} → {} via LID→phone map ({})",
                            c.id,
                            name,
                            phone_jid
                        );
                        c.name = name.clone();
                        continue;
                    }
                    // 3. Cross-protocol global directory (fuzzy phone match).
                    if let Some(name) = crate::contacts::global().lookup(&c.id) {
                        log::debug!(
                            "ChatsLoaded: resolved {} → {} via global directory",
                            c.id,
                            name
                        );
                        c.name = name;
                        continue;
                    }
                    // 4. For LIDs: also try fuzzy via the resolved phone.
                    if c.id.ends_with("@lid")
                        && let Some(phone_jid) = lid_map.get(&c.id)
                        && let Some(name) = crate::contacts::global().lookup(phone_jid)
                    {
                        log::debug!(
                            "ChatsLoaded: resolved {} → {} via LID→phone→global",
                            c.id,
                            name
                        );
                        c.name = name;
                    }
                }

                // Resolve @mentions in chat list previews
                for c in &mut chats {
                    if c.last_message.contains('@') {
                        for word in c.last_message.clone().split_whitespace() {
                            if word.starts_with('@') && word.len() > 4 {
                                let num = &word[1..];
                                if num.chars().all(|c| c.is_ascii_digit()) {
                                    let lid = format!("{num}@lid");
                                    let name = contacts
                                        .get(&lid)
                                        .or_else(|| lid_map.get(&lid).and_then(|p| contacts.get(p)))
                                        .or_else(|| contacts.get(&format!("{num}@s.whatsapp.net")));
                                    if let Some(n) = name {
                                        c.last_message =
                                            c.last_message.replace(word, &format!("@{n}"));
                                    }
                                }
                            }
                        }
                    }
                }

                // ── Dedup: same phone number = same person ──
                // Strips :device suffix and @domain, keeps the CLEAN JID
                // (no :device suffix) since that's where message history lives.
                {
                    let before = chats.len();
                    // First pass: for each phone, prefer the clean JID (no colon)
                    let mut best_idx: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    for (i, c) in chats.iter().enumerate() {
                        let phone = phone_number_from_jid(&c.id);
                        let local = c.id.split('@').next().unwrap_or("");
                        let has_device = local.contains(':');
                        match best_idx.get(&phone).copied() {
                            None => {
                                best_idx.insert(phone, i);
                            }
                            Some(prev) => {
                                let prev_local = chats[prev].id.split('@').next().unwrap_or("");
                                let prev_has_device = prev_local.contains(':');
                                // Prefer clean JID (no device suffix)
                                if prev_has_device && !has_device {
                                    log::info!(
                                        "Dedup: replacing {} with {} for phone {}",
                                        chats[prev].id,
                                        c.id,
                                        phone
                                    );
                                    best_idx.insert(phone, i);
                                } else if !prev_has_device && has_device {
                                    log::info!(
                                        "Dedup: dropping {} — clean {} already kept",
                                        c.id,
                                        chats[prev].id
                                    );
                                }
                            }
                        }
                    }
                    let keep: std::collections::HashSet<usize> =
                        best_idx.values().copied().collect();
                    let mut idx = 0;
                    chats.retain(|_| {
                        let kept = keep.contains(&idx);
                        idx += 1;
                        kept
                    });
                    if chats.len() < before {
                        log::info!("Dedup: removed {} duplicate(s)", before - chats.len());
                    }
                }

                // Cache chat list for multi-send
                *inner.cached_chats.borrow_mut() = chats.clone();
                for chat in &chats {
                    inner
                        .chat_view
                        .set_chat_favorite(&chat.id, chat.is_favorite);
                }

                // ListBox is not virtualized. Keep every lightweight summary
                // searchable, but let ChatListPanel materialize only its bounded
                // recent working set (plus pinned/favourite/search rows).
                inner.chat_list.load_catalog(&chats);

                // Re-fetch group names for any @g.us chat with a person-like name (max 5)
                {
                    let mut refetch_count = 0u32;
                    for c in &chats {
                        if refetch_count >= 5 {
                            break;
                        }
                        if c.id.ends_with("@g.us") {
                            let words: Vec<&str> = c.name.split_whitespace().collect();
                            let looks_like_person = words.len() <= 3
                                && !words.is_empty()
                                && words.iter().all(|w| {
                                    w.chars().next().map_or(false, |ch| ch.is_uppercase())
                                        && w.len() < 20
                                });
                            if looks_like_person {
                                inner
                                    .bridge
                                    .send_command(crate::bridge::WaCommand::GetGroupInfo {
                                        chat_id: c.id.clone(),
                                    });
                                refetch_count += 1;
                            }
                        }
                    }
                }

                populate_rail_favourites(
                    &inner.rail_favourites,
                    &chats,
                    &inner.chat_view,
                    &inner.bridge,
                    &inner.chat_list,
                    &inner.sidebar_stack,
                    &inner.profile_revealer,
                    &inner.gtk_app,
                );
                inner.new_chat_panel.load_contacts(&chats, &contacts);
            }
            WaEvent::ChatAdded(chat) => {
                // When a phone JID chat arrives, remove any @lid duplicate for
                // the same person (same name, one is @lid, one is @s.whatsapp.net)
                if chat.id.ends_with("@s.whatsapp.net") {
                    inner.chat_list.remove_lid_duplicate(&chat.name);
                }
                // If this incoming chat is pinned/favourite, the rail needs
                // to pick it up. A debounced refresh handles bursts of adds
                // during initial sync without hammering load_chats().
                let needs_rail_refresh = chat.is_pinned || chat.is_favorite;
                inner
                    .chat_view
                    .set_chat_favorite(&chat.id, chat.is_favorite);
                inner.chat_list.add_chat(chat);
                if needs_rail_refresh {
                    let rail = inner.rail_favourites.clone();
                    let cv = inner.chat_view.clone();
                    let br = inner.bridge.clone();
                    let list = inner.chat_list.clone();
                    let stack = inner.sidebar_stack.clone();
                    let profile = inner.profile_revealer.clone();
                    let app = inner.gtk_app.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(800),
                        move || {
                            let chats = crate::ui::runtime::load_chats();
                            populate_rail_favourites(
                                &rail, &chats, &cv, &br, &list, &stack, &profile, &app,
                            );
                        },
                    );
                }
            }
            WaEvent::ChatRowChanged(chat) => {
                // Authoritative row refresh from the owning runtime. apply_summary
                // is the SOLE row-state writer — it renders this verbatim. Note:
                // deliberately NO remove_lid_duplicate / rail refresh here (that's
                // why ChatAdded is not reused — its handler has per-chat side
                // effects that must not fire on every message).
                inner
                    .chat_view
                    .set_chat_favorite(&chat.id, chat.is_favorite);
                inner.chat_list.apply_summary(&chat);
            }
            WaEvent::MessageReceived(msg) => {
                let current_chat = inner.chat_view.current_chat_id();
                let is_current_chat = current_chat.as_deref() == Some(&msg.chat_id);
                // If this is a new group chat, fetch group name so it displays correctly
                if msg.chat_id.ends_with("@g.us") && !inner.chat_list.has_chat(&msg.chat_id) {
                    inner
                        .bridge
                        .send_command(crate::bridge::WaCommand::GetGroupInfo {
                            chat_id: msg.chat_id.clone(),
                        });
                }

                // Clear typing indicator for this sender — they sent a message,
                // so they're no longer typing. Do this BEFORE appending so the
                // dots disappear at the same time the message appears.
                if !msg.is_from_me {
                    inner
                        .chat_view
                        .set_typing_indicator(&msg.chat_id, &msg.sender_name, false);
                    inner
                        .chat_list
                        .set_typing(&msg.chat_id, &msg.sender_name, false);
                }

                // Append to chat view FIRST so the message body appears before
                // the chat list preview updates (fixes visual race condition).
                inner.chat_view.append_message((*msg).clone());

                // Feed the stealth-peek cache and clear any typing overlay. The
                // row's preview/time/unread/sort are NOT touched here — the
                // owning runtime emits ChatRowChanged (adjacent on the same FIFO
                // channel) as the sole row writer. For merged SMS that event may
                // even arrive AFTER this MessageReceived (command-channel detour),
                // so nothing here may assume row freshness.
                inner.chat_list.note_recent_message(&msg.chat_id, &msg);
                // If viewing this chat OR chat has auto-mark-read enabled, mark as read.
                // The is_current_chat arm is gated on window focus: a chat that is
                // "current" but sitting in the tray (or behind another window) must
                // NOT fire a read receipt for a message the user never saw. auto_mark
                // is an explicit per-chat opt-in and stays unconditional.
                let auto_mark = inner.chat_list.is_auto_mark_read(&msg.chat_id);
                if ((is_current_chat && inner.window.is_active()) || auto_mark) && !msg.is_from_me {
                    inner
                        .bridge
                        .send_command(crate::bridge::WaCommand::MarkRead {
                            chat_id: msg.chat_id.clone(),
                        });
                }
                // Send desktop notification if appropriate.
                // Skip notifications for old messages (e.g. history sync, offline
                // catch-up) — only notify for messages less than 60 seconds old.
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                // Some cached gm chat files were saved in milliseconds before
                // the unit fix landed — accept either by detecting magnitude.
                let msg_ts_secs = if msg.timestamp > 10_000_000_000 {
                    msg.timestamp / 1000
                } else {
                    msg.timestamp
                };
                let is_recent = (now_secs - msg_ts_secs).abs() < 60;
                // Diag log: every incoming MessageReceived for any chat. Helps
                // pinpoint why notifications/sound/2FA weren't firing.
                let is_gm = crate::bridge::MessageSource::from_message(&msg.chat_id, &msg.id)
                    == crate::bridge::MessageSource::GoogleMessages;
                log::debug!(
                    "MessageReceived: chat={} id={} from_me={} ts={} now={} is_recent={} is_gm={} text={:?}",
                    msg.chat_id,
                    msg.id,
                    msg.is_from_me,
                    msg.timestamp,
                    now_secs,
                    is_recent,
                    is_gm,
                    msg.text
                        .as_deref()
                        .map(|t| t.chars().take(40).collect::<String>()),
                );
                // Auto-detect 2FA codes in incoming SMS/RCS messages and pop
                // them into the clipboard with an OSD-style notification. Gated on
                // `is_gm` — a WhatsApp message containing a "code"/"pin" keyword
                // must NOT silently overwrite the clipboard.
                // Relay delivery of an SMS can lag minutes behind its timestamp, and a
                // stale OTP is still worth copying, so 2FA uses a wider window than
                // notifications.
                let is_recent_for_2fa = (now_secs - msg_ts_secs).abs() < 600;
                let mut handled_as_2fa = false;
                if !msg.is_from_me
                    && is_recent_for_2fa
                    && is_gm
                    && let Some(text) = msg.text.as_deref()
                {
                    if !is_recent {
                        log::info!(
                            "2FA scan: message is {}s old (outside the notification window)",
                            now_secs - msg_ts_secs
                        );
                    }
                    if let Some(code) = detect_two_factor_code(text) {
                        if !inner.settings.twofa_autocopy_enabled() {
                            log::info!("2FA code detected but auto-copy is disabled in settings");
                        }
                        if inner.settings.twofa_autocopy_enabled() && twofa_mark_copied(&msg.id) {
                            let sender = inner
                                .chat_list
                                .chat_name(&msg.chat_id)
                                .unwrap_or_else(|| msg.sender_name.clone());
                            copy_2fa_code_with_osd(&inner.gtk_app, &code, &sender);
                            handled_as_2fa = true;
                        }
                    } else {
                        log::debug!("2FA scan: no code detected in text");
                    }
                }
                let muted = inner.chat_list.is_chat_muted(&msg.chat_id);
                // A 2FA OSD already surfaced this OTP; don't also fire a normal
                // banner + sound for it. Muted chats suppress both entirely.
                if !msg.is_from_me && is_recent && !handled_as_2fa && !muted {
                    let is_active = inner.window.is_active();
                    if inner.settings.should_notify(is_active, is_current_chat) {
                        let chat_name = inner
                            .chat_list
                            .chat_name(&msg.chat_id)
                            .unwrap_or_else(|| msg.sender_name.clone());
                        let title = if msg.chat_id.ends_with("@g.us") {
                            format!("{}: {}", msg.sender_name, chat_name)
                        } else {
                            chat_name
                        };
                        let body = if inner.settings.show_preview() {
                            msg.text
                                .as_deref()
                                .or(msg.media_caption.as_deref())
                                .unwrap_or(match &msg.media_type {
                                    Some(crate::bridge::MediaType::Image) => "📷 Photo",
                                    Some(crate::bridge::MediaType::Video) => "🎥 Video",
                                    Some(crate::bridge::MediaType::Audio) => "🎵 Audio",
                                    Some(crate::bridge::MediaType::Document) => "📄 Document",
                                    Some(crate::bridge::MediaType::Sticker) => "🎭 Sticker",
                                    Some(crate::bridge::MediaType::Gif) => "🎞 GIF",
                                    None => "New message",
                                })
                                .to_string()
                        } else {
                            "New message".to_string()
                        };

                        send_desktop_notification(&inner.gtk_app, &msg.chat_id, &title, &body);
                    }
                    // Play notification sound. Suppress only when the user is
                    // actively looking at this chat (window focused AND current
                    // chat); a message for the open chat while the app is in the
                    // tray should still chime.
                    if inner.settings.should_play_sound() && !(is_current_chat && is_active) {
                        play_notification_sound();
                    }
                }
            }
            WaEvent::MessageConfirmed {
                tmp_id,
                real_id,
                chat_id: _,
            } => {
                // Re-key the optimistic bubble only. The row no longer bumps here:
                // bump_chat_to_top used a UI clock (SystemTime::now) that could
                // out-race the send's real timestamp. The runtime now moves the
                // row via ChatRowChanged (persist_new_message → emit_row) using
                // the single post-send timestamp — the only clock that reaches
                // the row.
                inner.chat_view.confirm_bubble(&tmp_id, &real_id);
            }
            WaEvent::MessageFailed { msg_id, chat_id: _ } => {
                inner
                    .chat_view
                    .update_receipt(&msg_id, crate::bridge::ReceiptStatus::Failed);
            }
            WaEvent::HistoryMessages {
                chat_id,
                chat_name: _,
                messages,
            } => {
                inner.chat_view.load_history(&chat_id, messages);
            }
            WaEvent::OlderMessages {
                chat_id,
                messages,
                has_more,
            } => {
                inner.chat_view.prepend_older(&chat_id, messages, has_more);
            }
            WaEvent::TypingIndicator {
                chat_id,
                sender_name,
                is_typing,
            } => {
                inner
                    .chat_view
                    .set_typing_indicator(&chat_id, &sender_name, is_typing);
                inner
                    .chat_list
                    .set_typing(&chat_id, &sender_name, is_typing);
                // Self-heal + cross-protocol: if this is a 1-on-1 chat
                // whose row title is STILL a bare phone/JID, the typer's
                // resolved name IS the chat name — retitle the row. We
                // explicitly do NOT overwrite a real-looking name with a
                // typing event's sender_name, because typing events often
                // carry just the first name or even an initial (push_name
                // not fully resolved yet), which previously regressed
                // chats from "Saad Suleman" to "S".
                let is_one_on_one = chat_id.ends_with("@s.whatsapp.net")
                    || chat_id.ends_with("@lid")
                    || chat_id.starts_with("gm:");
                if is_one_on_one
                    && !sender_name.is_empty()
                    && sender_name.chars().any(|c| c.is_alphabetic())
                    && !sender_name.starts_with('+')
                {
                    let current = inner.chat_list.chat_name(&chat_id).unwrap_or_default();
                    let current_looks_unresolved = current.is_empty()
                        || current.starts_with('+')
                        || current.chars().all(|c| !c.is_alphabetic())
                        || current == chat_id;
                    if current_looks_unresolved {
                        // Only NOW retitle — and even then, feed the global
                        // directory regardless so cross-protocol lookups
                        // can use it.
                        crate::contacts::global().insert(&chat_id, &sender_name, "typing");
                        inner.chat_list.update_chat_name(&chat_id, &sender_name);
                    }
                    // Always feed the directory; the insert() helper itself
                    // refuses to downgrade an alphabetic name with a worse
                    // alphabetic name based on length and recency, so this
                    // is safe.
                }
            }
            WaEvent::ReceiptUpdate { msg_id, status } => {
                inner.chat_view.update_receipt(&msg_id, status);
            }
            WaEvent::ChatReadOnOtherDevice { chat_id } => {
                log::info!("UI: Clearing unread badge for {chat_id}");
                inner.chat_list.reset_unread(&chat_id);
                withdraw_chat_notification(&inner.gtk_app, &chat_id);
            }
            WaEvent::SyncProgress(syncing) => {
                // Reference-count concurrent syncs so one producer's `false`
                // can't hide the bar while another sync is still running.
                let prev = inner.sync_count.get();
                let count = if syncing {
                    prev.saturating_add(1)
                } else {
                    prev.saturating_sub(1)
                };
                inner.sync_count.set(count);
                inner.sync_revealer.set_reveal_child(count > 0);
                // Only kick off the pulse loop + safety timeout on the leading
                // edge (0 -> 1), not on every nested SyncProgress(true).
                if syncing && prev == 0 {
                    // Start pulsing the progress bar every 150ms.
                    // GTK ProgressBar::pulse() is lightweight and doesn't freeze
                    // like a Spinner during heavy widget work.
                    let pb = inner.sync_progress.clone();
                    let rev = inner.sync_revealer.clone();
                    pb.pulse();
                    glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
                        if rev.reveals_child() {
                            pb.pulse();
                            glib::ControlFlow::Continue
                        } else {
                            glib::ControlFlow::Break
                        }
                    });
                    // Safety timeout: force-hide after 60s even if a matching
                    // SyncProgress(false) never arrives. Reset the counter too so
                    // a stale count can't wedge the bar open on the next sync.
                    let rev2 = inner.sync_revealer.clone();
                    let inner2 = inner.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(60), move || {
                        inner2.sync_count.set(0);
                        rev2.set_reveal_child(false);
                    });
                }
            }
            WaEvent::ChatNameUpdated { chat_id, name } => {
                // Authoritative: gm conversation refresh / WA contact sync /
                // user rename. Bypasses the downgrade-refusal heuristic that
                // protects against speculative typing-event renames, so a
                // chat that got mis-titled "Jake Steinman" from a sender
                // lookup on the user's own SMS can still be retitled
                // "Clayton" when the actual conversation name comes through.
                inner
                    .chat_list
                    .update_chat_name_authoritative(&chat_id, &name);
                inner.chat_view.update_chat_name(&chat_id, &name);
                inner.profile_panel.update_name(&chat_id, &name);
            }
            WaEvent::MediaReady {
                msg_id,
                chat_id,
                path,
                media_type,
            } => {
                inner
                    .chat_view
                    .set_media_loaded(&msg_id, &chat_id, &path, &media_type);
            }
            WaEvent::LinkPreviewReady {
                chat_id,
                msg_id,
                url,
                title,
                description,
                thumbnail_url,
            } => {
                inner.chat_view.set_link_preview(
                    &chat_id,
                    &msg_id,
                    &url,
                    title.as_deref(),
                    description.as_deref(),
                    thumbnail_url.as_deref(),
                );
            }
            WaEvent::AvatarReady { chat_id, path } => {
                inner.chat_list.set_avatar(&chat_id, &path);
            }
            WaEvent::ChatArchived { chat_id, archived } => {
                inner.chat_list.set_chat_archived(&chat_id, archived);
            }
            WaEvent::ChatMuted { chat_id, muted } => {
                inner.chat_list.set_chat_muted(&chat_id, muted);
            }
            WaEvent::ChatPinned { chat_id, pinned } => {
                inner.chat_list.set_chat_pinned(&chat_id, pinned);
                // Refresh rail after short delay (wait for disk flush)
                let rail = inner.rail_favourites.clone();
                let cv = inner.chat_view.clone();
                let br = inner.bridge.clone();
                let list = inner.chat_list.clone();
                let stack = inner.sidebar_stack.clone();
                let profile = inner.profile_revealer.clone();
                let app = inner.gtk_app.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    let chats = crate::ui::runtime::load_chats();
                    populate_rail_favourites(
                        &rail, &chats, &cv, &br, &list, &stack, &profile, &app,
                    );
                });
            }
            WaEvent::ChatFavorited { chat_id, favorite } => {
                inner.chat_list.set_chat_favorite(&chat_id, favorite);
                inner.chat_view.set_chat_favorite(&chat_id, favorite);
                let rail = inner.rail_favourites.clone();
                let cv = inner.chat_view.clone();
                let br = inner.bridge.clone();
                let list = inner.chat_list.clone();
                let stack = inner.sidebar_stack.clone();
                let profile = inner.profile_revealer.clone();
                let app = inner.gtk_app.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    let chats = crate::ui::runtime::load_chats();
                    populate_rail_favourites(
                        &rail, &chats, &cv, &br, &list, &stack, &profile, &app,
                    );
                });
            }
            WaEvent::ChatDeleted { chat_id } => {
                inner.chat_list.remove_chat(&chat_id);
            }
            WaEvent::ChatCleared { chat_id } => {
                // Row preview is now cleared by the runtime's ClearChat handler
                // (set_chat_preview "" with the timestamp preserved → emit_row →
                // ChatRowChanged), which also fixes the old epoch-date render.
                // Here we only wipe the open conversation view.
                inner.chat_view.clear_chat(&chat_id);
            }
            WaEvent::ChatLabeled { chat_id, label } => {
                inner.chat_list.set_chat_label(&chat_id, label.as_deref());
            }
            // Message action events — handled by chat_view
            WaEvent::ReactionUpdated {
                chat_id,
                msg_id,
                reactions,
                is_latest: _,
            } => {
                // Rebuild the reaction row on the bubble from the full set.
                // The "Reacted 👍" sidebar preview is now emitted by the runtime
                // as an EPHEMERAL ChatRowChanged (render-don't-persist, so a
                // restart shows the underlying message again) — see A6. The row
                // is no longer written here.
                inner.chat_view.show_reaction(&chat_id, &msg_id, &reactions);
            }
            WaEvent::MessageStarred { starred, .. } => {
                inner.toast_overlay.add_toast(adw::Toast::new(if starred {
                    "Message starred"
                } else {
                    "Message unstarred"
                }));
            }
            WaEvent::MessagePinned { chat_id, msg_id } => {
                inner.chat_view.show_pinned_banner(&chat_id, &msg_id);
            }
            WaEvent::MessageDeletedLocal {
                chat_id,
                msg_id,
                new_preview: _,
            } => {
                // Remove the bubble only. The sidebar preview (if the deleted
                // message was the latest) now updates via ChatRowChanged: the
                // runtime's delete handlers call set_chat_preview (timestamp
                // preserved) → emit_row. `new_preview` is retired by A/cleanup.
                inner.chat_view.remove_message(&chat_id, &msg_id);
            }
            WaEvent::MessageEdited {
                chat_id,
                msg_id,
                new_text,
                is_latest: _,
            } => {
                // Update the bubble only. The sidebar preview (when the edited
                // message is the chat's latest) now refreshes via ChatRowChanged:
                // the runtime's edit handler calls set_chat_preview with the
                // timestamp UNCHANGED, so the preview text changes without
                // reordering the row — the desired edit semantics.
                inner
                    .chat_view
                    .update_message_text(&chat_id, &msg_id, &new_text, true);
            }
            WaEvent::EditFailed {
                chat_id,
                msg_id,
                new_text,
            } => {
                inner
                    .chat_view
                    .restore_failed_edit(&chat_id, &msg_id, &new_text);
            }
            WaEvent::SenderNameResolved {
                chat_id,
                sender_id,
                name,
            } => {
                inner
                    .chat_view
                    .refresh_sender_name(&chat_id, &sender_id, &name);
            }
            WaEvent::ErrorToast(msg) => {
                log::warn!("Error toast: {msg}");
                show_toast_deduped(inner, &msg);
            }
            WaEvent::InfoToast(msg) => {
                show_toast_deduped(inner, &msg);
            }
            WaEvent::TwoFactorCodeDetected {
                msg_id,
                code,
                sender,
            } => {
                if !inner.settings.twofa_autocopy_enabled() {
                    log::info!("2FA code detected but auto-copy is disabled in settings");
                } else if twofa_mark_copied(&msg_id) {
                    let sender = inner.chat_list.chat_name(&sender).unwrap_or(sender);
                    copy_2fa_code_with_osd(&inner.gtk_app, &code, &sender);
                }
            }
            WaEvent::UpdateReadyToast(msg) => {
                let toast = adw::Toast::new(&msg);
                toast.set_button_label(Some("Restart now"));
                toast.set_timeout(0); // stays until acted on or dismissed
                let overlay = inner.toast_overlay.clone();
                toast.connect_button_clicked(move |_| {
                    if let Err(error) = crate::updater::restart_to_apply() {
                        overlay.add_toast(adw::Toast::new(&format!("Could not restart: {error}")));
                    }
                });
                inner.toast_overlay.add_toast(toast);
            }
            WaEvent::ForwardComplete { to_chat_id, count } => {
                log::info!("Forwarded {count} messages to {to_chat_id}");
            }
            WaEvent::ChatListForPicker(chats) => {
                inner.chat_view.show_forward_picker(chats);
            }
            WaEvent::GroupMembers { chat_id, members } => {
                inner.chat_view.set_group_members(&chat_id, members);
            }
            WaEvent::QuickRepliesSynced { replies } => {
                inner.chat_view.set_quick_replies(replies);
            }
            WaEvent::ContactProfile {
                chat_id,
                phone,
                about,
                avatar_path,
            } => {
                inner.profile_panel.set_contact_profile(
                    &chat_id,
                    &phone,
                    about.as_deref(),
                    avatar_path.as_deref(),
                );
            }
            WaEvent::GroupsInCommon { chat_id, groups } => {
                inner.profile_panel.set_groups_in_common(&chat_id, &groups);
            }
            WaEvent::GroupProfile {
                chat_id,
                subject,
                description,
                participants,
                i_am_admin,
                ..
            } => {
                inner.profile_panel.set_group_profile(
                    &chat_id,
                    &subject,
                    description.as_deref(),
                    &participants,
                    i_am_admin,
                );
                // Also update header subtitle with participant names
                inner
                    .chat_view
                    .set_group_members(&chat_id, participants.clone());
            }
            WaEvent::GroupInviteLink { chat_id, link } => {
                inner.profile_panel.set_invite_link(&chat_id, &link);
            }
            WaEvent::GifResults {
                request_id,
                query,
                gifs,
                error,
            } => {
                inner
                    .chat_view
                    .show_gif_results(request_id, &query, gifs, error.as_deref());
            }
            WaEvent::StickerResults {
                request_id,
                query,
                stickers,
                error,
            } => {
                inner.chat_view.show_sticker_results(
                    request_id,
                    &query,
                    stickers,
                    error.as_deref(),
                );
            }
            WaEvent::OwnProfile {
                name,
                about,
                description,
                email,
                website,
                address,
                category,
            } => {
                inner.profile_panel.set_own_profile(
                    &name,
                    &about,
                    &description,
                    &email,
                    &website,
                    &address,
                    &category,
                );
                *inner.own_profile_data.borrow_mut() = Some(OwnProfileData {
                    name,
                    about,
                    description,
                    email,
                    website,
                    address,
                    category,
                });
            }
            WaEvent::PollVoteUpdate {
                chat_id,
                poll_msg_id,
                all_votes,
            } => {
                inner
                    .chat_view
                    .update_poll_votes(&chat_id, &poll_msg_id, &all_votes);
            }
            WaEvent::PhoneLookupResult {
                phone,
                jid,
                is_registered,
            } => {
                inner
                    .new_chat_panel
                    .set_phone_result(&phone, jid.as_deref(), is_registered);
            }
            // ── Global search results ────────────────────────────────────
            WaEvent::GlobalSearchResults { query, results } => {
                show_global_search_results(
                    &inner.window,
                    &inner.bridge,
                    &inner.chat_view,
                    &inner.chat_list,
                    &inner.sidebar_stack,
                    &inner.profile_revealer,
                    &inner.gtk_app,
                    &query,
                    results,
                );
            }
            // ── Calls ────────────────────────────────────────────────────
            WaEvent::IncomingCall {
                chat_id,
                caller_name,
                is_video,
            } => {
                let kind = if is_video { "video" } else { "voice" };
                show_incoming_call_dialog(
                    &inner.window,
                    &inner.bridge,
                    &chat_id,
                    &caller_name,
                    is_video,
                );
                log::info!("Incoming {kind} call from {caller_name} ({chat_id})");
            }
            WaEvent::CallEnded { chat_id, reason } => {
                log::info!("Call ended: {chat_id} — {reason}");
            }
            WaEvent::CallAccepted { chat_id } => {
                log::info!("Call accepted: {chat_id}");
            }
            // ── Multi-send progress / complete ───────────────────────────
            WaEvent::MultiSendProgress {
                sent,
                total,
                current_chat,
            } => {
                log::info!("MultiSend progress: {sent}/{total} (sending to {current_chat})");
            }
            WaEvent::MultiSendComplete { sent, failed } => {
                let msg = if failed == 0 {
                    format!("Sent to all {sent} chats")
                } else {
                    format!("Sent to {sent} chats, {failed} failed")
                };
                let toast = adw::Toast::new(&msg);
                toast.set_timeout(3);
                inner.toast_overlay.add_toast(toast);
                log::info!("MultiSend complete: sent={sent} failed={failed}");
            }
        }
    }
}

const TOAST_DEDUP_WINDOW: Duration = Duration::from_secs(5);

/// Add a toast, suppressing a rapid duplicate without permanently silencing a
/// recurring error. A reconnect loop can emit the same text many times per
/// second, while a later occurrence is still useful feedback.
fn show_toast_deduped(inner: &MainWindowInner, msg: &str) {
    let now = Instant::now();
    if !toast_should_show(inner.last_toast.borrow().as_ref(), msg, now) {
        return;
    }
    *inner.last_toast.borrow_mut() = Some((msg.to_string(), now));
    inner.toast_overlay.add_toast(adw::Toast::new(msg));
}

fn toast_should_show(last: Option<&(String, Instant)>, msg: &str, now: Instant) -> bool {
    !matches!(
        last,
        Some((last_msg, shown_at))
            if last_msg == msg && now.saturating_duration_since(*shown_at) < TOAST_DEDUP_WINDOW
    )
}

/// Single navigation path used by the sidebar, search, notification actions,
/// profile links and favourites rail. Keeping these commands together avoids
/// a visually-open chat remaining inactive in the runtime.
fn activate_chat(chat_view: &ChatViewPanel, bridge: &Arc<Bridge>, chat_id: &str, chat_name: &str) {
    let needs_load = chat_view.open_chat(chat_id.to_string(), chat_name);
    if needs_load {
        bridge.send_command(WaCommand::LoadChat {
            chat_id: chat_id.to_string(),
            chat_name: chat_name.to_string(),
        });
    }
    bridge.send_command(WaCommand::SetActiveChat {
        chat_id: Some(chat_id.to_string()),
    });
    bridge.send_command(WaCommand::MarkRead {
        chat_id: chat_id.to_string(),
    });
}

/// Extract just the phone number from a JID for dedup comparison.
/// "19056016557@s.whatsapp.net" → "19056016557"
/// "19056016557:20@s.whatsapp.net" → "19056016557"
/// "19056016557@lid" → "19056016557"
/// Group JIDs are returned as-is (no dedup needed).
fn phone_number_from_jid(jid: &str) -> String {
    if jid.ends_with("@g.us") {
        return jid.to_string(); // groups — no dedup
    }
    let local = jid.split('@').next().unwrap_or(jid);
    // Strip :device suffix
    local.split(':').next().unwrap_or(local).to_string()
}

const RAIL_ORDER_FILE: &str = "wa_rail_order.json";

fn load_rail_order() -> Vec<String> {
    std::fs::read_to_string(RAIL_ORDER_FILE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_rail_order(order: &[String]) {
    if let Ok(data) = serde_json::to_string(order) {
        let _ = std::fs::write(RAIL_ORDER_FILE, data);
    }
}

/// Populate the icon rail with pinned/favourite chat avatars.
/// Respects user-defined drag order (persisted in wa_rail_order.json).
fn populate_rail_favourites(
    container: &Box,
    chats: &[crate::bridge::ChatSummary],
    chat_view: &crate::ui::chat_view::ChatViewPanel,
    bridge: &Arc<crate::bridge::Bridge>,
    chat_list: &ChatListPanel,
    sidebar_stack: &Stack,
    profile_revealer: &Revealer,
    app: &adw::Application,
) {
    // Clear existing
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    // Collect pinned + favourite chats (max 10)
    let fav_set: Vec<&crate::bridge::ChatSummary> = chats
        .iter()
        .filter(|c| c.is_pinned || c.is_favorite)
        .take(10)
        .collect();

    // Sort by persisted order, then by timestamp for any new additions
    let saved_order = load_rail_order();
    let mut ordered: Vec<&crate::bridge::ChatSummary> = Vec::new();
    // First: items in saved order
    for id in &saved_order {
        if let Some(c) = fav_set.iter().find(|c| c.id == *id) {
            ordered.push(c);
        }
    }
    // Then: any new favourites not in the saved order
    for c in &fav_set {
        if !saved_order.contains(&c.id) {
            ordered.push(c);
        }
    }

    // Save the current order (captures any new additions)
    let current_order: Vec<String> = ordered.iter().map(|c| c.id.clone()).collect();
    save_rail_order(&current_order);

    // Shared order state for drag-reorder
    let order_rc: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(current_order));

    for (pos, chat) in ordered.iter().enumerate() {
        let overlay = gtk4::Overlay::new();
        overlay.set_widget_name(&chat.id);

        let av = libadwaita::Avatar::new(40, Some(&chat.name), true);
        av.set_cursor_from_name(Some("pointer"));
        av.set_tooltip_text(Some(&chat.name));

        // Load cached avatar
        let safe = chat.id.replace(['/', '\\', '@', ':'], "_");
        let avatar_path = std::path::PathBuf::from("wa_avatars").join(format!("{safe}.jpg"));
        if avatar_path.exists() {
            if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(&avatar_path, 96) {
                av.set_custom_image(Some(&tex));
            }
        }

        let open_btn = Button::new();
        open_btn.add_css_class("flat");
        open_btn.add_css_class("circular");
        open_btn.set_tooltip_text(Some(&format!("Open {}", chat.name)));
        open_btn.set_child(Some(&av));
        overlay.set_child(Some(&open_btn));

        // Unread badge
        if chat.unread_count > 0 {
            let badge = gtk4::Label::new(Some(&chat.unread_count.to_string()));
            badge.add_css_class("badge");
            badge.add_css_class("success");
            badge.set_halign(gtk4::Align::End);
            badge.set_valign(gtk4::Align::Start);
            overlay.add_overlay(&badge);
        }

        // Click to open this chat
        let chat_id = chat.id.clone();
        let chat_name = chat.name.clone();
        let cv = chat_view.clone();
        let br = bridge.clone();
        let list = chat_list.clone();
        let stack = sidebar_stack.clone();
        let profile = profile_revealer.clone();
        let app = app.clone();
        open_btn.connect_clicked(move |_| {
            activate_chat(&cv, &br, &chat_id, &chat_name);
            stack.set_visible_child_name("chats");
            list.select_chat(&chat_id);
            profile.set_reveal_child(false);
            withdraw_chat_notification(&app, &chat_id);
        });

        // Drag source — provides the chat_id as string content
        let drag_source = gtk4::DragSource::new();
        drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
        let chat_id_for_drag = chat.id.clone();
        drag_source.connect_prepare(move |_src, _x, _y| {
            Some(gtk4::gdk::ContentProvider::for_value(
                &chat_id_for_drag.to_value(),
            ))
        });
        overlay.add_controller(drag_source);

        // Drop target — accepts reorder drops
        let drop_target = gtk4::DropTarget::new(glib::Type::STRING, gtk4::gdk::DragAction::MOVE);
        let container_c = container.clone();
        let order_c = order_rc.clone();
        let drop_chat_id = chat.id.clone();
        drop_target.connect_drop(move |_dt, value, _x, _y| {
            let Ok(dragged_id) = value.get::<String>() else {
                return false;
            };
            if dragged_id == drop_chat_id {
                return false;
            }

            let mut order = order_c.borrow_mut();
            let Some(from) = order.iter().position(|id| *id == dragged_id) else {
                return false;
            };
            let Some(to) = order.iter().position(|id| *id == drop_chat_id) else {
                return false;
            };

            // Move the dragged item to the drop position
            let item = order.remove(from);
            order.insert(to, item);
            save_rail_order(&order);

            // Reorder the GTK children to match
            // Collect all children, remove them, re-append in new order
            let mut children: Vec<(String, gtk4::Widget)> = Vec::new();
            let mut child = container_c.first_child();
            while let Some(c) = child {
                let next = c.next_sibling();
                let name = c.widget_name().to_string();
                container_c.remove(&c);
                children.push((name, c));
                child = next;
            }
            for id in order.iter() {
                if let Some(pos) = children.iter().position(|(n, _)| n == id) {
                    let (_, widget) = children.remove(pos);
                    container_c.append(&widget);
                }
            }
            // Append any remaining (shouldn't happen, but safety)
            for (_, widget) in children {
                container_c.append(&widget);
            }

            true
        });
        overlay.add_controller(drop_target);

        container.append(&overlay);
    }
}

/// Open a full-window overlay for editing the user's own business profile.
fn open_own_profile_window(
    parent: Option<&gtk4::Window>,
    bridge: &Arc<crate::bridge::Bridge>,
    display_name: &str,
    data: Option<&OwnProfileData>,
    own_jid: &str,
) {
    use gtk4::{Align, Entry, Label, Orientation, ScrolledWindow};

    let window = gtk4::Window::builder()
        .title("Your Profile")
        .default_width(520)
        .default_height(750)
        .modal(true)
        .build();
    // A modal with no transient parent is a Wayland/GTK pitfall (mis-parenting,
    // weak modality, poor placement) — anchor it to the main window.
    window.set_transient_for(parent);

    // Escape closes, matching the app's other dialogs (chat_view.rs).
    let key = gtk4::EventControllerKey::new();
    let win_weak = window.downgrade();
    key.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk4::gdk::Key::Escape {
            if let Some(window) = win_weak.upgrade() {
                window.close();
            }
            gtk4::glib::Propagation::Stop
        } else {
            gtk4::glib::Propagation::Proceed
        }
    });
    window.add_controller(key);

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);

    let content = Box::new(Orientation::Vertical, 12);
    content.set_margin_start(32);
    content.set_margin_end(32);
    content.set_margin_top(16);
    content.set_margin_bottom(24);

    // Avatar (large, centered)
    let avatar = libadwaita::Avatar::new(96, Some(display_name), true);
    avatar.set_halign(Align::Center);
    avatar.set_margin_bottom(8);
    // Load own avatar from disk — use the own JID phone number dynamically
    let own_phone = own_jid
        .split('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if !own_phone.is_empty() {
        for entry in std::fs::read_dir("wa_avatars")
            .into_iter()
            .flatten()
            .flatten()
        {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(own_phone) && fname.ends_with(".jpg") {
                if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(entry.path(), 192) {
                    avatar.set_custom_image(Some(&tex));
                }
                break;
            }
        }
    }
    content.append(&avatar);

    let change_photo_btn = Button::with_label("Change Profile Photo");
    change_photo_btn.add_css_class("flat");
    change_photo_btn.add_css_class("accent");
    change_photo_btn.set_halign(Align::Center);
    change_photo_btn.set_margin_bottom(8);
    {
        let br = bridge.clone();
        let av = avatar.clone();
        let window_weak = window.downgrade();
        change_photo_btn.connect_clicked(move |_| {
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let dialog = gtk4::FileDialog::builder()
                .title("Choose Profile Photo")
                .build();
            let filter = gtk4::FileFilter::new();
            filter.add_mime_type("image/jpeg");
            filter.add_mime_type("image/png");
            filter.add_mime_type("image/webp");
            filter.set_name(Some("Images"));
            let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
            filters.append(&filter);
            dialog.set_filters(Some(&filters));
            let br2 = br.clone();
            let av2 = av.clone();
            dialog.open(Some(&window), gtk4::gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        let path_str = path.to_string_lossy().to_string();
                        // Update avatar preview immediately. Bypass the cache here
                        // because the user just picked a fresh file — we want the
                        // exact bytes they chose, not whatever happened to share
                        // this path's mtime in the cache. invalidate() ensures
                        // the next call from elsewhere reloads too.
                        crate::ui::texture_cache::invalidate(&path);
                        if let Some(tex) = crate::ui::texture_cache::texture_thumbnail(&path, 192) {
                            av2.set_custom_image(Some(&tex));
                        }
                        // Send to WhatsApp
                        br2.send_command(WaCommand::SetProfilePicture { path: path_str });
                    }
                }
            });
        });
    }
    content.append(&change_photo_btn);

    let sep1 = gtk4::Separator::new(Orientation::Horizontal);
    sep1.set_margin_top(4);
    sep1.set_margin_bottom(4);
    content.append(&sep1);

    // Helper for labeled fields
    let make_row = |parent: &Box, label: &str, placeholder: &str| -> Entry {
        let lbl = Label::new(Some(label));
        lbl.add_css_class("dim-label");
        lbl.add_css_class("caption");
        lbl.set_halign(Align::Start);
        parent.append(&lbl);
        let entry = Entry::new();
        entry.set_placeholder_text(Some(placeholder));
        parent.append(&entry);
        entry
    };

    let name_entry = make_row(&content, "Display Name", "Your name");
    name_entry.set_text(if let Some(d) = &data {
        &d.name
    } else {
        display_name
    });

    let about_entry = make_row(
        &content,
        "About / Status",
        "Hey there! I am using WhatsApp.",
    );
    if let Some(d) = &data {
        if !d.about.is_empty() {
            about_entry.set_text(&d.about);
        }
    }

    let desc_entry = make_row(
        &content,
        "Business Description",
        "What does your business do?",
    );
    if let Some(d) = &data {
        if !d.description.is_empty() {
            desc_entry.set_text(&d.description);
        }
    }

    let category_entry = make_row(
        &content,
        "Business Category",
        "e.g., Real Estate, Technology",
    );
    if let Some(d) = &data {
        if !d.category.is_empty() {
            category_entry.set_text(&d.category);
        }
    }

    let address_entry = make_row(&content, "Business Address", "123 Main St, City, Country");
    if let Some(d) = &data {
        if !d.address.is_empty() {
            address_entry.set_text(&d.address);
        }
    }

    let email_entry = make_row(&content, "Business Email", "contact@yourbusiness.com");
    if let Some(d) = &data {
        if !d.email.is_empty() {
            email_entry.set_text(&d.email);
        }
    }

    let website_entry = make_row(&content, "Website", "https://yourbusiness.com");
    if let Some(d) = &data {
        if !d.website.is_empty() {
            website_entry.set_text(&d.website);
        }
    }

    let sep2 = gtk4::Separator::new(Orientation::Horizontal);
    sep2.set_margin_top(8);
    sep2.set_margin_bottom(4);
    content.append(&sep2);

    // Business hours
    let hours_title = Label::new(Some("Business Hours"));
    hours_title.add_css_class("heading");
    hours_title.set_halign(Align::Start);
    content.append(&hours_title);

    let days = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    let mut _hour_entries: Vec<(Entry, Entry)> = Vec::new();
    for day in &days {
        let row = Box::new(Orientation::Horizontal, 8);
        row.set_margin_start(8);
        let day_lbl = Label::new(Some(day));
        day_lbl.set_width_chars(11);
        day_lbl.set_halign(Align::Start);
        day_lbl.add_css_class("dim-label");
        let open = Entry::new();
        open.set_placeholder_text(Some("09:00"));
        open.set_width_chars(7);
        let dash = Label::new(Some("–"));
        let close = Entry::new();
        close.set_placeholder_text(Some("17:00"));
        close.set_width_chars(7);
        row.append(&day_lbl);
        row.append(&open);
        row.append(&dash);
        row.append(&close);
        content.append(&row);
        _hour_entries.push((open, close));
    }

    // Save button
    let btn_row = Box::new(Orientation::Horizontal, 12);
    btn_row.set_halign(Align::Center);
    btn_row.set_margin_top(16);

    let save_btn = Button::with_label("Save Changes");
    save_btn.add_css_class("suggested-action");
    save_btn.set_size_request(160, -1);

    let cancel_btn = Button::with_label("Cancel");
    cancel_btn.add_css_class("flat");
    let window_weak = window.downgrade();
    cancel_btn.connect_clicked(move |_| {
        if let Some(window) = window_weak.upgrade() {
            window.close();
        }
    });

    let bridge_c = bridge.clone();
    let name_c = name_entry.clone();
    let about_c = about_entry.clone();
    let window_weak = window.downgrade();
    save_btn.connect_clicked(move |_| {
        let new_name = name_c.text().to_string();
        let new_about = about_c.text().to_string();
        if !new_name.is_empty() {
            bridge_c.send_command(crate::bridge::WaCommand::SetPushName { name: new_name });
        }
        if !new_about.is_empty() {
            bridge_c.send_command(crate::bridge::WaCommand::SetStatus { text: new_about });
        }
        // TODO: save business-specific fields (description, hours, etc.)
        if let Some(window) = window_weak.upgrade() {
            window.close();
        }
    });

    btn_row.append(&cancel_btn);
    btn_row.append(&save_btn);
    content.append(&btn_row);

    scroll.set_child(Some(&content));

    window.set_child(Some(&scroll));

    // Request profile data from server to populate fields
    bridge.send_command(crate::bridge::WaCommand::GetOwnProfile);

    window.present();
}

/// Send a desktop notification via GApplication (allows withdrawal later).
/// Uses a stable per-chat notification ID so new messages replace old ones
/// and can be dismissed when the chat is read.
/// Robust cross-display clipboard set. GTK4's clipboard API on Wayland
/// (COSMIC, GNOME, KDE) sometimes drops `set_text` calls when no window
/// is focused or when the offer never gets registered with the
/// compositor. We:
///
/// 1. Set the GTK clipboard (works on X11, mostly works on Wayland).
/// 2. Pipe through `wl-copy` (Wayland) — overwrites the previous offer
///    with one owned by an external process, which sticks even after
///    our app loses focus.
/// 3. Fall back to `xclip -selection clipboard` for X11.
///
/// All three are best-effort; failures are silent.
fn set_clipboard_text(text: &str) {
    use std::io::Write;

    fn pipe_to(cmd: &str, args: &[&str], text: &str) -> bool {
        let Ok(mut child) = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // wl-copy daemonizes once stdin closes; the parent exits immediately.
        drop(child.stdin.take());
        matches!(child.wait(), Ok(s) if s.success())
    }

    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

    if wayland {
        // --trim-newline: a trailing \n rides along otherwise, and segmented
        // OTP inputs (TD's is one) treat it as a submit or reject the paste.
        // No --type: the default offer includes the charset variant that
        // XWayland maps to UTF8_STRING, which browsers ask for.
        let ok = pipe_to("wl-copy", &["--trim-newline"], text);
        if !ok {
            log::warn!("clipboard: wl-copy failed");
        }
        // wl-copy writes via data-control, which wl-paste also reads — so a
        // readback here proves nothing about what a normal client can paste.
        // Regular clients read the seat selection, and KWin only syncs that
        // from data-control on the next focus change, which is why a code can
        // become pasteable minutes late. Re-assert through GTK the moment we
        // own focus so the seat selection is current.
        return;
    }

    if let Some(display) = gtk4::gdk::Display::default() {
        display.clipboard().set_text(text);
    }
    pipe_to("xclip", &["-selection", "clipboard"], text);
}

fn send_desktop_notification(app: &adw::Application, chat_id: &str, title: &str, body: &str) {
    let notif = gtk4::gio::Notification::new(title);
    notif.set_body(Some(body));
    // Clicking the banner raises the window and opens this chat.
    notif.set_default_action_and_target_value(
        "app.open-chat",
        Some(&gtk4::glib::Variant::from(chat_id)),
    );
    let notif_id = format!("chat-{}", chat_id.replace('@', "-").replace('.', "-"));
    app.send_notification(Some(&notif_id), &notif);
}

// 2FA detector lives in [`crate::bridge::detect_two_factor_code`] now —
// the gmessages_runtime needs it pre-routing so verification SMS go to
// the dedicated "Verification Codes" inbox instead of a per-shortcode chat.
pub use crate::bridge::detect_two_factor_code;

thread_local! {
    static TWOFA_COPIED: RefCell<std::collections::VecDeque<String>> =
        const { RefCell::new(std::collections::VecDeque::new()) };
}

/// Claim `msg_id` for a 2FA copy, returning false if it was already claimed.
/// The live-push and server-fetch paths can both surface the same message, and
/// reopening a chat re-walks history — without this the clipboard would churn.
fn twofa_mark_copied(msg_id: &str) -> bool {
    TWOFA_COPIED.with(|seen| {
        let mut seen = seen.borrow_mut();
        if seen.iter().any(|id| id == msg_id) {
            return false;
        }
        seen.push_back(msg_id.to_string());
        if seen.len() > 200 {
            seen.pop_front();
        }
        true
    })
}

thread_local! {
    /// Last auto-copied code, re-asserted when the window regains focus.
    static LAST_2FA: RefCell<Option<(String, std::time::Instant)>> = const { RefCell::new(None) };
}

/// Re-publish the most recent OTP through GTK now that we hold focus. Wayland
/// refuses clipboard writes from unfocused clients, so the wl-copy offer made
/// at arrival time may not have reached the seat selection normal apps read.
pub fn reassert_2fa_clipboard() {
    let pending = LAST_2FA.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(_, at)| at.elapsed().as_secs() < 300)
            .map(|(code, _)| code.clone())
    });
    if let Some(code) = pending
        && let Some(display) = gtk4::gdk::Display::default()
    {
        display.clipboard().set_text(&code);
        log::info!("clipboard: re-asserted OTP on focus");
    }
}

/// Copy `text` to the system clipboard and show an OSD-style notification.
/// On COSMIC and other freedesktop-spec compliant DEs, the notification
/// renders as a transient OSD overlay.
pub fn copy_2fa_code_with_osd(app: &adw::Application, code: &str, sender: &str) {
    LAST_2FA.with(|c| {
        *c.borrow_mut() = Some((code.to_string(), std::time::Instant::now()));
    });
    set_clipboard_text(code);
    // Single GNotification under our real app-id (com.whatsapp.desktop), so it
    // groups with the app's other notifications and the user's per-app mute
    // actually applies to it.
    //
    // Priority is NORMAL, not Urgent. Urgent maps to freedesktop "critical"
    // urgency, which is RESIDENT: the DE (KDE/Plasma especially) keeps it on
    // screen until manually dismissed and IGNORES any timeout — that's what made
    // this toast stick around for 16+ minutes. Normal auto-expires after the
    // DE's default (a few seconds), which is plenty to read and Ctrl+V the code.
    //
    // We deliberately do NOT also fire `notify-send`: without `--app-name` it
    // posts under the binary name `whatsapp-desktop` — a DIFFERENT app-id from
    // ours — so it both duplicated this toast AND couldn't be muted along with
    // the rest of the app's notifications.
    let notif = gtk4::gio::Notification::new("Verification code copied");
    notif.set_body(Some(&format!("{code} from {sender} — paste with Ctrl+V")));
    notif.set_priority(gtk4::gio::NotificationPriority::Normal);
    // Unique ID so successive codes don't replace/coalesce each other.
    let notif_id = format!(
        "auth-code-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    app.send_notification(Some(&notif_id), &notif);
    log::info!("2FA code {code} from {sender} copied to clipboard");
}

#[cfg(test)]
mod twofa_tests {
    use super::detect_two_factor_code;

    #[test]
    fn google_format() {
        assert_eq!(
            detect_two_factor_code("G-123456 is your Google verification code."),
            Some("123456".into())
        );
    }
    #[test]
    fn bank_format() {
        assert_eq!(
            detect_two_factor_code("Your one-time passcode is 4729. Do not share it."),
            Some("4729".into())
        );
    }
    #[test]
    fn hyphenated() {
        assert_eq!(
            detect_two_factor_code("Your code is 123-456"),
            Some("123456".into())
        );
    }
    #[test]
    fn ignores_friend_texts() {
        assert_eq!(detect_two_factor_code("call me at 1234"), None);
        assert_eq!(
            detect_two_factor_code("see you at 4pm, bring 5 beers"),
            None
        );
    }
    #[test]
    fn ignores_long_runs() {
        // Order numbers, account refs, etc. — too long to be 2FA.
        assert_eq!(
            detect_two_factor_code("verify your account 1234567890"),
            None
        );
    }

    #[test]
    fn td_security_code_format() {
        let body = "TD will not send you sign-in links by text. Beware of scams. \
                    Do not reveal this code. We will not contact you for it. \
                    Your security code is 065458.";
        assert_eq!(detect_two_factor_code(body), Some("065458".into()));
    }
}

/// Withdraw (dismiss) any notification for a chat — called when chat is opened or read.
fn withdraw_chat_notification(app: &adw::Application, chat_id: &str) {
    let notif_id = format!("chat-{}", chat_id.replace('@', "-").replace('.', "-"));
    app.withdraw_notification(&notif_id);
}

/// Play the standard freedesktop message notification sound.
fn play_notification_sound() {
    std::thread::spawn(|| {
        let sound_file = "/usr/share/sounds/freedesktop/stereo/message-new-instant.oga";
        let fallback = "/usr/share/sounds/freedesktop/stereo/message.oga";
        let file = if std::path::Path::new(sound_file).exists() {
            sound_file
        } else if std::path::Path::new(fallback).exists() {
            fallback
        } else {
            let _ = std::process::Command::new("canberra-gtk-play")
                .arg("--id=message-new-instant")
                .spawn();
            return;
        };
        if std::process::Command::new("paplay")
            .arg(file)
            .spawn()
            .is_err()
        {
            let _ = std::process::Command::new("canberra-gtk-play")
                .arg("--file")
                .arg(file)
                .spawn();
        }
    });
}

// ── Global search results window ────────────────────────────────────────────

fn show_global_search_results(
    parent: &adw::ApplicationWindow,
    bridge: &Arc<crate::bridge::Bridge>,
    chat_view: &crate::ui::chat_view::ChatViewPanel,
    chat_list: &ChatListPanel,
    sidebar_stack: &Stack,
    profile_revealer: &Revealer,
    app: &adw::Application,
    query: &str,
    results: Vec<crate::bridge::SearchHit>,
) {
    use gtk4::{Label, ListBox, Orientation, ScrolledWindow, SelectionMode};

    let dialog = gtk4::Window::builder()
        .title(&format!(
            "Search: \"{}\" — {} results",
            query,
            results.len()
        ))
        .default_width(600)
        .default_height(500)
        .modal(true)
        .transient_for(parent)
        .build();

    let vbox = Box::new(Orientation::Vertical, 0);

    if results.is_empty() {
        let lbl = Label::new(Some("No messages found."));
        lbl.add_css_class("dim-label");
        lbl.set_margin_top(32);
        vbox.append(&lbl);
    } else {
        let list = ListBox::new();
        list.set_selection_mode(SelectionMode::None);
        list.add_css_class("boxed-list");

        for hit in &results {
            let row = adw::ActionRow::builder()
                .title(&hit.text.chars().take(120).collect::<String>())
                .subtitle(&format!(
                    "{} — {}",
                    if hit.sender_name.is_empty() {
                        "You"
                    } else {
                        &hit.sender_name
                    },
                    format_timestamp(hit.timestamp),
                ))
                .activatable(true)
                .build();

            let chat_id = hit.chat_id.clone();
            let chat_name = hit.chat_name.clone();
            let msg_id = hit.msg_id.clone();
            let cv = chat_view.clone();
            let br = bridge.clone();
            let chat_list = chat_list.clone();
            let sidebar_stack = sidebar_stack.clone();
            let profile_revealer = profile_revealer.clone();
            let app = app.clone();
            let dialog_weak = dialog.downgrade();
            row.connect_activated(move |_| {
                activate_chat(&cv, &br, &chat_id, &chat_name);
                cv.jump_to_message(&msg_id);
                sidebar_stack.set_visible_child_name("chats");
                chat_list.select_chat(&chat_id);
                profile_revealer.set_reveal_child(false);
                withdraw_chat_notification(&app, &chat_id);
                if let Some(dialog) = dialog_weak.upgrade() {
                    dialog.close();
                }
            });
            list.append(&row);
        }

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_child(Some(&list));
        vbox.append(&scroll);
    }

    dialog.set_child(Some(&vbox));

    // Escape closes, matching the app's other dialogs.
    let key = gtk4::EventControllerKey::new();
    let dialog_weak = dialog.downgrade();
    key.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk4::gdk::Key::Escape {
            if let Some(dialog) = dialog_weak.upgrade() {
                dialog.close();
            }
            gtk4::glib::Propagation::Stop
        } else {
            gtk4::glib::Propagation::Proceed
        }
    });
    dialog.add_controller(key);

    dialog.present();
}

fn format_timestamp(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%b %d, %H:%M").to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

// ── Incoming call dialog ────────────────────────────────────────────────────

fn show_incoming_call_dialog(
    parent: &adw::ApplicationWindow,
    bridge: &Arc<crate::bridge::Bridge>,
    chat_id: &str,
    caller_name: &str,
    is_video: bool,
) {
    let kind = if is_video { "Video" } else { "Voice" };
    let dialog = adw::AlertDialog::builder()
        .heading(&format!("Incoming {} Call", kind))
        .body(&format!("{} is calling you", caller_name))
        .build();
    dialog.add_response("reject", "Decline");
    dialog.add_response("accept", "Accept");
    dialog.set_response_appearance("reject", adw::ResponseAppearance::Destructive);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);

    let br = bridge.clone();
    let cid = chat_id.to_string();
    dialog.connect_response(None, move |_, response| match response {
        "accept" => br.send_command(WaCommand::AcceptCall {
            chat_id: cid.clone(),
        }),
        _ => br.send_command(WaCommand::RejectCall {
            chat_id: cid.clone(),
        }),
    });
    dialog.present(Some(parent));
}

// ── Multi-send panel (select chats → blast message) ─────────────────────────

/// Open a multi-send window: user selects up to 8 chats, types a message, sends.
#[allow(dead_code)]
pub fn show_multi_send_window(
    parent: &adw::ApplicationWindow,
    bridge: &Arc<crate::bridge::Bridge>,
    chats: &[crate::bridge::ChatSummary],
) {
    use gtk4::{CheckButton, Entry, Label, ListBox, Orientation, ScrolledWindow, SelectionMode};

    let window = gtk4::Window::builder()
        .title("Multi-Send")
        .default_width(500)
        .default_height(600)
        .modal(true)
        .transient_for(parent)
        .build();

    let vbox = Box::new(Orientation::Vertical, 8);
    vbox.set_margin_start(16);
    vbox.set_margin_end(16);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);

    let info = Label::new(Some(
        "Select up to 8 chats and type your message.\nMessages will be sent with random delays (2-6s) to avoid detection.",
    ));
    info.add_css_class("dim-label");
    info.set_wrap(true);
    info.set_halign(gtk4::Align::Start);
    vbox.append(&info);

    // Counter label
    let counter = Label::new(Some("0 / 8 selected"));
    counter.add_css_class("caption");
    counter.set_halign(gtk4::Align::End);
    vbox.append(&counter);

    // Chat selection list
    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("boxed-list");

    let checks: Rc<RefCell<Vec<(String, gtk4::glib::WeakRef<CheckButton>)>>> =
        Rc::new(RefCell::new(Vec::new()));

    for chat in chats {
        let row = Box::new(Orientation::Horizontal, 8);
        row.set_margin_start(8);
        row.set_margin_end(8);
        row.set_margin_top(4);
        row.set_margin_bottom(4);

        let check = CheckButton::new();
        let name_lbl = Label::new(Some(&chat.name));
        name_lbl.set_hexpand(true);
        name_lbl.set_halign(gtk4::Align::Start);
        let type_lbl = Label::new(Some(if chat.is_group { "Group" } else { "Chat" }));
        type_lbl.add_css_class("dim-label");
        type_lbl.add_css_class("caption");

        row.append(&check);
        row.append(&name_lbl);
        row.append(&type_lbl);
        list.append(&row);

        checks
            .borrow_mut()
            .push((chat.id.clone(), check.downgrade()));

        // Enforce 8-chat limit
        let checks_c = checks.clone();
        let counter_c = counter.clone();
        check.connect_toggled(move |_| {
            let count = checks_c
                .borrow()
                .iter()
                .filter(|(_, cb)| cb.upgrade().is_some_and(|cb| cb.is_active()))
                .count();
            counter_c.set_text(&format!("{} / 8 selected", count));
            // Disable unchecked boxes when at limit
            for (_, cb) in checks_c.borrow().iter() {
                if let Some(cb) = cb.upgrade()
                    && !cb.is_active()
                {
                    cb.set_sensitive(count < 8);
                }
            }
        });
    }

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    vbox.append(&scroll);

    // Message input
    let msg_entry = Entry::new();
    msg_entry.set_placeholder_text(Some("Type your message..."));
    msg_entry.set_margin_top(8);
    vbox.append(&msg_entry);

    // Send button
    let send_btn = Button::with_label("Send to Selected");
    send_btn.add_css_class("suggested-action");
    send_btn.set_margin_top(8);

    let br = bridge.clone();
    let checks_c = checks.clone();
    let window_weak = window.downgrade();
    send_btn.connect_clicked(move |_| {
        let text = msg_entry.text().to_string();
        if text.trim().is_empty() {
            return;
        }
        let selected: Vec<String> = checks_c
            .borrow()
            .iter()
            .filter(|(_, cb)| cb.upgrade().is_some_and(|cb| cb.is_active()))
            .map(|(id, _)| id.clone())
            .collect();
        if selected.is_empty() {
            return;
        }
        br.send_command(WaCommand::MultiSend {
            chat_ids: selected,
            text,
        });
        if let Some(window) = window_weak.upgrade() {
            window.close();
        }
    });
    vbox.append(&send_btn);

    window.set_child(Some(&vbox));
    window.present();
}

// ── Send Groups: persistent sidebar panel ───────────────────────────────────

const SEND_GROUPS_FILE: &str = "wa_send_groups.json";

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SendGroup {
    name: String,
    chat_ids: Vec<String>,
}

fn load_send_groups() -> Vec<SendGroup> {
    std::fs::read_to_string(SEND_GROUPS_FILE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_send_groups(groups: &[SendGroup]) {
    if let Ok(data) = serde_json::to_string_pretty(groups) {
        let _ = std::fs::write(SEND_GROUPS_FILE, data);
    }
}

fn build_send_groups_panel(
    bridge: Arc<crate::bridge::Bridge>,
    sidebar_stack: &gtk4::Stack,
    chat_view: crate::ui::chat_view::ChatViewPanel,
    profile_revealer: &Revealer,
) -> Box {
    use gtk4::{Label, ListBox, Orientation, ScrolledWindow, SelectionMode};

    let root = Box::new(Orientation::Vertical, 0);
    root.set_width_request(360);
    root.add_css_class("chat-list-bg");

    // Header
    let header_row = Box::new(Orientation::Horizontal, 8);
    header_row.set_margin_start(14);
    header_row.set_margin_end(14);
    header_row.set_margin_top(14);
    header_row.set_margin_bottom(8);

    let back_btn = Button::from_icon_name("go-previous-symbolic");
    back_btn.add_css_class("flat");
    back_btn.add_css_class("circular");
    let stack_c = sidebar_stack.clone();
    back_btn.connect_clicked(move |_| {
        stack_c.set_visible_child_name("chats");
    });

    let title = Label::new(Some("Send Groups"));
    title.add_css_class("title");
    title.set_hexpand(true);
    title.set_halign(gtk4::Align::Start);

    let add_btn = Button::from_icon_name("list-add-symbolic");
    add_btn.add_css_class("flat");
    add_btn.add_css_class("circular");
    add_btn.set_tooltip_text(Some("Create new send group"));

    header_row.append(&back_btn);
    header_row.append(&title);
    header_row.append(&add_btn);
    root.append(&header_row);

    let info = Label::new(Some(
        "Create groups of chats to message them all at once.\nMax 8 chats per group. Messages are sent with random delays.",
    ));
    info.add_css_class("dim-label");
    info.add_css_class("caption");
    info.set_wrap(true);
    info.set_margin_start(14);
    info.set_margin_end(14);
    info.set_margin_bottom(8);
    root.append(&info);

    // List of saved send groups
    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("navigation-sidebar");

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    root.append(&scroll);

    // Shared state
    let groups: Rc<RefCell<Vec<SendGroup>>> = Rc::new(RefCell::new(load_send_groups()));

    let rebuild_list = {
        let list_c = list.clone();
        let groups_c = groups.clone();
        let bridge_c = bridge.clone();
        let sidebar_stack_c = sidebar_stack.clone();
        let chat_view_c = chat_view.clone();
        let profile_revealer_c = profile_revealer.clone();
        Rc::new(move || {
            while let Some(child) = list_c.first_child() {
                list_c.remove(&child);
            }
            let gs = groups_c.borrow().clone();
            for (idx, sg) in gs.iter().enumerate() {
                let row = Box::new(Orientation::Horizontal, 8);
                row.set_margin_start(14);
                row.set_margin_end(14);
                row.set_margin_top(8);
                row.set_margin_bottom(8);

                let vbox = Box::new(Orientation::Vertical, 2);
                vbox.set_hexpand(true);
                let name_lbl = Label::new(Some(&sg.name));
                name_lbl.set_halign(gtk4::Align::Start);
                name_lbl.add_css_class("heading");
                let count_lbl = Label::new(Some(&format!("{} chats", sg.chat_ids.len())));
                count_lbl.add_css_class("dim-label");
                count_lbl.add_css_class("caption");
                count_lbl.set_halign(gtk4::Align::Start);
                vbox.append(&name_lbl);
                vbox.append(&count_lbl);

                // Send button — opens the send group in the main message pane
                let send_btn = Button::from_icon_name("go-up-symbolic");
                send_btn.add_css_class("suggested-action");
                send_btn.add_css_class("circular");
                send_btn.set_tooltip_text(Some("Compose message"));
                let chat_ids = sg.chat_ids.clone();
                let sg_name = sg.name.clone();
                let cv = chat_view_c.clone();
                let stack_send = sidebar_stack_c.clone();
                let bridge_send = bridge_c.clone();
                let profile_send = profile_revealer_c.clone();
                send_btn.connect_clicked(move |_| {
                    cv.open_send_group(&sg_name, chat_ids.clone());
                    bridge_send.send_command(WaCommand::SetActiveChat { chat_id: None });
                    stack_send.set_visible_child_name("chats");
                    profile_send.set_reveal_child(false);
                });

                // Edit members button
                let edit_btn = Button::from_icon_name("document-edit-symbolic");
                edit_btn.add_css_class("flat");
                edit_btn.add_css_class("circular");
                edit_btn.set_tooltip_text(Some("Edit members"));
                let groups_edit = groups_c.clone();
                let rebuild_edit = Rc::clone(&{
                    // We need a reference to rebuild_list but it hasn't been
                    // returned from this closure yet — we'll wire it after.
                    // For now, use a deferred approach: modify and save, then
                    // the panel's connect_map will rebuild on next view.
                    Rc::new(()) // placeholder — wired below
                });
                let sg_name_edit = sg.name.clone();
                let current_ids = sg.chat_ids.clone();
                edit_btn.connect_clicked(move |btn| {
                    let parent = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
                    let gr = groups_edit.clone();
                    let edit_idx = idx;
                    let name = sg_name_edit.clone();
                    crate::ui::chat_picker::show_chat_picker_preselected(
                        &format!("Edit \"{}\" (max 8)", name),
                        parent.as_ref(),
                        &current_ids,
                        move |selected| {
                            if selected.is_empty() {
                                return;
                            }
                            let chat_ids: Vec<String> = selected.into_iter().take(8).collect();
                            let mut gs = gr.borrow_mut();
                            if edit_idx < gs.len() {
                                gs[edit_idx].chat_ids = chat_ids;
                                save_send_groups(&gs);
                            }
                        },
                    );
                });

                // Delete button
                let del_btn = Button::from_icon_name("edit-delete-symbolic");
                del_btn.add_css_class("flat");
                del_btn.add_css_class("circular");
                del_btn.set_tooltip_text(Some("Delete group"));
                let groups_del = groups_c.clone();
                del_btn.connect_clicked(move |_| {
                    let mut gs = groups_del.borrow_mut();
                    if idx < gs.len() {
                        gs.remove(idx);
                        save_send_groups(&gs);
                    }
                    // List will be rebuilt on next panel open
                });

                row.append(&vbox);
                row.append(&send_btn);
                row.append(&edit_btn);
                row.append(&del_btn);

                let gtk_row = gtk4::ListBoxRow::new();
                gtk_row.set_child(Some(&row));
                list_c.append(&gtk_row);
            }
        })
    };

    // Build initial list
    (rebuild_list)();

    // "Add" button → name prompt then chat picker
    let groups_add = groups.clone();
    let rebuild_c = rebuild_list.clone();
    add_btn.connect_clicked(move |btn| {
        // Step 1: Ask for group name via a small popover
        let popover = gtk4::Popover::new();
        popover.set_parent(btn);

        let form = Box::new(Orientation::Vertical, 8);
        form.set_margin_start(8);
        form.set_margin_end(8);
        form.set_margin_top(8);
        form.set_margin_bottom(8);
        form.set_width_request(280);

        let name_entry = gtk4::Entry::new();
        name_entry.set_placeholder_text(Some("Group name (e.g. 'Clients')"));

        let next_btn = Button::with_label("Select Chats");
        next_btn.add_css_class("suggested-action");

        let gr = groups_add.clone();
        let rb = rebuild_c.clone();
        let pop = popover.clone();
        let ne = name_entry.clone();
        let parent_widget = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok());

        let go = Rc::new({
            let gr = gr.clone();
            let rb = rb.clone();
            let pop = pop.clone();
            let ne = ne.clone();
            let parent_widget = parent_widget.clone();
            move || {
                let name = ne.text().to_string();
                if name.trim().is_empty() {
                    return;
                }
                pop.popdown();

                // Step 2: Open the standard chat picker (multi-select, max 8)
                let gr2 = gr.clone();
                let rb2 = rb.clone();
                let name2 = name.clone();
                crate::ui::chat_picker::show_chat_picker(
                    "Select chats for group (max 8)",
                    true,
                    parent_widget.as_ref(),
                    move |selected| {
                        if selected.is_empty() {
                            return;
                        }
                        let chat_ids: Vec<String> = selected.into_iter().take(8).collect();
                        gr2.borrow_mut().push(SendGroup {
                            name: name2.clone(),
                            chat_ids,
                        });
                        save_send_groups(&gr2.borrow());
                        (rb2)();
                    },
                );
            }
        });

        let go2 = go.clone();
        next_btn.connect_clicked(move |_| (go2)());
        let go3 = go.clone();
        name_entry.connect_activate(move |_| (go3)());

        form.append(&name_entry);
        form.append(&next_btn);
        popover.set_child(Some(&form));
        popover.popup();
    });

    // Rebuild list each time the panel becomes visible
    let rebuild_vis = rebuild_list.clone();
    let groups_vis = groups.clone();
    root.connect_map(move |_| {
        *groups_vis.borrow_mut() = load_send_groups();
        (rebuild_vis)();
    });

    root
}

// show_send_group_compose removed — send groups now use the main ChatViewPanel
// via chat_view.open_send_group()

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_CHAT_WIDTH, MIN_SIDEBAR_WIDTH,
        TOAST_DEDUP_WINDOW, effective_sidebar_width, sanitize_sidebar_width, toast_should_show,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn toast_dedup_only_suppresses_recent_identical_text() {
        let shown_at = Instant::now();
        let last = ("Reconnecting…".to_string(), shown_at);

        assert!(!toast_should_show(
            Some(&last),
            "Reconnecting…",
            shown_at + Duration::from_secs(1)
        ));
        assert!(toast_should_show(
            Some(&last),
            "Connected",
            shown_at + Duration::from_secs(1)
        ));
        assert!(toast_should_show(
            Some(&last),
            "Reconnecting…",
            shown_at + TOAST_DEDUP_WINDOW
        ));
    }

    #[test]
    fn sidebar_width_uses_a_safe_default_for_missing_or_bad_state() {
        assert_eq!(sanitize_sidebar_width(None), DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(
            sanitize_sidebar_width(Some(MIN_SIDEBAR_WIDTH - 1)),
            DEFAULT_SIDEBAR_WIDTH
        );
        assert_eq!(
            sanitize_sidebar_width(Some(MAX_SIDEBAR_WIDTH + 1)),
            DEFAULT_SIDEBAR_WIDTH
        );
        assert_eq!(sanitize_sidebar_width(Some(475)), 475);
    }

    #[test]
    fn sidebar_width_is_temporarily_clamped_on_narrow_windows() {
        assert_eq!(effective_sidebar_width(475, 1200), 475);
        assert_eq!(effective_sidebar_width(475, MIN_CHAT_WIDTH + 420), 420);
        assert_eq!(
            effective_sidebar_width(475, MIN_CHAT_WIDTH + 200),
            MIN_SIDEBAR_WIDTH
        );
    }
}
