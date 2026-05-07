//! Shared atomic state for the Google Messages pairing QR.
//!
//! The gmessages runtime writes the latest QR URL here when pairing is
//! needed; the settings page reads it to render a scannable QR widget
//! inside the app, so users don't have to dig through terminal stderr.

use std::sync::OnceLock;
use std::sync::RwLock;

#[derive(Default)]
struct State {
    /// Current QR URL, if pairing is required. None when paired.
    url: Option<String>,
}

static GLOBAL: OnceLock<RwLock<State>> = OnceLock::new();

fn state() -> &'static RwLock<State> {
    GLOBAL.get_or_init(|| RwLock::new(State::default()))
}

/// Set the current QR URL. Pass `None` to clear once pairing is complete.
pub fn set(url: Option<String>) {
    if let Ok(mut s) = state().write() {
        s.url = url;
    }
}

/// Read the current QR URL.
pub fn get() -> Option<String> {
    state().read().ok().and_then(|s| s.url.clone())
}

/// Cross-thread signal: settings page → gm runtime to drop the current
/// connection and re-run pairing. Set true from the UI; the runtime
/// observes via `take_repair_request` and resets the flag.
static REPAIR_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Settings UI calls this when the user clicks "Re-pair…".
pub fn request_repair() {
    REPAIR_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// gm runtime polls this; returns true once if repair was requested
/// (consumes the flag).
pub fn take_repair_request() -> bool {
    REPAIR_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst)
}
