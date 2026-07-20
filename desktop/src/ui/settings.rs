//! App settings — persisted as JSON, exposed to the UI via a preferences window.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box, Label, Orientation};
use libadwaita as adw;
use libadwaita::prelude::*;
use serde::{Deserialize, Serialize};

const SETTINGS_FILE: &str = "wa_settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// Show OS notification banners for new messages
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
    /// Play a sound on new messages
    #[serde(default = "default_true")]
    pub notification_sound: bool,
    /// Show message preview text in notifications
    #[serde(default = "default_true")]
    pub notification_preview: bool,
    /// Notify even when the window is focused (but different chat is open)
    #[serde(default)]
    pub notify_when_focused: bool,
    /// Mute all notifications temporarily (do-not-disturb)
    #[serde(default)]
    pub do_not_disturb: bool,
    /// Auto-copy verification codes from incoming SMS to the clipboard and show
    /// a brief OSD. Off = no auto-copy and no 2FA notification at all.
    #[serde(default = "default_true")]
    pub twofa_autocopy: bool,
    // ── AI Autocorrect ──
    /// Gemini API key (used when ai_model = "gemini").
    #[serde(default)]
    pub ai_api_key: String,
    /// DeepSeek API key (used when ai_model = "deepseek"). DeepSeek is an
    /// OpenAI-compatible provider that is far cheaper than Gemini.
    #[serde(default)]
    pub deepseek_api_key: String,
    /// Which AI provider to use: "gemini", "deepseek", or "none".
    #[serde(default = "default_ai_model")]
    pub ai_model: String,
    // ── Appearance ──
    /// Theme: "dark", "light", or "system"
    #[serde(default = "default_theme")]
    pub theme: String,
    /// UI zoom level: 0.0 = auto-detect, otherwise 0.75 – 3.0
    #[serde(default)]
    pub zoom_level: f64,
    /// Preferred width of the chat-list pane in logical pixels. `None` means
    /// the user has not moved the divider yet, so the UI uses its comfortable
    /// first-run default.
    #[serde(default)]
    pub sidebar_width: Option<i32>,
    // ── Behaviour ──
    /// Close to tray instead of quitting
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    // ── Audio ──
    /// PulseAudio/PipeWire source name for voice recording (empty = "default")
    #[serde(default)]
    pub audio_input: String,
}

fn default_ai_model() -> String {
    "gemini".to_string()
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            notifications_enabled: true,
            notification_sound: true,
            notification_preview: true,
            notify_when_focused: false,
            do_not_disturb: false,
            twofa_autocopy: true,
            ai_api_key: String::new(),
            deepseek_api_key: String::new(),
            ai_model: "gemini".to_string(),
            theme: "dark".to_string(),
            zoom_level: 0.0, // 0.0 = auto-detect
            sidebar_width: None,
            close_to_tray: true,
            audio_input: String::new(),
        }
    }
}

impl AppSettings {
    /// Pre-fill the API key from environment/files if not already set.
    pub fn prefill_ai_key(&mut self) {
        let mut changed = false;
        // Gemini key from env vars / files in priority order.
        if self.ai_api_key.is_empty() {
            let key = std::env::var("GEMINI_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .or_else(|| {
                    std::env::var("GOOGLE_API_KEY")
                        .ok()
                        .filter(|k| !k.is_empty())
                })
                .or_else(|| {
                    std::fs::read_to_string("gemini_key.txt")
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|k| !k.is_empty())
                })
                .or_else(|| {
                    let home = std::env::var("HOME").unwrap_or_default();
                    std::fs::read_to_string(
                        std::path::PathBuf::from(home).join(".config/whatsapp-desktop/gemini_key"),
                    )
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|k| !k.is_empty())
                });
            if let Some(k) = key {
                self.ai_api_key = k;
                changed = true;
            }
        }
        // DeepSeek key from env var.
        if self.deepseek_api_key.is_empty()
            && let Ok(k) = std::env::var("DEEPSEEK_API_KEY")
            && !k.is_empty()
        {
            self.deepseek_api_key = k;
            changed = true;
        }
        if changed {
            self.save();
        }
    }

    pub fn load() -> Self {
        let path = PathBuf::from(SETTINGS_FILE);
        if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self) {
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(SETTINGS_FILE, data);
        }
    }
}

/// Shared settings handle — clone-friendly, auto-saves on changes.
#[derive(Clone)]
pub struct SettingsHandle {
    inner: Rc<RefCell<AppSettings>>,
}

impl SettingsHandle {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(AppSettings::load())),
        }
    }

    pub fn get(&self) -> AppSettings {
        self.inner.borrow().clone()
    }

    fn update(&self, f: impl FnOnce(&mut AppSettings)) {
        let mut s = self.inner.borrow_mut();
        f(&mut s);
        s.save();
    }

    pub fn should_notify(&self, is_window_active: bool, is_current_chat: bool) -> bool {
        let s = self.inner.borrow();
        if s.do_not_disturb || !s.notifications_enabled {
            return false;
        }
        if is_current_chat && is_window_active {
            return false;
        }
        if is_window_active && !s.notify_when_focused {
            // Window is focused but different chat — skip notification
            // unless notify_when_focused is on
            return false;
        }
        true
    }

    pub fn should_play_sound(&self) -> bool {
        let s = self.inner.borrow();
        s.notification_sound && s.notifications_enabled && !s.do_not_disturb
    }

    pub fn twofa_autocopy_enabled(&self) -> bool {
        self.inner.borrow().twofa_autocopy
    }

    pub fn show_preview(&self) -> bool {
        self.inner.borrow().notification_preview
    }

    /// Persist the user's preferred chat-list width. Callers sanitize the
    /// value against the layout limits before storing it.
    pub fn set_sidebar_width(&self, width: i32) {
        if self.inner.borrow().sidebar_width == Some(width) {
            return;
        }
        self.update(|settings| settings.sidebar_width = Some(width));
    }
}

/// Show the settings/preferences window.
pub fn show_settings_window(
    settings: &SettingsHandle,
    parent: Option<&gtk4::Window>,
    bridge: &std::sync::Arc<crate::bridge::Bridge>,
) {
    let window = adw::PreferencesWindow::new();
    window.set_title(Some("Settings"));
    // 800×700 fits the QR widget (280px) + page chrome without resizing,
    // and works at every supported zoom level.
    window.set_default_size(800, 700);
    window.set_modal(true);
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
    }

    // ── Notifications page ──────────────────────────────────────────────
    let page = adw::PreferencesPage::new();
    page.set_title("Notifications");
    page.set_icon_name(Some("preferences-system-notifications-symbolic"));

    let group = adw::PreferencesGroup::new();
    group.set_title("Notification Settings");
    group.set_description(Some("Control how you're notified about new messages"));

    // Enable notifications
    let row_enable = adw::SwitchRow::new();
    row_enable.set_title("Desktop Notifications");
    row_enable.set_subtitle("Show banner notifications for new messages");
    row_enable.set_active(settings.get().notifications_enabled);
    let sh = settings.clone();
    row_enable.connect_active_notify(move |row| {
        sh.update(|s| s.notifications_enabled = row.is_active());
    });
    group.add(&row_enable);

    // Sound
    let row_sound = adw::SwitchRow::new();
    row_sound.set_title("Notification Sound");
    row_sound.set_subtitle("Play a sound when a message arrives");
    row_sound.set_active(settings.get().notification_sound);
    let sh = settings.clone();
    row_sound.connect_active_notify(move |row| {
        sh.update(|s| s.notification_sound = row.is_active());
    });
    group.add(&row_sound);

    // Preview
    let row_preview = adw::SwitchRow::new();
    row_preview.set_title("Message Preview");
    row_preview.set_subtitle("Show message text in notification banners");
    row_preview.set_active(settings.get().notification_preview);
    let sh = settings.clone();
    row_preview.connect_active_notify(move |row| {
        sh.update(|s| s.notification_preview = row.is_active());
    });
    group.add(&row_preview);

    // Notify when focused
    let row_focused = adw::SwitchRow::new();
    row_focused.set_title("Notify When Focused");
    row_focused.set_subtitle("Show notifications even when the app window is active");
    row_focused.set_active(settings.get().notify_when_focused);
    let sh = settings.clone();
    row_focused.connect_active_notify(move |row| {
        sh.update(|s| s.notify_when_focused = row.is_active());
    });
    group.add(&row_focused);

    // Do not disturb
    let row_dnd = adw::SwitchRow::new();
    row_dnd.set_title("Do Not Disturb");
    row_dnd.set_subtitle("Mute all notifications");
    row_dnd.set_active(settings.get().do_not_disturb);
    let sh = settings.clone();
    row_dnd.connect_active_notify(move |row| {
        sh.update(|s| s.do_not_disturb = row.is_active());
    });
    group.add(&row_dnd);

    // Auto-copy verification codes (2FA OSD)
    let row_2fa = adw::SwitchRow::new();
    row_2fa.set_title("Auto-copy verification codes");
    row_2fa.set_subtitle("Copy 2FA codes from SMS to the clipboard and show a brief notification");
    row_2fa.set_active(settings.get().twofa_autocopy);
    let sh = settings.clone();
    row_2fa.connect_active_notify(move |row| {
        sh.update(|s| s.twofa_autocopy = row.is_active());
    });
    group.add(&row_2fa);

    page.add(&group);
    window.add(&page);

    // ── AI Autocorrect page ────────────────────────────────────────────
    let ai_page = adw::PreferencesPage::new();
    ai_page.set_title("AI Autocorrect");
    ai_page.set_icon_name(Some("preferences-other-symbolic"));

    let ai_group = adw::PreferencesGroup::new();
    ai_group.set_title("AI Provider");
    ai_group.set_description(Some("Configure the AI model used for autocorrect"));

    // Model selector dropdown
    let model_row = adw::ComboRow::new();
    model_row.set_title("AI Provider");
    model_row.set_subtitle("Gemini, or DeepSeek (much cheaper)");
    let models = gtk4::StringList::new(&["Gemini", "DeepSeek", "None"]);
    model_row.set_model(Some(&models));
    let current_model = settings.get().ai_model.clone();
    let active_idx = match current_model.as_str() {
        "gemini" => 0,
        "deepseek" => 1,
        _ => 2,
    };
    model_row.set_selected(active_idx);
    let sh = settings.clone();
    model_row.connect_selected_notify(move |row| {
        let model = match row.selected() {
            0 => "gemini",
            1 => "deepseek",
            _ => "none",
        };
        sh.update(|s| s.ai_model = model.to_string());
    });
    ai_group.add(&model_row);

    // Gemini API key — masked by default, eye icon to reveal.
    let key_row = adw::PasswordEntryRow::new();
    key_row.set_title("Gemini API Key");
    key_row.set_text(&settings.get().ai_api_key);
    let sh = settings.clone();
    key_row.connect_changed(move |row| {
        let key = row.text().to_string();
        sh.update(|s| s.ai_api_key = key);
    });
    ai_group.add(&key_row);

    // DeepSeek API key — OpenAI-compatible, far cheaper than Gemini.
    let deepseek_key_row = adw::PasswordEntryRow::new();
    deepseek_key_row.set_title("DeepSeek API Key");
    deepseek_key_row.set_text(&settings.get().deepseek_api_key);
    let sh = settings.clone();
    deepseek_key_row.connect_changed(move |row| {
        let key = row.text().to_string();
        sh.update(|s| s.deepseek_api_key = key);
    });
    ai_group.add(&deepseek_key_row);

    ai_page.add(&ai_group);
    window.add(&ai_page);

    // ── Appearance page ───────────────────────────────────────────────
    let appear_page = adw::PreferencesPage::new();
    appear_page.set_title("Appearance");
    appear_page.set_icon_name(Some("applications-graphics-symbolic"));

    let appear_group = adw::PreferencesGroup::new();
    appear_group.set_title("Theme");
    appear_group.set_description(Some("Choose a colour scheme for the app"));

    let theme_row = adw::ComboRow::new();
    theme_row.set_title("Theme");
    theme_row.set_subtitle("Dark, Light, or follow system settings");
    let themes = gtk4::StringList::new(&["Dark", "Light", "System"]);
    theme_row.set_model(Some(&themes));
    let current_theme = settings.get().theme.clone();
    let theme_idx = match current_theme.as_str() {
        "dark" => 0,
        "light" => 1,
        _ => 2,
    };
    theme_row.set_selected(theme_idx);
    let sh = settings.clone();
    theme_row.connect_selected_notify(move |row| {
        let theme = match row.selected() {
            0 => "dark",
            1 => "light",
            _ => "system",
        };
        sh.update(|s| s.theme = theme.to_string());
        apply_theme(theme);
    });
    appear_group.add(&theme_row);
    appear_page.add(&appear_group);

    // ── Zoom / Scale ──
    let zoom_group = adw::PreferencesGroup::new();
    zoom_group.set_title("Zoom");
    zoom_group.set_description(Some(
        "Scales the entire UI — text, spacing, icons, everything. Restart required.",
    ));

    // Current effective zoom display
    let effective = current_effective_zoom();
    let zoom_info = adw::ActionRow::new();
    zoom_info.set_title("Current Scale");
    zoom_info.set_subtitle(&format!("{:.0}%", effective * 100.0));

    zoom_group.add(&zoom_info);

    // Zoom level row with a Scale (slider) widget
    let zoom_row = adw::ActionRow::new();
    zoom_row.set_title("Zoom Level");
    let current_zoom = settings.get().zoom_level;
    zoom_row.set_subtitle(&zoom_label(current_zoom));

    let scale = gtk4::Scale::with_range(Orientation::Horizontal, 0.75, 3.0, 0.25);
    scale.set_width_request(200);
    scale.set_valign(Align::Center);
    scale.set_draw_value(false);
    // Snap positions at 0.25 increments
    for v in (3..=12).map(|i| i as f64 * 0.25) {
        scale.add_mark(v, gtk4::PositionType::Bottom, None);
    }
    // Special "Auto" mark at the left edge
    scale.add_mark(0.75, gtk4::PositionType::Top, Some("Auto"));
    // If zoom_level is 0 (auto), show the slider at the auto-detect position
    let display_val = if current_zoom <= 0.0 {
        0.75_f64 // "Auto" position at left edge
    } else {
        current_zoom
    };
    scale.set_value(display_val);

    // Restart hint label (hidden until changed)
    let restart_hint = Label::new(Some("Restart app to apply new zoom level"));
    restart_hint.add_css_class("dim-label");
    restart_hint.set_visible(false);
    restart_hint.set_halign(Align::Start);
    restart_hint.set_margin_top(4);

    let sh = settings.clone();
    let row_weak = zoom_row.downgrade();
    let hint_weak = restart_hint.downgrade();
    scale.connect_value_changed(move |s| {
        let val = s.value();
        // Snap: if within 0.1 of 0.75, treat as "auto"
        let level = if val < 0.85 {
            0.0
        } else {
            (val * 4.0).round() / 4.0
        };
        sh.update(|settings| settings.zoom_level = level);
        if let Some(row) = row_weak.upgrade() {
            row.set_subtitle(&zoom_label(level));
        }
        if let Some(hint) = hint_weak.upgrade() {
            hint.set_visible(true);
        }
    });
    zoom_row.add_suffix(&scale);
    zoom_group.add(&zoom_row);

    // Restart now button
    let restart_box = Box::new(Orientation::Horizontal, 8);
    restart_box.set_halign(Align::Start);
    restart_box.set_margin_top(4);

    let auto_btn = gtk4::Button::with_label("Reset to Auto");
    let sh = settings.clone();
    let scale_ref = scale.clone();
    let hint_ref2 = restart_hint.clone();
    auto_btn.connect_clicked(move |_| {
        sh.update(|s| s.zoom_level = 0.0);
        scale_ref.set_value(0.75);
        hint_ref2.set_visible(true);
    });

    let restart_btn = gtk4::Button::with_label("Restart Now");
    restart_btn.add_css_class("suggested-action");
    restart_btn.connect_clicked(move |_| {
        // Re-exec the process so GDK_DPI_SCALE takes effect
        let exe = std::env::current_exe().unwrap_or_default();
        let _ = std::process::Command::new(&exe).spawn();
        std::process::exit(0);
    });

    restart_box.append(&auto_btn);
    restart_box.append(&restart_btn);
    zoom_group.add(&restart_hint);
    zoom_group.add(&restart_box);

    appear_page.add(&zoom_group);
    window.add(&appear_page);

    // ── Behaviour page ────────────────────────────────────────────────
    let behave_page = adw::PreferencesPage::new();
    behave_page.set_title("Behaviour");
    behave_page.set_icon_name(Some("preferences-system-symbolic"));

    let behave_group = adw::PreferencesGroup::new();
    behave_group.set_title("Window");

    let tray_row = adw::SwitchRow::new();
    tray_row.set_title("Close to Tray");
    tray_row.set_subtitle("Keep running in background when window is closed");
    tray_row.set_active(settings.get().close_to_tray);
    let sh = settings.clone();
    tray_row.connect_active_notify(move |row| {
        sh.update(|s| s.close_to_tray = row.is_active());
    });
    behave_group.add(&tray_row);
    behave_page.add(&behave_group);

    // ── Audio input group ──
    let audio_group = adw::PreferencesGroup::new();
    audio_group.set_title("Audio");
    audio_group.set_description(Some("Microphone for voice notes"));

    // Detect available audio sources from PulseAudio/PipeWire
    let sources: Vec<String> = {
        let mut srcs = vec!["default".to_string()];
        if let Ok(output) = std::process::Command::new("pactl")
            .args(["list", "short", "sources"])
            .output()
        {
            if let Ok(text) = String::from_utf8(output.stdout) {
                for line in text.lines() {
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.len() >= 2 {
                        let name = parts[1].to_string();
                        // Skip monitor sources (output loopbacks)
                        if !name.contains(".monitor") {
                            srcs.push(name);
                        }
                    }
                }
            }
        }
        srcs
    };

    let audio_row = adw::ComboRow::new();
    audio_row.set_title("Microphone");
    audio_row.set_subtitle("Audio input device for voice note recording");
    let source_list =
        gtk4::StringList::new(&sources.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    audio_row.set_model(Some(&source_list));

    // Select current setting
    let current_input = settings.get().audio_input.clone();
    let active_idx = sources
        .iter()
        .position(|s| s == &current_input)
        .unwrap_or(0) as u32;
    audio_row.set_selected(active_idx);

    let sources_c = sources.clone();
    let sh = settings.clone();
    audio_row.connect_selected_notify(move |row| {
        let idx = row.selected() as usize;
        let input = sources_c.get(idx).cloned().unwrap_or_default();
        let input = if input == "default" {
            String::new()
        } else {
            input
        };
        sh.update(|s| s.audio_input = input);
    });
    audio_group.add(&audio_row);
    behave_page.add(&audio_group);

    // ── Account group: log out / unlink this device ──
    let account_group = adw::PreferencesGroup::new();
    account_group.set_title("Account");
    let logout_row = adw::ActionRow::new();
    logout_row.set_title("Disconnect this session");
    logout_row.set_subtitle(
        "Disconnects this desktop from WhatsApp for now. The device stays linked \
         to your phone and reconnects on next launch — it is not unlinked.",
    );
    let logout_btn = gtk4::Button::with_label("Disconnect");
    logout_btn.set_valign(gtk4::Align::Center);
    logout_btn.add_css_class("destructive-action");
    {
        let bridge = bridge.clone();
        let window_weak = window.downgrade();
        logout_btn.connect_clicked(move |_| {
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            // Honest copy: this only disconnects the current session. The
            // device remains linked on the phone and the app reconnects on
            // next launch — a true unlink (remove-device IQ + local session
            // wipe) is not yet implemented (see runtime.rs Logout handler).
            let dialog = adw::AlertDialog::new(
                Some("Disconnect this session?"),
                Some(
                    "This disconnects the desktop from WhatsApp until you reopen \
                     the app. The device stays linked to your phone — it is not \
                     unlinked.",
                ),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("logout", "Disconnect");
            dialog.set_response_appearance("logout", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");
            let bridge = bridge.clone();
            let window_weak = window.downgrade();
            dialog.connect_response(None, move |dlg, resp| {
                if resp == "logout" {
                    bridge.send_command(crate::bridge::WaCommand::Logout);
                    if let Some(window) = window_weak.upgrade() {
                        window.close();
                    }
                }
                dlg.close();
            });
            dialog.present(Some(&window));
        });
    }
    logout_row.add_suffix(&logout_btn);
    account_group.add(&logout_row);
    behave_page.add(&account_group);

    window.add(&behave_page);

    // ── Google Messages page (optional) ─────────────────────────────────
    // Hidden unless the user has set GMESSAGES_ENABLE=1 (during the
    // experimental phase) — once we're confident in the integration this
    // gate goes away.
    if std::env::var("GMESSAGES_ENABLE").as_deref() == Ok("1") {
        let gm_page = build_gmessages_page();
        window.add(&gm_page);
    }

    window.present();
}

/// Build the Google Messages settings page. Renders a scannable QR in-app
/// when pairing is required; status text + send-routing toggle otherwise.
fn build_gmessages_page() -> adw::PreferencesPage {
    use gtk4::prelude::*;

    let page = adw::PreferencesPage::new();
    page.set_title("SMS / Google Messages");
    page.set_icon_name(Some("phone-symbolic"));

    // ── Status group ────────────────────────────────────────────────────
    let status = adw::PreferencesGroup::new();
    status.set_title("Status");
    status.set_description(Some(
        "Pairs your desktop with the Google Messages app on your phone, \
         relaying SMS, MMS, and RCS through it.",
    ));

    let auth_path = std::path::PathBuf::from("gmessages-auth.json");
    let is_paired = std::fs::metadata(&auth_path)
        .map(|m| m.len() > 100)
        .unwrap_or(false);
    let qr_url = crate::gm_qr_state::get();

    let pair_row = adw::ActionRow::new();
    pair_row.set_title("Pairing");
    pair_row.set_subtitle(if qr_url.is_some() {
        "Pairing required — scan the QR code below with Google Messages on your phone"
    } else if is_paired {
        "Paired — phone is relaying SMS to this desktop"
    } else {
        "Not paired"
    });
    let pair_btn = gtk4::Button::with_label(if is_paired { "Re-pair…" } else { "Pair…" });
    pair_btn.set_valign(gtk4::Align::Center);
    pair_btn.add_css_class("suggested-action");
    pair_btn.connect_clicked(|_btn| {
        // Wipe current auth and signal the runtime to enter pair flow.
        // The runtime then publishes a fresh QR URL into gm_qr_state,
        // which the polling loop above renders into the QR row in this
        // same settings page.
        let auth = std::path::PathBuf::from("gmessages-auth.json");
        let _ = std::fs::remove_file(&auth);
        crate::gm_qr_state::request_repair();
    });
    pair_row.add_suffix(&pair_btn);
    pair_row.set_activatable_widget(Some(&pair_btn));
    status.add(&pair_row);

    // Persistent (Gaia) pairing row — uses Firefox cookies for a months-long
    // session that doesn't need a phone scan. Falls back to QR (above) if
    // the user isn't logged into messages.google.com in Firefox.
    let gaia_row = adw::ActionRow::new();
    gaia_row.set_title("Persistent login (Firefox cookies)");
    // Pull the paired account's email out of the auth file (Gaia path
    // stamps it on AuthData.gaia_account_email) so the row says "Paired
    // with foo@gmail.com" instead of a generic "Paired".
    let gaia_email = read_gaia_account_email();
    let gaia_subtitle: String = match &gaia_email {
        Some(email) => format!(
            "Paired with {email}. Stays signed in as long as your Firefox \
             session does — no QR scans needed."
        ),
        None => "Pairs without scanning a QR. Requires being signed into \
             messages.google.com in Firefox. Stays logged in for months."
            .to_string(),
    };
    gaia_row.set_subtitle(&gaia_subtitle);
    let gaia_btn = gtk4::Button::with_label(if gaia_email.is_some() {
        "Re-pair…"
    } else {
        "Pair via Firefox"
    });
    gaia_btn.set_valign(gtk4::Align::Center);
    gaia_btn.connect_clicked(move |btn| {
        let auth = std::path::PathBuf::from("gmessages-auth.json");
        let _ = std::fs::remove_file(&auth);
        // Reset ALL stale gaia state from any previous attempt so the
        // dialog can't accidentally render a leftover Success / emoji /
        // account list from before. Without these resets, e.g. a prior
        // attempt that ended in Success would leak its status into the
        // first poll tick of the new dialog and the user would briefly
        // see "Paired ✓" before the runtime even picked up our request.
        crate::gm_qr_state::set_gaia_status(crate::gm_qr_state::GaiaStatus::Starting);
        crate::gm_qr_state::set_gaia_emoji(None);
        crate::gm_qr_state::set_available_accounts(None);
        // Drain any stale answer bits.
        let _ = crate::gm_qr_state::take_chosen_authuser();
        let _ = crate::gm_qr_state::take_gaia_confirmation();
        crate::gm_qr_state::request_gaia_pair();
        log::info!("gaia: click → resetting status to Starting; opening dialog");
        if let Some(win) = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok()) {
            start_gaia_status_dialog(win);
        } else {
            log::warn!("gaia: couldn't find parent window for status dialog");
        }
    });
    gaia_row.add_suffix(&gaia_btn);
    gaia_row.set_activatable_widget(Some(&gaia_btn));
    status.add(&gaia_row);

    // QR row that auto-updates: when the gm runtime publishes a fresh QR
    // URL into `gm_qr_state`, we re-render. Polled because the runtime
    // lives in another thread and dispatching glib idle callbacks back to
    // the GTK main loop from there would require more plumbing.
    let qr_row = adw::ActionRow::new();
    qr_row.set_title("QR code");
    qr_row.set_subtitle("Scan with Google Messages → Settings → Device pairing → QR code scanner");
    let qr_holder = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    qr_holder.set_size_request(280, 280);
    qr_holder.set_valign(gtk4::Align::Center);
    qr_row.add_suffix(&qr_holder);
    status.add(&qr_row);
    page.add(&status);

    // Initial render + 1Hz refresh until the page is destroyed.
    let last_url = std::rc::Rc::new(std::cell::RefCell::new(qr_url.clone()));
    refresh_qr_holder(&qr_holder, &qr_row, qr_url.as_deref());
    {
        let qr_holder_weak = qr_holder.downgrade();
        let qr_row_weak = qr_row.downgrade();
        let last_url = last_url.clone();
        gtk4::glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            let (Some(qr_holder), Some(qr_row)) = (qr_holder_weak.upgrade(), qr_row_weak.upgrade())
            else {
                return gtk4::glib::ControlFlow::Break;
            };
            let cur = crate::gm_qr_state::get();
            if cur != *last_url.borrow() {
                refresh_qr_holder(&qr_holder, &qr_row, cur.as_deref());
                *last_url.borrow_mut() = cur;
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    // ── Send-routing group ──────────────────────────────────────────────
    let routing = adw::PreferencesGroup::new();
    routing.set_title("Send routing");
    routing.set_description(Some(
        "When a contact is reachable on both WhatsApp and SMS, \
         choose which protocol to use by default.",
    ));

    let row_default_wa = adw::SwitchRow::new();
    row_default_wa.set_title("Prefer WhatsApp when available");
    row_default_wa.set_subtitle(
        "Off: send via SMS even if the contact has WhatsApp. \
         On: send via WhatsApp if available, fall back to SMS otherwise.",
    );
    // Read from a simple config file in CWD (data dir). Default true.
    let pref_path = std::path::PathBuf::from("gmessages-prefer-whatsapp");
    let prefer_wa =
        !pref_path.exists() || std::fs::read_to_string(&pref_path).unwrap_or_default() != "0";
    row_default_wa.set_active(prefer_wa);
    row_default_wa.connect_active_notify(move |row| {
        let v = if row.is_active() { "1" } else { "0" };
        let _ = std::fs::write(&pref_path, v);
    });
    routing.add(&row_default_wa);
    page.add(&routing);

    // ── Storage group ──────────────────────────────────────────────────
    let storage = adw::PreferencesGroup::new();
    storage.set_title("Storage");

    let purge_row = adw::ActionRow::new();
    purge_row.set_title("Clear SMS cache");
    purge_row.set_subtitle(
        "Wipes locally cached SMS history and chat list (gm_*.bin). \
         Messages re-download from your phone on next connect.",
    );
    let purge_btn = gtk4::Button::with_label("Clear");
    purge_btn.set_valign(gtk4::Align::Center);
    purge_btn.add_css_class("destructive-action");
    purge_btn.connect_clicked(|_| {
        let dir = std::path::PathBuf::from("wa_messages");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str()
                    && name.starts_with("gm_")
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_file("gm_chats.bin");
    });
    purge_row.add_suffix(&purge_btn);
    storage.add(&purge_row);
    page.add(&storage);

    page
}

/// Replace the contents of `holder` with either a QR widget for `url`, or
/// an "already paired" placeholder when `url` is None.
fn refresh_qr_holder(holder: &gtk4::Box, row: &adw::ActionRow, url: Option<&str>) {
    use gtk4::prelude::*;
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    if let Some(u) = url {
        row.set_subtitle("Scan with Google Messages → Settings → Device pairing → QR code scanner");
        holder.append(&render_qr_widget(u, 280));
    } else {
        row.set_subtitle("Already paired — no QR needed");
        let label = gtk4::Label::new(Some("✓ Paired"));
        label.add_css_class("dim-label");
        label.set_valign(gtk4::Align::Center);
        holder.append(&label);
    }
}

/// Render a scannable QR code as a `gtk4::DrawingArea` of `size`×`size` px.
/// Black-on-white with a 2-module quiet zone. Cairo draws each module as a
/// filled rectangle; suitable for being scanned directly off a screen.
fn render_qr_widget(url: &str, size: i32) -> gtk4::DrawingArea {
    use gtk4::prelude::*;
    use qrcode::{Color, QrCode};

    let area = gtk4::DrawingArea::new();
    area.set_content_width(size);
    area.set_content_height(size);
    area.set_valign(gtk4::Align::Center);
    area.set_halign(gtk4::Align::Center);

    // Encode once; if the URL is malformed, leave the area blank.
    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("settings: failed to encode QR: {e}");
            return area;
        }
    };
    let modules = code.to_colors();
    let width = code.width(); // # of modules per side
    let quiet = 2_usize;
    let total = width + quiet * 2;

    area.set_draw_func(move |_, cr, w, h| {
        let cell = (w.min(h) as f64) / total as f64;
        // Background: white.
        cr.set_source_rgb(1.0, 1.0, 1.0);
        let _ = cr.paint();
        // Modules: black.
        cr.set_source_rgb(0.0, 0.0, 0.0);
        for y in 0..width {
            for x in 0..width {
                if modules[y * width + x] == Color::Dark {
                    let xx = (x + quiet) as f64 * cell;
                    let yy = (y + quiet) as f64 * cell;
                    cr.rectangle(xx, yy, cell, cell);
                }
            }
        }
        let _ = cr.fill();
    });
    area
}

/// Apply the selected theme via libadwaita's StyleManager.
pub fn apply_theme(theme: &str) {
    let sm = adw::StyleManager::default();
    match theme {
        "light" => sm.set_color_scheme(adw::ColorScheme::ForceLight),
        "dark" => sm.set_color_scheme(adw::ColorScheme::ForceDark),
        _ => sm.set_color_scheme(adw::ColorScheme::Default),
    }
}

/// Read the currently active scale from GDK_DPI_SCALE (set in main.rs before GTK init).
pub fn current_effective_zoom() -> f64 {
    std::env::var("GDK_DPI_SCALE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.0)
}

/// Format a zoom level for display: "Auto (150%)" or "125%"
fn zoom_label(level: f64) -> String {
    if level <= 0.0 {
        let effective = current_effective_zoom();
        format!("Auto ({:.0}%)", effective * 100.0)
    } else {
        format!("{:.0}%", level * 100.0)
    }
}

/// Pop a single non-blocking status dialog the moment Gaia pair is
/// requested, and progress its body in place as `gm_qr_state::gaia_status`
/// advances. When the runtime publishes an emoji to verify, swap the
/// dialog into a Confirm/Reject prompt and forward the user's answer via
/// `answer_gaia_confirmation`. Auto-closes ~3 seconds after Success or
/// Failed.
///
/// Polled at 4Hz; the timer self-cancels once the dialog is gone.
pub fn start_gaia_status_dialog(parent: gtk4::Window) {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::gm_qr_state::GaiaStatus;

    // Build a persistent dialog with custom content. We deliberately
    // avoid adw::AlertDialog because it auto-closes on every response
    // click — that destroys the dialog the moment the user picks an
    // account, so the emoji-verification step never gets a chance to
    // render. With adw::Dialog + raw gtk4::Buttons, the dialog stays
    // open until WE close it.
    let dialog = adw::Dialog::new();
    dialog.set_title("Pair via Firefox");
    dialog.set_content_width(480);
    dialog.set_can_close(true);

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Pair via Firefox", "")));
    toolbar.add_top_bar(&header);

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    outer.set_margin_top(24);
    outer.set_margin_bottom(24);
    outer.set_margin_start(24);
    outer.set_margin_end(24);
    outer.set_size_request(420, -1);

    let heading_label = gtk4::Label::new(Some("Setting up…"));
    heading_label.set_xalign(0.0);
    heading_label.add_css_class("title-2");
    heading_label.set_wrap(true);
    outer.append(&heading_label);

    let body_label = gtk4::Label::new(Some(&GaiaStatus::Starting.human()));
    body_label.set_xalign(0.0);
    body_label.set_wrap(true);
    body_label.set_use_markup(true);
    outer.append(&body_label);

    // Container that gets repopulated per phase: account-picker buttons,
    // emoji-confirm Confirm/Reject buttons, etc. Always cleared before
    // appending new buttons.
    let actions_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    outer.append(&actions_box);

    let cancel_btn = gtk4::Button::with_label("Cancel");
    cancel_btn.set_halign(gtk4::Align::End);
    cancel_btn.add_css_class("destructive-action");
    outer.append(&cancel_btn);

    toolbar.set_content(Some(&outer));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(&parent));

    let cancelled = Rc::new(RefCell::new(false));
    {
        let dialog_weak = dialog.downgrade();
        let cancelled_c = cancelled.clone();
        cancel_btn.connect_clicked(move |_| {
            log::info!("gaia dialog: user clicked Cancel");
            // Signal both possible waiters to abort cleanly.
            crate::gm_qr_state::answer_chosen_authuser(0); // best-effort no-op if not waiting
            crate::gm_qr_state::answer_gaia_confirmation(false);
            *cancelled_c.borrow_mut() = true;
            if let Some(dialog) = dialog_weak.upgrade() {
                dialog.close();
            }
        });
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Phase {
        Status,
        AccountPicker,
        EmojiConfirm,
        AwaitingPhone,
        Closing,
    }
    let phase = Rc::new(RefCell::new(Phase::Status));
    let phase_for_close = phase.clone();
    let last_status: Rc<RefCell<Option<GaiaStatus>>> = Rc::new(RefCell::new(None));
    let close_at: Rc<RefCell<Option<std::time::Instant>>> = Rc::new(RefCell::new(None));

    // Per-tick state-machine. 4Hz is fast enough that all transitions
    // feel instant but slow enough not to hammer the lock.
    let dialog_weak = dialog.downgrade();
    let parent_weak = parent.downgrade();
    let actions_box_w = actions_box.clone();
    let heading_w = heading_label.clone();
    let body_w = body_label.clone();
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let Some(dialog) = dialog_weak.upgrade() else {
            log::info!("gaia dialog: widget gone, stopping timer");
            return gtk4::glib::ControlFlow::Break;
        };
        if matches!(*phase.borrow(), Phase::Closing) {
            return gtk4::glib::ControlFlow::Break;
        }
        // Auto-close after a terminal status with a 3s grace.
        if let Some(t) = *close_at.borrow()
            && std::time::Instant::now() >= t
        {
            log::info!("gaia dialog: auto-close after terminal status");
            *phase.borrow_mut() = Phase::Closing;
            dialog.close();
            return gtk4::glib::ControlFlow::Break;
        }

        let status = crate::gm_qr_state::get_gaia_status();
        let emoji = crate::gm_qr_state::get_gaia_emoji();
        let accounts = crate::gm_qr_state::get_available_accounts();

        let want_phase = if emoji.is_some() && matches!(status, GaiaStatus::WaitingForEmoji) {
            Phase::EmojiConfirm
        } else if emoji.is_some() && matches!(status, GaiaStatus::AwaitingPhone) {
            Phase::AwaitingPhone
        } else if accounts.is_some() && matches!(status, GaiaStatus::PickingAccount) {
            Phase::AccountPicker
        } else {
            Phase::Status
        };

        // Phase transitions: rebuild the action area.
        if want_phase != *phase.borrow() {
            log::info!(
                "gaia dialog: phase {:?} → {:?} (status={:?}, accounts={}, emoji={})",
                *phase.borrow(),
                want_phase,
                status,
                accounts.as_ref().map(|v| v.len()).unwrap_or(0),
                emoji.is_some(),
            );
            clear_box(&actions_box_w);
            match want_phase {
                Phase::AccountPicker => {
                    let list = accounts.clone().unwrap();
                    heading_w.set_label("Pick a Google account");
                    body_w.set_label(
                            "Several accounts are signed in to Firefox. Choose the one that has Google Messages on your phone:",
                        );
                    for acct in &list {
                        let label = if acct.display_name.is_empty() {
                            acct.email.clone()
                        } else {
                            format!("{}  ({})", acct.email, acct.display_name)
                        };
                        let btn = gtk4::Button::with_label(&label);
                        btn.add_css_class("pill");
                        let n = acct.authuser;
                        btn.connect_clicked(move |_| {
                            log::info!("gaia dialog: user picked authuser={n}");
                            crate::gm_qr_state::answer_chosen_authuser(n);
                        });
                        actions_box_w.append(&btn);
                    }
                }
                Phase::EmojiConfirm => {
                    let e = emoji.clone().unwrap_or_default();
                    heading_w.set_label("Verify pairing emoji");
                    body_w.set_markup(&format!(
                            "Your phone should be showing this emoji:\n\n\
                             <span size=\"xx-large\">{e}</span>\n\n\
                             Tap \"Yes, this matches\" on the phone, then click <b>Confirm</b> here.\n\
                             If they don't match, click <b>Reject</b>.",
                        ));
                    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                    row.set_halign(gtk4::Align::End);
                    let reject = gtk4::Button::with_label("Reject");
                    reject.add_css_class("destructive-action");
                    reject.connect_clicked(|_| {
                        log::info!("gaia dialog: user rejected emoji");
                        crate::gm_qr_state::answer_gaia_confirmation(false);
                    });
                    row.append(&reject);
                    let confirm = gtk4::Button::with_label("Confirm");
                    confirm.add_css_class("suggested-action");
                    confirm.connect_clicked(|_| {
                        log::info!("gaia dialog: user confirmed emoji");
                        crate::gm_qr_state::answer_gaia_confirmation(true);
                    });
                    row.append(&confirm);
                    actions_box_w.append(&row);
                }
                Phase::AwaitingPhone => {
                    let e = emoji.clone().unwrap_or_default();
                    heading_w.set_label("Almost done — confirm on phone");
                    body_w.set_markup(&format!(
                        "<span size=\"xx-large\">{e}</span>\n\n\
                             Your phone is now showing this emoji. \
                             Tap <b>\"Yes, this matches\"</b> on the phone to finish pairing. \
                             This window will close automatically once your phone confirms.",
                    ));
                    // No action buttons here — the next move is on the phone.
                }
                Phase::Status => {
                    heading_w.set_label("Pair via Firefox");
                    body_w.set_label(&status.human());
                }
                Phase::Closing => {}
            }
            *phase.borrow_mut() = want_phase;
            *last_status.borrow_mut() = Some(status.clone());
        } else if want_phase == Phase::Status {
            // Same phase, just refresh body text on status change.
            let stale = last_status.borrow().as_ref() != Some(&status);
            if stale {
                log::info!("gaia dialog: status update → {:?}", status);
                body_w.set_label(&status.human());
                *last_status.borrow_mut() = Some(status.clone());
            }
        }

        // Schedule auto-close on terminal states.
        if matches!(status, GaiaStatus::Success | GaiaStatus::Failed(_))
            && close_at.borrow().is_none()
        {
            *close_at.borrow_mut() =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
        }

        // If parent window is dead, stop polling.
        if parent_weak.upgrade().is_none() {
            log::info!("gaia dialog: parent gone, stopping timer");
            return gtk4::glib::ControlFlow::Break;
        }

        gtk4::glib::ControlFlow::Continue
    });

    // When the dialog itself is closed (via Cancel, ESC, or our own
    // close()), stop polling. Avoid leaking the timer into the next
    // pairing attempt.
    {
        dialog.connect_closed(move |_| {
            log::info!("gaia dialog: closed");
            *phase_for_close.borrow_mut() = Phase::Closing;
        });
    }

    // Ensure cancelled flag wired up (suppresses warnings about unused
    // capture if never inspected — kept for future use).
    let _ = cancelled;
}

/// Read the Gaia account's email from the persisted auth file, if any.
/// Returns `None` if the file is missing, isn't valid JSON, or has no
/// `gaia_account_email` (e.g. a QR-paired session).
fn read_gaia_account_email() -> Option<String> {
    let path = std::path::PathBuf::from("gmessages-auth.json");
    let bytes = std::fs::read(&path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("gaia_account_email")?.as_str().map(|s| s.to_string())
}

/// Remove all children of a gtk4::Box.
fn clear_box(b: &gtk4::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::AppSettings;

    #[test]
    fn settings_without_sidebar_width_remain_compatible() {
        let settings: AppSettings = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(settings.theme, "light");
        assert_eq!(settings.sidebar_width, None);
    }
}
