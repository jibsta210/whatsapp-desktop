#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod app;
mod bridge;
mod contacts;
mod gm_qr_state;
mod gmessages_runtime;
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

/// Detect native display width from sysfs (no GTK needed).
/// Returns the widest mode in pixels, or 0 if detection fails.
fn detect_display_width() -> u32 {
    let mut max_w = 0u32;
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let modes = entry.path().join("modes");
            if let Ok(text) = std::fs::read_to_string(&modes) {
                if let Some(first) = text.lines().next() {
                    if let Some(w_str) = first.split('x').next() {
                        if let Ok(w) = w_str.parse::<u32>() {
                            max_w = max_w.max(w);
                        }
                    }
                }
            }
        }
    }
    max_w
}

/// Apply display scaling BEFORE GTK initializes by setting GDK_DPI_SCALE.
/// This env var scales the entire rendering pipeline — text, padding,
/// margins, icons, widgets — everything uniformly.
fn apply_prescale(data_dir: &std::path::Path) {
    // Don't override if the user already set GDK_DPI_SCALE externally
    if std::env::var("GDK_DPI_SCALE").is_ok() {
        return;
    }

    // Load zoom_level from settings (before GTK init, so no GTK APIs available)
    let settings_path = data_dir.join("wa_settings.json");
    let zoom_level: f64 = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("zoom_level")?.as_f64())
        .unwrap_or(0.0);

    let scale = if zoom_level > 0.0 {
        zoom_level
    } else {
        // Auto-detect from display resolution
        let width = detect_display_width();
        if width > 2500 {
            // High-DPI: 3200px→1.5, 3840px→1.75, cap at 2.5
            let s = (width as f64 / 2200.0).min(2.5).max(1.0);
            (s * 4.0).round() / 4.0 // round to nearest 0.25
        } else {
            1.0
        }
    };

    if (scale - 1.0).abs() > 0.01 {
        let scale_str = format!("{:.4}", scale);
        // SAFETY: called in main() before any threads or GTK init — single-threaded.
        unsafe { std::env::set_var("GDK_DPI_SCALE", &scale_str) };
        eprintln!("WhatsApp: applied UI scale {:.0}% (GDK_DPI_SCALE={scale_str})", scale * 100.0);
    }
}

fn main() {
    // Set the working directory to the XDG data dir so every relative
    // path in the app (whatsapp.db, wa_avatars/, wa_messages/, …)
    // lands in ~/.local/share/whatsapp-desktop/ instead of $HOME.
    let data_dir = ensure_data_dir();
    std::env::set_current_dir(&data_dir)
        .unwrap_or_else(|e| panic!("failed to chdir to {}: {}", data_dir.display(), e));

    // Apply display scaling BEFORE GTK init — GDK_DPI_SCALE must be set
    // as an env var before the toolkit reads it.
    apply_prescale(&data_dir);

    // Force the GL renderer instead of Vulkan. On many Linux setups GTK4's
    // Vulkan renderer constantly hits VK_SUBOPTIMAL_KHR and rebuilds the
    // swapchain every few frames — this manifests as input lag, choppy
    // hover highlights, and a generally "heavy" feel even though CPU is
    // idle. The classic GL renderer is more stable for chat-like UIs.
    // Users can override by setting GSK_RENDERER themselves before launch.
    if std::env::var("GSK_RENDERER").is_err() {
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    }

    // Tee stderr to a rotating-ish log file so we can diagnose runtime
    // issues even when launched from Cosmic (which discards stderr).
    // Truncate-on-launch: each launch starts fresh; the previous run is
    // preserved as `app.log.prev`.
    let log_path = data_dir.join("app.log");
    let prev_log = data_dir.join("app.log.prev");
    let _ = std::fs::rename(&log_path, &prev_log);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        // Use the file as stderr for everything that follows (env_logger,
        // panic hook, eprintln!).
        use std::os::unix::io::IntoRawFd;
        let fd = file.into_raw_fd();
        unsafe {
            libc::dup2(fd, 2);
            libc::close(fd);
        }
    }

    // Default to info level so the runtime's status logs (gmessages /
    // gaia / repair) make it into app.log. User can still override with
    // RUST_LOG.
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var(
                "RUST_LOG",
                "info,gmessages_rust=debug,whatsapp_desktop=info,whatsapp_rust=warn",
            );
        }
    }
    env_logger::init();
    log::info!(
        "whatsapp-desktop launched; logging to {} (prev run at {})",
        log_path.display(),
        prev_log.display()
    );

    // Log panics to a file before aborting — so we can diagnose crashes
    // even when running without a terminal attached.
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let crash_msg = format!("PANIC at {location}: {msg}\n");
        eprintln!("{crash_msg}");
        // Append to crash log file so it survives abort
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("crash.log")
            .and_then(|mut f| {
                use std::io::Write;
                let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                writeln!(f, "[{ts}] {crash_msg}")
            });
    }));

    let app = WhatsAppApp::new();
    app.run();
}
