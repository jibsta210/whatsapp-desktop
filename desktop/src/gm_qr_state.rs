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
    /// Verification emoji set during Gaia pairing. None when not active.
    gaia_emoji: Option<String>,
    /// User's confirmation response to a Gaia emoji prompt.
    /// `Some(true)` = confirmed match, `Some(false)` = rejected. The
    /// runtime polls + clears via `take_gaia_confirmation`.
    gaia_confirmation: Option<bool>,
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

/// Cross-thread signal: settings page → gm runtime to attempt Gaia
/// (Firefox-cookie based) pairing. Read + cleared by the runtime.
static GAIA_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn request_gaia_pair() {
    GAIA_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn take_gaia_request() -> bool {
    GAIA_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

/// Set the verification emoji. The settings page polls this and shows the
/// confirmation modal when it appears. Pass `None` once handled.
pub fn set_gaia_emoji(emoji: Option<String>) {
    if let Ok(mut s) = state().write() {
        s.gaia_emoji = emoji;
        // New prompt clears any stale confirmation.
        if s.gaia_emoji.is_some() {
            s.gaia_confirmation = None;
        }
    }
}

pub fn get_gaia_emoji() -> Option<String> {
    state().read().ok().and_then(|s| s.gaia_emoji.clone())
}

/// Settings UI calls this when the user clicks Confirm/Reject in the
/// Gaia emoji modal.
pub fn answer_gaia_confirmation(confirmed: bool) {
    if let Ok(mut s) = state().write() {
        s.gaia_confirmation = Some(confirmed);
    }
}

/// Runtime polls this to consume the user's answer.
pub fn take_gaia_confirmation() -> Option<bool> {
    state().write().ok().and_then(|mut s| s.gaia_confirmation.take())
}
