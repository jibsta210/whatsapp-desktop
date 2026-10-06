//! Top-level [`Client`] facade.
//!
//! Mirrors `pkg/libgm/client.go` from mautrix-gmessages.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::crypto::aesctr::AesCtrHelper;
use crate::crypto::ecdsa::JwkPair;
use crate::http::{ContentType, RelayHttp};
use crate::session::PendingResponse;
use crate::{Error, Event, Result};

/// Persistent auth state. Serialize this between sessions to avoid re-pairing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthData {
    /// Tachyon authentication token returned by RegisterPhoneRelay / pair.
    #[serde(default)]
    pub tachyon_auth_token: Option<Vec<u8>>,

    /// TTL of the tachyon token (microseconds).
    #[serde(default)]
    pub tachyon_ttl: i64,

    /// Tachyon expiry (UNIX millis).
    #[serde(default)]
    pub tachyon_expiry: i64,

    /// Mobile (phone) device descriptor returned at end of pairing.
    #[serde(default)]
    pub mobile: Option<crate::gmproto::authentication::Device>,

    /// Browser/desktop device descriptor.
    #[serde(default)]
    pub browser: Option<crate::gmproto::authentication::Device>,

    /// AES-CTR key + HMAC key used for relay channel encryption.
    #[serde(default)]
    pub request_crypto: Option<AesCtrHelper>,

    /// Refresh ECDSA P-256 keypair used during pairing/refresh.
    #[serde(default)]
    pub refresh_key: Option<JwkPair>,

    /// Web encryption key (returned by GetWebEncryptionKey).
    #[serde(default)]
    pub web_encryption_key: Option<Vec<u8>>,

    /// Cookies (set when using Gaia / Google account auth).
    #[serde(default)]
    pub cookies: HashMap<String, String>,

    /// Destination registration ID; UUID string when present (Gaia path).
    #[serde(default)]
    pub dest_reg_id: Option<String>,

    /// Persistent session UUID negotiated during a previous Connect.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Email of the Google account this client was paired against. Set
    /// only on the Gaia (Firefox-cookie) path; absent for QR pairs.
    /// Used by the desktop UI to display "Paired with foo@gmail.com".
    #[serde(default)]
    pub gaia_account_email: Option<String>,

    /// `authuser` index for the Google account this client was paired
    /// against (0, 1, 2, …). Set on the Gaia path. The relay routes
    /// requests to this account via the `X-Goog-AuthUser` HTTP header;
    /// without this stored, post-restart requests would default to
    /// account 0 and the relay would reject the session.
    #[serde(default)]
    pub gaia_authuser: Option<u32>,
}

impl AuthData {
    pub fn is_paired(&self) -> bool {
        self.tachyon_auth_token.is_some() && self.request_crypto.is_some() && self.browser.is_some()
    }

    pub fn has_cookies(&self) -> bool {
        !self.cookies.is_empty()
    }

    /// Network name carried in `AuthMessage.network` for the **long-poll /
    /// session** path. Empty for QR pairing (matches Go reference's
    /// `AuthData.AuthNetwork()`); the only place the literal `"Bugle"` is
    /// used is the initial `RegisterPhoneRelay` call during pairing.
    pub fn auth_network(&self) -> &'static str {
        if self.has_cookies() { "GDitto" } else { "" }
    }
}

/// Session-scoped runtime state.
pub(crate) struct SessionState {
    /// UUID for the current relay session. Reset on Reconnect.
    pub session_id: String,
    /// Map of `request_id` → oneshot waiter for the matching response.
    pub response_waiters: HashMap<String, oneshot::Sender<PendingResponse>>,
    /// Queue of `response_id`s we need to ACK back to the server.
    pub ack_queue: Vec<String>,
    /// 8-slot ring buffer of `(thing_id, sha256(decrypted_data))` for dedup.
    pub recent_updates: [(String, [u8; 32]); 8],
    pub recent_updates_ptr: usize,
    /// Number of inbound `DataEvent`s the server told us were "old"
    /// (the long-poll opens with an `Ack { count: N }`).
    pub skip_count: i32,
    /// PairCallback is invoked once when the phone sends `PairedData`. It is
    /// cleared after firing.
    pub pair_completion: Option<oneshot::Sender<crate::gmproto::authentication::PairedData>>,
    /// Set during Gaia pairing while waiting on user emoji confirmation.
    /// `true` = user confirmed match; `false` = user rejected. Cleared on
    /// either response.
    pub gaia_emoji_confirm: Option<oneshot::Sender<bool>>,
    /// Set during Gaia pairing while waiting on the user to pick which
    /// Google account to register against. Holds the chosen `authuser`
    /// index (0, 1, …).
    pub gaia_account_choice: Option<oneshot::Sender<u32>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            response_waiters: HashMap::new(),
            ack_queue: Vec::new(),
            recent_updates: Default::default(),
            recent_updates_ptr: 0,
            skip_count: 0,
            pair_completion: None,
            gaia_emoji_confirm: None,
            gaia_account_choice: None,
        }
    }
}

/// Callback invoked whenever [`AuthData`] changes (token refresh, pairing
/// completion). Caller should persist the snapshot to disk.
pub type AuthChangedCallback = Arc<dyn Fn(&AuthData) + Send + Sync>;

/// Inner state shared across tasks via `Arc`.
pub struct ClientInner {
    pub(crate) auth: Mutex<AuthData>,
    pub(crate) event_tx: mpsc::UnboundedSender<Event>,
    pub(crate) event_rx: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
    pub(crate) http: RelayHttp,
    pub(crate) session: Mutex<SessionState>,
    pub(crate) on_auth_changed: Mutex<Option<AuthChangedCallback>>,
    /// Broadcast: sending `()` cancels the long-poll loop (closes the
    /// receiver) so it shuts down cleanly. Use `broadcast` so multiple
    /// background tasks can listen.
    pub(crate) shutdown: broadcast::Sender<()>,
    /// Guards the long-poll/pinger/ack/refresh task set against duplicate
    /// starts from pairing followed by connect, or repeated connect calls.
    pub(crate) task_set_running: AtomicBool,
}

/// Top-level Google Messages client.
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<ClientInner>,
}

impl Client {
    /// Construct a new client. If `auth` is empty, drive pairing via
    /// [`Client::start_pairing`].
    pub fn new(auth: AuthData) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (shutdown, _) = broadcast::channel(8);
        let inner = ClientInner {
            auth: Mutex::new(auth),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            http: RelayHttp::new(),
            session: Mutex::new(SessionState::default()),
            on_auth_changed: Mutex::new(None),
            shutdown,
            task_set_running: AtomicBool::new(false),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub(crate) fn try_start_task_set(&self) -> bool {
        self.inner
            .task_set_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn finish_task_set(&self) {
        self.inner.task_set_running.store(false, Ordering::Release);
    }

    pub(crate) fn is_task_set_running(&self) -> bool {
        self.inner.task_set_running.load(Ordering::Acquire)
    }

    /// Snapshot the current [`AuthData`]. Persist between sessions.
    pub async fn auth_snapshot(&self) -> AuthData {
        self.inner.auth.lock().await.clone()
    }

    /// Take the event receiver. Can only be called once per client.
    pub async fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.inner.event_rx.lock().await.take()
    }

    pub(crate) fn emit(&self, event: Event) {
        let _ = self.inner.event_tx.send(event);
    }

    /// Register a callback that fires every time [`AuthData`] changes
    /// (token refresh, pairing completion). Caller should persist the
    /// snapshot to disk so the next session starts with fresh credentials.
    pub async fn set_auth_changed_callback(&self, cb: AuthChangedCallback) {
        *self.inner.on_auth_changed.lock().await = Some(cb);
    }

    /// Invoke the auth-changed callback, if registered. Called internally
    /// after pairing and after each successful token refresh.
    /// Fire the auth-changed callback registered by
    /// [`Self::set_auth_changed_callback`]. Used internally after pairing
    /// and token refresh; also exposed so the desktop runtime can persist
    /// after out-of-band updates like a background cookie refresh.
    pub async fn notify_auth_changed(&self) {
        let cb_opt = self.inner.on_auth_changed.lock().await.clone();
        if let Some(cb) = cb_opt {
            let snapshot = self.inner.auth.lock().await.clone();
            cb(&snapshot);
        }
    }

    /// Begin a fresh QR pairing flow.
    ///
    /// Emits [`Event::QrCode`], then [`Event::PairSuccess`] once the phone
    /// scans the QR and finalizes pairing. Returns when pairing is fully
    /// complete (caller should then persist [`auth_snapshot`](Self::auth_snapshot)
    /// and call [`connect`](Self::connect)).
    pub async fn start_pairing(&self) -> Result<()> {
        crate::pairing::start_qr_pairing(self).await
    }

    /// Begin a fresh Gaia (Google account) pairing flow. Cookies must
    /// already be installed via [`set_cookies`](Self::set_cookies).
    ///
    /// Emits [`Event::PairingEmoji`] partway through. Caller MUST invoke
    /// [`confirm_pairing_emoji`](Self::confirm_pairing_emoji) once the user
    /// confirms the same emoji appears on the phone. Then returns once
    /// pairing is fully complete.
    pub async fn start_gaia_pairing(&self) -> Result<()> {
        crate::pairing::start_gaia_pairing(self).await
    }

    /// Install Google session cookies into [`AuthData::cookies`]. Used
    /// before [`start_gaia_pairing`](Self::start_gaia_pairing). The expected
    /// keys are listed in [`crate::cookies::GAIA_COOKIE_NAMES`].
    pub async fn set_cookies(&self, cookies: std::collections::HashMap<String, String>) {
        let mut auth = self.inner.auth.lock().await;
        auth.cookies = cookies;
    }

    /// Install cookies from a Google sign-in this application performed
    /// itself, rather than ones scraped out of the user's Firefox profile.
    ///
    /// This is the difference between the bridge having a login and
    /// borrowing one: an adopted session survives Firefox being absent,
    /// signed into a different account, or logged out, and it is refreshed
    /// in place by Google's own `Set-Cookie` responses. Prefer this over
    /// [`set_cookies`](Self::set_cookies) for any new sign-in path.
    ///
    /// Errors if the supplied jar is not a usable Google session, so a
    /// half-finished login cannot take ownership and strand the fallback.
    pub async fn adopt_login_session(
        &self,
        cookies: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        crate::cookies::adopt_login_session(cookies.clone())?;
        let mut auth = self.inner.auth.lock().await;
        auth.cookies = cookies;
        Ok(())
    }

    /// Caller's response to [`Event::PairingEmoji`]: `true` if the user
    /// confirmed the displayed emoji matches the one on the phone, `false`
    /// to abort pairing. Idempotent: only the first call has an effect.
    pub async fn confirm_pairing_emoji(&self, matches: bool) {
        let waiter = self.inner.session.lock().await.gaia_emoji_confirm.take();
        if let Some(tx) = waiter {
            let _ = tx.send(matches);
        } else {
            log::warn!(
                "confirm_pairing_emoji called but no Gaia pairing in progress (or already answered)"
            );
        }
    }

    /// Caller's response to [`Event::AvailableGoogleAccounts`]: pass the
    /// `authuser` index of the chosen account. Idempotent.
    pub async fn choose_google_account(&self, authuser: u32) {
        let waiter = self.inner.session.lock().await.gaia_account_choice.take();
        if let Some(tx) = waiter {
            let _ = tx.send(authuser);
        } else {
            log::warn!(
                "choose_google_account called but no Gaia pairing in progress (or already answered)"
            );
        }
    }

    /// Connect using existing [`AuthData`] and start the long-poll receive loop.
    /// Returns once the loop is established. Long-poll continues running on
    /// a background task until [`disconnect`](Self::disconnect) is called.
    ///
    /// After spawning the long-poll, this also runs the post-connect
    /// activation: tells the server we're the active desktop and asks for
    /// any updates we missed while disconnected. Without this step the phone
    /// won't push live message events.
    pub async fn connect(&self) -> Result<()> {
        if !self.inner.auth.lock().await.is_paired() {
            return Err(Error::AuthRevoked);
        }
        crate::longpoll::spawn(self.clone()).await?;

        // Post-connect activation. Run on a background task so connect()
        // returns promptly; mirrors Go's `postConnect` goroutine.
        let client = self.clone();
        // Subscribed before spawning so a disconnect that lands during the
        // sleep is not missed. Without this the task outlived its connection
        // and reported AuthRevoked for a session nobody was using any more —
        // straight into whatever pairing flow had started in the meantime.
        let mut shutdown = self.inner.shutdown.subscribe();
        tokio::spawn(async move {
            let activate = async {
                // Brief pause to let the long-poll establish before we start
                // sending RPCs (matches Go's 2s sleep).
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                crate::session::set_active_session(&client).await
            };
            let result = tokio::select! {
                r = activate => r,
                _ = shutdown.recv() => return,
            };
            match result {
                Ok(()) => log::info!("post-connect: active session registered"),
                Err(Error::AuthRevoked) => {
                    log::warn!(
                        "post-connect: server rejected our token (AuthRevoked); emitting event so caller can re-pair"
                    );
                    client.emit(Event::AuthRevoked);
                }
                Err(e) => log::warn!("set_active_session failed: {e}"),
            }
        });

        Ok(())
    }

    /// Cancel the long-poll loop and any in-flight RPCs.
    pub async fn disconnect(&self) -> Result<()> {
        let _ = self.inner.shutdown.send(());
        // Wait briefly for the task coordinator to release the running guard,
        // so an immediate re-pair/reconnect starts a fresh task set reliably.
        for _ in 0..100 {
            if !self.is_task_set_running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// Send a text message.
    pub async fn send_text(&self, conversation_id: &str, text: &str) -> Result<String> {
        crate::session::send_text(self, conversation_id, text).await
    }

    /// List the most recent conversations.
    pub async fn list_conversations(
        &self,
        count: i64,
    ) -> Result<crate::gmproto::client::ListConversationsResponse> {
        crate::session::list_conversations(self, count).await
    }

    /// Mark a message as read.
    pub async fn mark_read(&self, conversation_id: &str, message_id: &str) -> Result<()> {
        crate::session::mark_read(self, conversation_id, message_id).await
    }

    /// Set the typing indicator on a conversation.
    pub async fn set_typing(&self, conversation_id: &str, typing: bool) -> Result<()> {
        crate::session::set_typing(self, conversation_id, typing).await
    }

    /// Add or remove an emoji reaction. `action`: `1`=Add, `2`=Remove, `3`=Switch.
    pub async fn send_reaction(
        &self,
        conversation_id: &str,
        message_id: &str,
        emoji: &str,
        action: i32,
    ) -> Result<()> {
        crate::session::send_reaction(self, conversation_id, message_id, emoji, action).await
    }

    /// Ask the phone to upload the full-size version of an image that
    /// arrived thumbnail-only (RCS). The phone re-relays the message with
    /// a real `media_id` afterwards.
    pub async fn get_full_size_image(
        &self,
        message_id: &str,
        action_message_id: &str,
    ) -> Result<()> {
        crate::session::get_full_size_image(self, message_id, action_message_id).await
    }

    /// Fetch the most recent `count` messages of a conversation.
    pub async fn fetch_messages(
        &self,
        conversation_id: &str,
        count: i64,
    ) -> Result<crate::gmproto::client::ListMessagesResponse> {
        crate::session::fetch_messages(self, conversation_id, count, None).await
    }

    /// List contacts the phone has saved. Returns up to ~350 entries.
    pub async fn list_contacts(&self) -> Result<crate::gmproto::client::ListContactsResponse> {
        crate::session::list_contacts(self).await
    }

    /// List the top N (most recently messaged) contacts.
    pub async fn list_top_contacts(
        &self,
        count: i32,
    ) -> Result<crate::gmproto::client::ListTopContactsResponse> {
        crate::session::list_top_contacts(self, count).await
    }

    /// Download a media attachment and decrypt it. `decryption_key` comes
    /// from the `MediaContent` proto on the originating message.
    pub async fn download_media(&self, media_id: &str, decryption_key: &[u8]) -> Result<Vec<u8>> {
        crate::session::download_media(self, media_id, decryption_key).await
    }

    /// Upload and send a media attachment (image / video / audio) as an
    /// MMS/RCS message. `data` is the raw file bytes. Returns the temporary
    /// message id; the phone echoes the real message via the long-poll.
    pub async fn send_media(
        &self,
        conversation_id: &str,
        data: &[u8],
        file_name: &str,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<String> {
        let media = crate::session::upload_media(self, data, file_name, mime).await?;
        crate::session::send_media(self, conversation_id, media, caption).await
    }

    /// Resolve an E.164 phone number to a conversation ID, creating the
    /// conversation if needed.
    pub async fn get_or_create_conversation(&self, phone: &str) -> Result<String> {
        crate::session::get_or_create_conversation(self, phone).await
    }

    /// Ping the phone (NOTIFY_DITTO_ACTIVITY). Returns once the phone replies.
    pub async fn ping_phone(&self) -> Result<()> {
        crate::session::notify_ditto_activity(self).await
    }

    /// Make a raw POST to a pairing endpoint. Used internally.
    pub(crate) async fn post_protobuf<Req, Resp>(
        &self,
        url: &str,
        req: &Req,
        ct: ContentType,
    ) -> Result<Resp>
    where
        Req: prost::Message + prost_reflect::ReflectMessage,
        Resp: prost::Message + prost_reflect::ReflectMessage + Default,
    {
        let (cookies, authuser) = {
            let auth = self.inner.auth.lock().await;
            (auth.cookies.clone(), auth.gaia_authuser)
        };
        self.inner
            .http
            .post::<Req, Resp>(url, req, ct, &cookies, authuser)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthData, Client};

    #[test]
    fn task_set_guard_is_idempotent_and_reusable_after_finish() {
        let client = Client::new(AuthData::default());
        assert!(client.try_start_task_set());
        assert!(!client.try_start_task_set());
        assert!(client.is_task_set_running());

        client.finish_task_set();
        assert!(!client.is_task_set_running());
        assert!(client.try_start_task_set());
    }
}
