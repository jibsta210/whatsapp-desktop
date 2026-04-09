#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod app;
mod bridge;
mod ui;

use app::WhatsAppApp;

/// Return the XDG-compliant data directory for whatsapp-desktop,
/// creating it if it doesn't exist.  All relative data paths
/// (whatsapp.db, wa_avatars/, wa_messages/, etc.) resolve against this.
fn ensure_data_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            std::path::PathBuf::from(home).join(".local/share")
        });
    let dir = base.join("whatsapp-desktop");
    std::fs::create_dir_all(&dir).expect("failed to create data directory");
    dir
}

fn main() {
    // Set the working directory to the XDG data dir so every relative
    // path in the app (whatsapp.db, wa_avatars/, wa_messages/, …)
    // lands in ~/.local/share/whatsapp-desktop/ instead of $HOME.
    let data_dir = ensure_data_dir();
    std::env::set_current_dir(&data_dir)
        .unwrap_or_else(|e| panic!("failed to chdir to {}: {}", data_dir.display(), e));

    env_logger::init();

    let app = WhatsAppApp::new();
    app.run();
}
