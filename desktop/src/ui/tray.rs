//! System tray icon using the StatusNotifierItem D-Bus protocol (ksni crate).
//! This provides a persistent tray icon on GNOME/KDE/XFCE that lets the user
//! show/hide the window or quit the application.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared state between the tray icon (background thread) and GTK (main thread).
pub struct TrayHandle {
    /// Set to true when the user clicks "Show" or activates the tray icon.
    pub show_requested: Arc<AtomicBool>,
    /// Set to true when the user clicks "Quit" in the tray menu.
    pub quit_requested: Arc<AtomicBool>,
}

struct WhatsAppTray {
    show_requested: Arc<AtomicBool>,
    quit_requested: Arc<AtomicBool>,
}

impl ksni::Tray for WhatsAppTray {
    fn id(&self) -> String {
        "com-whatsapp-desktop".to_string()
    }

    fn title(&self) -> String {
        "WhatsApp".to_string()
    }

    fn icon_name(&self) -> String {
        // Try the app icon first, fall back to a standard chat icon
        "com.whatsapp.desktop".to_string()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Communications
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            ksni::MenuItem::Standard(StandardItem {
                label: "Show WhatsApp".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    tray.show_requested.store(true, Ordering::SeqCst);
                }),
                ..Default::default()
            }),
            ksni::MenuItem::Separator,
            ksni::MenuItem::Standard(StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    tray.quit_requested.store(true, Ordering::SeqCst);
                }),
                ..Default::default()
            }),
        ]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // Left-click on tray icon → show window
        self.show_requested.store(true, Ordering::SeqCst);
    }
}

/// Spawn the StatusNotifierItem tray icon in a background thread.
/// Returns a `TrayHandle` that the GTK main loop can poll to detect
/// show/quit requests from the tray menu.
pub fn spawn_tray_icon() -> TrayHandle {
    let show_requested = Arc::new(AtomicBool::new(false));
    let quit_requested = Arc::new(AtomicBool::new(false));

    let handle = TrayHandle {
        show_requested: show_requested.clone(),
        quit_requested: quit_requested.clone(),
    };

    std::thread::spawn(move || {
        use ksni::blocking::TrayMethods;
        let tray = WhatsAppTray {
            show_requested,
            quit_requested,
        };
        match tray.spawn() {
            Ok(_handle) => {
                log::info!("System tray icon active");
                // Keep the thread alive — the D-Bus service runs in the background
                // but needs this thread's runtime to stay alive.
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }
            Err(e) => {
                log::warn!("System tray icon failed: {e}");
                log::warn!(
                    "Install gnome-shell-extension-appindicator for tray support on GNOME."
                );
            }
        }
    });

    handle
}
