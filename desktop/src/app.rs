use std::sync::Arc;
use std::thread;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use tokio::sync::mpsc;

const APP_CSS: &str = "
/* Panel backgrounds */
box.chat-list-bg { background-color: rgba(0, 0, 0, 0.55); }
/* chat list header removed — search is inline now */
headerbar.message-pane-hdr { background: transparent; border: none; box-shadow: none; min-height: 72px; }
/* system titlebar — default GTK size */
box.message-pane-bg { background-color: rgba(0, 0, 0, 0.45); }

box.message-bubble-out {
    background-color: #005c4b;
    border-radius: 8px 8px 2px 8px;
    max-width: 520px;
}
box.message-bubble-in {
    background-color: #202c33;
    border-radius: 8px 8px 8px 2px;
    max-width: 520px;
}
box.message-bubble-out label,
box.message-bubble-in label {
    color: #e9edef;
}
box.message-bubble-out .dim-label,
box.message-bubble-in .dim-label {
    color: #8696a0;
}
box.message-bubble-out .accent,
box.message-bubble-in .accent {
    color: #53bdeb;
}
box.reply-context {
    border-left: 3px solid #53bdeb;
    padding: 4px 8px;
    background-color: alpha(black, 0.04);
    border-radius: 0 4px 4px 0;
}
button.filter-chip,
button.filter-chip:focus,
button.filter-chip:focus-visible,
button.filter-chip:active,
button.filter-chip:backdrop,
button.filter-chip:disabled {
    border-radius: 20px;
    padding: 8px 14px;
    font-size: 0.85em;
    font-weight: 400;
    min-height: 18px;
    min-width: 0;
    margin: 0;
    background-color: transparent;
    background-image: none;
    border: 1px solid rgba(255, 255, 255, 0.25);
    outline: none;
    outline-style: none;
    outline-width: 0;
    outline-offset: 0;
    box-shadow: none;
    -gtk-outline-radius: 0;
    -gtk-icon-shadow: none;
    text-shadow: none;
    color: #e9edef;
}
button.filter-chip:hover {
    background-color: rgba(255, 255, 255, 0.05);
    background-image: none;
    border-color: rgba(255, 255, 255, 0.25);
    box-shadow: none;
}
button.filter-chip:checked,
button.filter-chip:checked:focus,
button.filter-chip:checked:focus-visible {
    background-color: transparent;
    background-image: none;
    border: 1px solid #00a884;
    box-shadow: none;
    color: #00a884;
    outline: none;
    outline-width: 0;
}
button.filter-chip:checked:hover {
    background-color: rgba(0, 168, 132, 0.08);
    background-image: none;
    border-color: #00a884;
    box-shadow: none;
}
label.date-separator {
    color: #667781;
    font-size: 0.78em;
    margin-top: 12px;
    margin-bottom: 4px;
}
window.lightbox {
    background-color: rgba(0, 0, 0, 0.92);
}
box.message-input-frame {
    border-radius: 9999px;
    background-color: rgba(42, 57, 66, 0.2);
    border: 1.5px solid rgba(255, 255, 255, 0.2);
}
button.input-action-btn {
    min-width: 28px;
    min-height: 28px;
    padding: 2px;
    margin: 0 0;
    color: #aebac1;
    -gtk-icon-size: 22px;
}
entry.search-rounded {
    border-radius: 22px;
}
textview.message-input {
    background-color: transparent;
    color: #e9edef;
}
box.message-bubble-out .error,
box.message-bubble-in .error {
    color: #ef5350;
}

/* Typing indicator — bouncing dots */
@keyframes typing-bounce {
    0%, 60%, 100% { opacity: 0.2; margin-bottom: 0px; }
    30%           { opacity: 1.0; margin-bottom: 4px; }
}
.typing-dot {
    color: #00a884;
    font-size: 0.7em;
    animation: typing-bounce 1.4s infinite ease-in-out;
}
.typing-dot-1 { animation-delay: 0s; }
.typing-dot-2 { animation-delay: 0.2s; }
.typing-dot-3 { animation-delay: 0.4s; }

/* Icon rail (left nav panel) */
box.icon-rail {
    background-color: rgba(0, 0, 0, 0.65);
    border-right: 1px solid rgba(255, 255, 255, 0.08);
}

/* Poll widget */
box.poll-widget {
    padding: 4px 2px;
}
box.poll-widget button.flat {
    padding: 4px 6px;
    border-radius: 8px;
}
box.poll-widget button.flat:hover {
    background-color: rgba(255, 255, 255, 0.06);
}

/* Poll voter avatar stack — overlap via negative margins */
box.poll-voters > .avatar {
    margin-start: -4px;
    border: 1px solid rgba(0, 0, 0, 0.3);
    border-radius: 9999px;
}
box.poll-voters > .avatar:first-child {
    margin-start: 0;
}

/* Poll vote bars */
box.poll-bar-track {
    background-color: rgba(255, 255, 255, 0.08);
    border-radius: 4px;
    min-height: 6px;
    margin-top: -2px;
    margin-bottom: 2px;
}
box.poll-bar-fill {
    background-color: #25D366;
    border-radius: 4px;
    min-height: 6px;
}

/* Pinned message banner */
box.pin-banner {
    background-color: rgba(0, 168, 132, 0.15);
    border-bottom: 1px solid rgba(0, 168, 132, 0.4);
    padding: 8px 12px;
}
box.pin-banner button.flat {
    padding: 2px 8px;
}

/* Flash highlight for scroll-to-message */
@keyframes flash-bg {
    0%   { background-color: rgba(0, 168, 132, 0.3); }
    100% { background-color: transparent; }
}
.flash-highlight {
    animation: flash-bg 1.5s ease-out;
}

/* Quick reply / mention list selection */
listbox.navigation-sidebar row:selected {
    background-color: rgba(0, 168, 132, 0.3);
}

/* Go-to-latest scroll button */
button.goto-latest {
    background-color: #00a884;
    color: white;
    min-width: 36px;
    min-height: 36px;
    border-radius: 50%;
    box-shadow: 0 2px 8px rgba(0,0,0,0.4);
}

/* Unread count badge — green circle with white number */
label.unread-badge {
    background-color: #25D366;
    color: white;
    font-size: 0.75em;
    font-weight: 700;
    border-radius: 9999px;
    min-width: 20px;
    min-height: 20px;
    padding: 0 5px;
}

/* Autocorrect send mode toggle */
button.ac-mode-btn {
    min-width: 16px;
    min-height: 16px;
    padding: 1px 4px;
    font-size: 0.7em;
    border-radius: 4px;
    color: #8696a0;
}
button.ac-mode-btn:checked {
    color: #00a884;
}

/* ── Pilafy: bubble breathes while AC waits, settles into final shape ── */
/* While pulsing, the bubble takes on the libadwaita accent (same teal-
 * blue as the send button) so it reads as waiting-to-send. On settle,
 * it transitions to the final outgoing bubble green (#005c4b).
 * accent_bg_color is the adwaita variable the send button uses, so the
 * two stay in sync if the user theme changes. */
@keyframes pilafy-pulse {
    0%   { opacity: 1.0;  background-color: @accent_bg_color; }
    50%  { opacity: 0.55; background-color: alpha(@accent_bg_color, 0.45); }
    100% { opacity: 1.0;  background-color: @accent_bg_color; }
}
box.bubble-row-out.pilafy box.message-bubble-out {
    background-color: @accent_bg_color;
    /* Slightly more rounded than a normal bubble (asymmetric pill feel)
     * but same physical size — matches outgoing bubble padding so the
     * width and line-height don't change when settle removes the class. */
    border-radius: 18px 18px 6px 18px;
    border: 1px solid rgba(255, 255, 255, 0.18);
    animation: pilafy-pulse 1.0s ease-in-out infinite;
}
box.bubble-row-out.pilafy .dim-label {
    opacity: 0.55;
    transition: opacity 0.2s ease-out;
}

/* Phase 2: settle — fade from accent → final green, radius springs back */
@keyframes pilafy-settle {
    0%   { background-color: @accent_bg_color; opacity: 0.92; }
    100% { background-color: #005c4b;          opacity: 1.0;  }
}
box.bubble-row-out.pilafy-settle box.message-bubble-out {
    border-radius: 8px 8px 2px 8px;
    border: 0;
    animation: pilafy-settle 0.45s ease-out forwards;
}
box.bubble-row-out.pilafy-settle .dim-label {
    opacity: 1.0;
    transition: opacity 0.3s ease-in 0.2s;
}

/* Stealth read popup */
popover.stealth-popover contents {
    background-color: #1a2328;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 12px;
    padding: 8px 0;
    min-width: 320px;
    max-height: 400px;
}
box.stealth-message {
    padding: 6px 12px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.05);
}
box.stealth-message:last-child {
    border-bottom: none;
}
label.stealth-sender {
    color: #53bdeb;
    font-size: 0.82em;
    font-weight: 600;
}
label.stealth-text {
    color: #e9edef;
    font-size: 0.88em;
}
label.stealth-time {
    color: #667781;
    font-size: 0.72em;
}
label.stealth-header {
    color: #8696a0;
    font-size: 0.78em;
    font-weight: 600;
    padding: 4px 12px 6px;
}
label.stealth-empty {
    color: #667781;
    font-size: 0.85em;
    font-style: italic;
    padding: 12px;
}

/* Sync progress bar */
progressbar.sync-progress trough {
    min-height: 3px;
    background-color: rgba(255, 255, 255, 0.08);
}
progressbar.sync-progress progress {
    min-height: 3px;
    background-color: #00a884;
    border-radius: 2px;
}

/* ── New-bubble slide-in animation ─────────────────────────────────── */
/* Outgoing (right-aligned) bubbles slide in from the right via animated
 * margin-right (starts large negative, settles to baseline). Incoming
 * bubbles slide in from the left via margin-start. Both use a spring
 * cubic-bezier to land with a small overshoot bounce. The class is
 * added by Rust on bubble creation and removed after ~360ms.
 *
 * Note: GTK4 CSS animations on margin work but reflow layout each
 * frame, which is fine for a single bubble on send/receive. The 50-
 * bubble bulk render in load_history all animates at once on chat
 * switch — that is intentional, gives the chat a reveal feel. */
@keyframes bubble-slide-in-right {
    0%   { opacity: 0; margin-right: -180px; }
    70%  { opacity: 1; margin-right:    4px; }
    100% { opacity: 1; margin-right:    8px; }
}
@keyframes bubble-slide-in-left {
    0%   { opacity: 0; margin-left: -180px; }
    70%  { opacity: 1; margin-left:    4px; }
    100% { opacity: 1; margin-left:    8px; }
}
box.bubble-enter.bubble-row-out {
    animation: bubble-slide-in-right 0.48s cubic-bezier(0.34, 1.56, 0.64, 1);
}
box.bubble-enter.bubble-row-in {
    animation: bubble-slide-in-left 0.48s cubic-bezier(0.34, 1.56, 0.64, 1);
}
/* System / centered bubbles (no left/right anchor) — fall back to a
 * gentle vertical fade-in. */
@keyframes bubble-fade-in {
    0%   { opacity: 0; margin-top: -20px; }
    100% { opacity: 1; margin-top:   3px; }
}
box.bubble-enter:not(.bubble-row-out):not(.bubble-row-in) {
    animation: bubble-fade-in 0.36s cubic-bezier(0.16, 1, 0.3, 1);
}

/* ── Window / dialog open animation ────────────────────────────────── */
/* GTK4 CSS only animates opacity reliably (no transform). Opacity
 * fade-in keeps it subtle and smooth. Applied via .modal-fade class
 * on dialogs/popovers we want animated. */
@keyframes window-fade-in {
    0%   { opacity: 0; }
    25%  { opacity: 0.05; }
    100% { opacity: 1; }
}
window.modal-fade {
    animation: window-fade-in 0.32s cubic-bezier(0.22, 0.61, 0.36, 1);
}
popover.fade-popover contents {
    animation: window-fade-in 0.24s cubic-bezier(0.22, 0.61, 0.36, 1);
}
";

use crate::bridge::{Bridge, WaCommand, WaEvent};
use crate::ui::window::MainWindow;

const APP_ID: &str = "com.whatsapp.desktop";

pub struct WhatsAppApp {
    gtk_app: adw::Application,
}

impl WhatsAppApp {
    pub fn new() -> Self {
        let gtk_app = adw::Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::FLAGS_NONE)
            .build();

        let app = Self { gtk_app };
        app.setup_signals();
        app
    }

    fn setup_signals(&self) {
        // Track whether we've already created the runtime + window.
        // On re-activation (e.g. user clicks dock icon, second instance
        // tries to launch), we just re-show the existing window.
        let initialized = std::cell::Cell::new(false);
        // Store the window so re-activation can show it
        let saved_window: std::cell::RefCell<Option<gtk4::Window>> =
            std::cell::RefCell::new(None);

        self.gtk_app.connect_activate(move |gtk_app| {
            // Re-activation: just show the existing window
            if initialized.get() {
                if let Some(win) = saved_window.borrow().as_ref() {
                    win.set_visible(true);
                    win.present();
                }
                return;
            }
            initialized.set(true);

            // Load application CSS
            let provider = gtk4::CssProvider::new();
            provider.load_from_string(APP_CSS);
            gtk4::style_context_add_provider_for_display(
                &gtk4::gdk::Display::default().expect("No display"),
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );

            // Apply saved theme on startup (zoom is applied pre-GTK in main.rs)
            let startup_settings = crate::ui::settings::AppSettings::load();
            crate::ui::settings::apply_theme(&startup_settings.theme);

            // GTK → Tokio: command channel
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<WaCommand>();

            // Tokio → GTK: async_channel (unbounded so Tokio never stalls during sync —
            // a bounded channel caused the Tokio runtime to block when GTK was busy
            // rebuilding widgets, which prevented the runtime from receiving WhatsApp
            // sync data and caused self-messages to be dropped at the protocol level)
            let (event_tx, event_rx) = async_channel::unbounded::<WaEvent>();

            let bridge = Arc::new(Bridge { cmd_tx });

            // Spawn Tokio runtime in background thread
            let event_tx_clone = event_tx.clone();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_stack_size(8 * 1024 * 1024) // 8MB stack for media uploads
                    .build()
                    .expect("Failed to build Tokio runtime");

                rt.block_on(async move {
                    crate::ui::runtime::run_wa_runtime(event_tx_clone, cmd_rx).await;
                });
            });

            // Build main window
            let window = MainWindow::new(gtk_app, bridge);
            window.present();

            // Save the GTK window for re-activation
            *saved_window.borrow_mut() = window.gtk_window();

            // Receive events on the GTK main context.
            // Process up to BATCH_SIZE events per iteration, then yield back
            // to the GTK event loop so it can paint frames and respond to
            // window-manager pings (prevents "Not Responding" during sync).
            const BATCH_SIZE: usize = 8;
            let win_clone = window.clone();
            glib::MainContext::default().spawn_local(async move {
                loop {
                    // Block until at least one event arrives
                    let first = match event_rx.recv().await {
                        Ok(ev) => ev,
                        Err(_) => break,
                    };
                    win_clone.handle_event(first);

                    // Drain any queued events (up to BATCH_SIZE-1 more)
                    for _ in 1..BATCH_SIZE {
                        match event_rx.try_recv() {
                            Ok(ev) => win_clone.handle_event(ev),
                            Err(_) => break,
                        }
                    }

                    // Yield: let GTK process a paint cycle before the next batch.
                    // This is safe (no reentrancy) because we yield at the await
                    // point, not by calling g_main_context_iteration() inline.
                    glib::timeout_future_with_priority(
                        glib::Priority::HIGH_IDLE,
                        std::time::Duration::from_millis(1),
                    )
                    .await;
                }
            });
        });
    }

    pub fn run(&self) {
        self.gtk_app.run();
    }
}
