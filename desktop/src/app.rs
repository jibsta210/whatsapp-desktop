use std::sync::Arc;
use std::thread;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::gio;
use libadwaita as adw;
use libadwaita::prelude::*;
use tokio::sync::mpsc;

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
        self.gtk_app.connect_activate(|gtk_app| {
            // GTK → Tokio: command channel
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<WaCommand>();

            // Tokio → GTK: async_channel (sender is Send+Clone, receiver is awaitable)
            let (event_tx, event_rx) = async_channel::bounded::<WaEvent>(64);

            let bridge = Arc::new(Bridge { cmd_tx });

            // Spawn Tokio runtime in background thread
            let event_tx_clone = event_tx.clone();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to build Tokio runtime");

                rt.block_on(async move {
                    crate::ui::runtime::run_wa_runtime(event_tx_clone, cmd_rx).await;
                });
            });

            // Build main window
            let window = MainWindow::new(gtk_app, bridge);
            window.present();

            // Receive events on the GTK main context
            let win_clone = window.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = event_rx.recv().await {
                    win_clone.handle_event(event);
                }
            });
        });
    }

    pub fn run(&self) {
        self.gtk_app.run();
    }
}
