//! App settings — persisted as JSON, exposed to the UI via a preferences window.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{Align, Box, Label, Orientation, Switch};
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
    // ── AI Autocorrect ──
    /// API key for the AI provider (Gemini, Claude, etc.)
    #[serde(default)]
    pub ai_api_key: String,
    /// Which AI model to use: "gemini", "claude", or "none"
    #[serde(default = "default_ai_model")]
    pub ai_model: String,
    // ── Appearance ──
    /// Theme: "dark", "light", or "system"
    #[serde(default = "default_theme")]
    pub theme: String,
    /// UI zoom level: 0.0 = auto-detect, otherwise 0.75 – 3.0
    #[serde(default)]
    pub zoom_level: f64,
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
            ai_api_key: String::new(),
            ai_model: "gemini".to_string(),
            theme: "dark".to_string(),
            zoom_level: 0.0, // 0.0 = auto-detect
            close_to_tray: true,
            audio_input: String::new(),
        }
    }
}

impl AppSettings {
    /// Pre-fill the API key from environment/files if not already set.
    pub fn prefill_ai_key(&mut self) {
        if !self.ai_api_key.is_empty() {
            return;
        }
        // Try env vars and files in priority order
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
                    std::path::PathBuf::from(home)
                        .join(".config/whatsapp-desktop/gemini_key"),
                )
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|k| !k.is_empty())
            });
        if let Some(k) = key {
            self.ai_api_key = k;
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

    pub fn show_preview(&self) -> bool {
        self.inner.borrow().notification_preview
    }
}

/// Show the settings/preferences window.
pub fn show_settings_window(settings: &SettingsHandle, parent: Option<&gtk4::Window>) {
    let window = adw::PreferencesWindow::new();
    window.set_title(Some("Settings"));
    window.set_default_size(500, 450);
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
    model_row.set_title("AI Model");
    model_row.set_subtitle("Which AI provider to use for autocorrect");
    let models = gtk4::StringList::new(&["Gemini", "Claude", "None"]);
    model_row.set_model(Some(&models));
    let current_model = settings.get().ai_model.clone();
    let active_idx = match current_model.as_str() {
        "gemini" => 0,
        "claude" => 1,
        _ => 2,
    };
    model_row.set_selected(active_idx);
    let sh = settings.clone();
    model_row.connect_selected_notify(move |row| {
        let model = match row.selected() {
            0 => "gemini",
            1 => "claude",
            _ => "none",
        };
        sh.update(|s| s.ai_model = model.to_string());
    });
    ai_group.add(&model_row);

    // API key entry — masked by default, eye icon to reveal
    let key_row = adw::PasswordEntryRow::new();
    key_row.set_title("API Key");
    key_row.set_text(&settings.get().ai_api_key);
    let sh = settings.clone();
    key_row.connect_changed(move |row| {
        let key = row.text().to_string();
        sh.update(|s| s.ai_api_key = key);
    });
    ai_group.add(&key_row);

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
    zoom_group.set_description(Some("Adjust the UI scale — useful for high-DPI displays"));

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

    let sh = settings.clone();
    let row_ref = zoom_row.clone();
    scale.connect_value_changed(move |s| {
        let val = s.value();
        // Snap: if within 0.1 of 0.75, treat as "auto"
        let level = if val < 0.85 { 0.0 } else { (val * 4.0).round() / 4.0 };
        sh.update(|settings| settings.zoom_level = level);
        row_ref.set_subtitle(&zoom_label(level));
        apply_zoom(level);
    });
    zoom_row.add_suffix(&scale);
    zoom_group.add(&zoom_row);

    // Reset to auto button
    let auto_btn = gtk4::Button::with_label("Reset to Auto");
    auto_btn.set_halign(Align::Start);
    auto_btn.set_margin_top(4);
    let sh = settings.clone();
    let scale_ref = scale.clone();
    auto_btn.connect_clicked(move |_| {
        sh.update(|s| s.zoom_level = 0.0);
        scale_ref.set_value(0.75);
        apply_zoom(0.0);
    });
    zoom_group.add(&auto_btn);

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
    let source_list = gtk4::StringList::new(&sources.iter().map(|s| s.as_str()).collect::<Vec<_>>());
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

    window.add(&behave_page);

    window.present();
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

/// Auto-detect a sensible zoom level from the primary monitor's geometry.
/// Returns a multiplier (e.g. 1.5 for 150%).
pub fn detect_zoom() -> f64 {
    let display = match gtk4::gdk::Display::default() {
        Some(d) => d,
        None => return 1.0,
    };

    // Get the monitors list
    let monitors = display.monitors();
    let monitor: Option<gtk4::gdk::Monitor> = monitors
        .item(0)
        .and_then(|obj| obj.downcast::<gtk4::gdk::Monitor>().ok());

    if let Some(mon) = monitor {
        let scale_factor = mon.scale_factor() as f64;
        let geom = mon.geometry();
        let width_px = (geom.width() as f64) * scale_factor;

        // High DPI heuristic: if native resolution > 2500px wide and
        // the compositor isn't already applying 2x scaling, suggest scaling up.
        if width_px > 2500.0 && scale_factor < 1.5 {
            // Scale proportionally: 3200px → ~1.5x, 3840px → ~1.75x
            let suggested = (width_px / 1920.0).min(2.5).max(1.0);
            // Round to nearest 0.25
            return (suggested * 4.0).round() / 4.0;
        }
        // If compositor already scales (e.g. 2x), don't double up
        return 1.0;
    }
    1.0
}

/// Apply a zoom level to the GTK runtime via the Xft DPI setting.
/// This scales all fonts and most widgets uniformly.
/// `level` is a multiplier: 1.0 = 100%, 1.5 = 150%, etc.
pub fn apply_zoom(level: f64) {
    let effective = if level <= 0.0 { detect_zoom() } else { level };
    let dpi = (effective * 96.0 * 1024.0) as i32; // GTK uses DPI × 1024
    if let Some(settings) = gtk4::Settings::default() {
        settings.set_gtk_xft_dpi(dpi);
    }
    log::info!(
        "Applied zoom: {:.0}% (xft-dpi={})",
        effective * 100.0,
        dpi / 1024
    );
}

/// Format a zoom level for display: "Auto (150%)" or "125%"
fn zoom_label(level: f64) -> String {
    if level <= 0.0 {
        let auto = detect_zoom();
        format!("Auto ({:.0}%)", auto * 100.0)
    } else {
        format!("{:.0}%", level * 100.0)
    }
}
