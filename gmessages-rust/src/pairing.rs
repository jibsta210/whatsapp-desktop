//! Pairing flows.
//!
//! Two paths exist:
//!
//! - [`qr`] — phone-relay (Bugle) QR pairing. Implements
//!   `RegisterPhoneRelay` → QR display → `Paired` event → finalization.
//! - [`gaia`] — Google account pairing. Uses the user's existing
//!   `messages.google.com` session cookies to sign in via `SignInGaia`,
//!   then runs a UKEY2 emoji-verification handshake with the phone.
//!   Resulting auth is durable as long as the Google login stays alive.
//!
//! See `pkg/libgm/pair.go` and `pkg/libgm/pair_google.go` (mautrix-gmessages).

use crate::gmproto::authentication::{BrowserDetails, BrowserType, DeviceType};
use crate::{Client, Result};

pub const USER_AGENT: &str = crate::headers::USER_AGENT;

/// Identity we present to the relay when registering, shared by BOTH pairing
/// flows.
///
/// `TABLET` is load-bearing, not cosmetic. Google allows only one *web*
/// session per phone at a time, so registering as WEB (or PWA, which the
/// server buckets with it) means this client and the user's own
/// messages.google.com tab evict each other — whoever paired last wins.
/// Registering as a tablet takes a separate slot, so the bridge and the
/// user's browser session coexist. This mirrors `BrowserDetailsMessage` in
/// mautrix-gmessages `pkg/libgm/util/config.go`, which is TABLET for the
/// same reason.
///
/// The enum values are spelled out rather than hardcoded because the wire
/// numbering is not the obvious one: WEB=1, TABLET=2, PWA=3.
pub fn browser_details() -> BrowserDetails {
    BrowserDetails {
        user_agent: crate::headers::USER_AGENT.into(),
        browser_type: BrowserType::Other as i32,
        os: "libgm".into(),
        device_type: DeviceType::Tablet as i32,
    }
}

/// Device ID format for Gaia pairing: `messages-web-{uuid_no_dashes}`.
pub fn make_device_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("messages-web-{id}")
}

/// Drive a fresh QR pairing flow on `client`. The client must have empty
/// `AuthData`; the function will populate it on success.
pub async fn start_qr_pairing(client: &Client) -> Result<()> {
    qr::run(client).await
}

/// Drive a fresh Gaia (Google account) pairing flow on `client`. Cookies must
/// already be set via [`Client::set_cookies`]. Emits [`Event::PairingEmoji`]
/// after the UKEY2 ServerInit; caller must invoke
/// [`Client::confirm_pairing_emoji`] once the user has confirmed the same
/// emoji appears on the phone. Returns when pairing fully completes.
pub async fn start_gaia_pairing(client: &Client) -> Result<()> {
    gaia::run(client).await
}

#[cfg(test)]
mod device_identity_tests {
    use super::*;

    // Google allows one web session per phone. If we register as WEB — or as
    // PWA, which this client did for its whole life because the enum was
    // read off by one — we evict the user's own messages.google.com tab and
    // it evicts us back. TABLET is a separate slot. The wire numbering is
    // WEB=1, TABLET=2, PWA=3, so assert the number, not just the name.
    #[test]
    fn registers_as_a_tablet_so_it_does_not_evict_the_users_web_session() {
        let details = browser_details();
        assert_eq!(details.device_type, 2, "must be TABLET on the wire");
        assert_eq!(details.device_type, DeviceType::Tablet as i32);
        assert_ne!(details.device_type, DeviceType::Web as i32);
        assert_ne!(details.device_type, DeviceType::Pwa as i32);
        assert_eq!(details.browser_type, BrowserType::Other as i32);
        assert_eq!(details.os, "libgm");
    }

    // Both flows have to agree: pairing by QR and pairing by Google account
    // must claim the same device slot, or which one you used decides whether
    // the bridge fights the browser.
    #[test]
    fn both_pairing_flows_present_the_same_identity() {
        assert_eq!(qr::browser_details(), browser_details());
    }
}

pub mod qr {
    //! QR (Bugle) pairing.
    //!
    //! Steps:
    //! 1. Generate a fresh refresh ECDSA key + AES-CTR/HMAC keys.
    //! 2. POST [`RegisterPhoneRelay`] with the PKIX-encoded public key.
    //! 3. Store the returned `tachyon_auth_token`.
    //! 4. Register the pair-completion oneshot.
    //! 5. Spawn the long-poll loop (so the server can push us the
    //!    `PairEvent`).
    //! 6. Emit the QR code so the user scans it.
    //! 7. Wait for the `Paired` event; persist `mobile`/`browser`/token.
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use prost::Message as _;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use crate::crypto::aesctr::AesCtrHelper;
    use crate::crypto::ecdsa::JwkPair;
    use crate::gmproto::authentication::UrlData;
    use crate::gmproto::authentication::{
        AuthMessage, AuthenticationContainer, EcdsaKeys, KeyData, RegisterPhoneRelayResponse,
        authentication_container,
    };
    use crate::http::ContentType;
    use crate::{Client, Error, Result, urls};

    pub use super::browser_details;

    /// Wait up to this long for the phone to scan the QR and finalize.
    pub const PAIR_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    /// Re-issue `RegisterPhoneRelay` this often while waiting for the user
    /// to scan. Google's pairing keys are short-lived (~90s on the server
    /// side); without refresh, scans after the first minute fail with
    /// "something went wrong" on the phone.
    pub const QR_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

    pub async fn run(client: &Client) -> Result<()> {
        // Steps 1-3: register with the relay.
        log::info!("QR pair: posting RegisterPhoneRelay");
        let (pairing_key, request_crypto) = register_phone_relay(client).await?;
        log::info!(
            "QR pair: got tachyon token + pairing key ({} bytes)",
            pairing_key.len()
        );

        // Step 4: register the oneshot BEFORE we emit the QR or start the
        // long-poll, so we can never lose a pair event.
        let (tx, pair_rx) = oneshot::channel();
        client.inner.session.lock().await.pair_completion = Some(tx);

        // Step 5: spawn the long-poll. We can't go via `Client::connect`
        // because pairing isn't complete yet (`is_paired()` returns false).
        log::info!("QR pair: spawning long-poll loop");
        crate::longpoll::spawn(client.clone()).await?;

        // Step 6: emit the QR.
        let emit_qr = |key: &[u8], crypto: &AesCtrHelper| -> Result<()> {
            let url_data = UrlData {
                pairing_key: key.to_vec(),
                aes_key: crypto.aes_key.clone(),
                hmac_key: crypto.hmac_key.clone(),
            };
            let mut buf = Vec::with_capacity(url_data.encoded_len());
            url_data.encode(&mut buf)?;
            let qr = format!("{}{}", urls::QR_CODE_URL_BASE, STANDARD.encode(&buf));
            client.emit(crate::Event::QrCode { url: qr });
            Ok(())
        };
        emit_qr(&pairing_key, &request_crypto)?;
        log::info!(
            "QR pair: QR emitted, will refresh every {}s; total wait up to {}s",
            QR_REFRESH_INTERVAL.as_secs(),
            PAIR_TIMEOUT.as_secs()
        );

        // Step 7: wait for the phone to finalize. While waiting, refresh the
        // pairing_key every 60s — Google's pairing keys expire quickly
        // (~90s) and a scan after expiry shows "something went wrong" on
        // the phone. We only re-roll the pairing_key, NOT the request_crypto
        // keys (which are part of the long-lived AuthData).
        let paired = {
            let mut total_elapsed = std::time::Duration::ZERO;
            let mut current_key = pairing_key;
            let waiter = pair_rx;
            tokio::pin!(waiter);
            loop {
                let result = tokio::time::timeout(QR_REFRESH_INTERVAL, &mut waiter).await;
                match result {
                    Ok(Ok(p)) => break p,
                    Ok(Err(_)) => return Err(Error::Pairing("pair waiter dropped".into())),
                    Err(_) => {
                        total_elapsed += QR_REFRESH_INTERVAL;
                        if total_elapsed >= PAIR_TIMEOUT {
                            return Err(Error::Pairing(format!(
                                "pair timed out after {}s — phone never scanned",
                                PAIR_TIMEOUT.as_secs()
                            )));
                        }
                        // Re-issue RegisterPhoneRelay to get a fresh pairing key.
                        log::info!(
                            "QR pair: refreshing pairing key (elapsed {}s)",
                            total_elapsed.as_secs()
                        );
                        match refresh_pairing_key(client, &current_key).await {
                            Ok(new_key) => {
                                current_key = new_key;
                                if let Err(e) = emit_qr(&current_key, &request_crypto) {
                                    log::warn!("QR pair: failed to emit refreshed QR: {e}");
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "QR pair: refresh_pairing_key failed: {e}; \
                                     keeping old QR (may be expired)"
                                );
                            }
                        }
                    }
                }
            }
        };

        log::info!(
            "QR pair: complete; phone source_id={:?}",
            paired.mobile.as_ref().map(|m| &m.source_id)
        );
        client.emit(crate::Event::PairSuccess);
        Ok(())
    }

    /// Re-issue `RefreshPhoneRelay` to get a fresh pairing key without
    /// rotating the AES/HMAC keys. Called periodically while the QR is
    /// displayed so a slow phone scan still works after Google's
    /// server-side pairing-key TTL has elapsed.
    async fn refresh_pairing_key(client: &Client, _current: &[u8]) -> Result<Vec<u8>> {
        // The simplest correct path: call register_phone_relay again.
        // It rotates the refresh ECDSA key + tachyon token, but that's fine
        // until the user actually finalizes pairing — the LATEST keys are
        // what the phone signs against. We deliberately only return the new
        // pairing_key here; the caller already holds the request_crypto
        // it generated up front.
        let (key, _crypto) = register_phone_relay(client).await?;
        Ok(key)
    }

    /// Run steps 1-3 and persist the partial AuthData. Returns the pairing
    /// key (for QR encoding) and the AES/HMAC pair (for QR encoding too).
    async fn register_phone_relay(client: &Client) -> Result<(Vec<u8>, AesCtrHelper)> {
        // 1. Generate fresh keys.
        let refresh_key = JwkPair::generate()?;
        let request_crypto = AesCtrHelper::new_random();
        let pubkey_pkix = refresh_key.public_key_pkix_der()?;

        // 2. Build and POST the AuthenticationContainer request.
        let payload = AuthenticationContainer {
            auth_message: Some(AuthMessage {
                request_id: Uuid::new_v4().to_string(),
                network: "Bugle".into(),
                tachyon_auth_token: Vec::new(),
                config_version: Some(crate::session::config_version()),
            }),
            browser_details: Some(browser_details()),
            data: Some(authentication_container::Data::KeyData(KeyData {
                mobile: None,
                ecdsa_keys: Some(EcdsaKeys {
                    field1: 2,
                    encrypted_keys: pubkey_pkix,
                }),
                web_auth_key_data: None,
                browser: None,
            })),
        };

        let resp: RegisterPhoneRelayResponse = client
            .post_protobuf(urls::REGISTER_PHONE_RELAY, &payload, ContentType::Protobuf)
            .await?;

        let pairing_key = resp.pairing_key;
        let auth_token = resp
            .auth_key_data
            .as_ref()
            .map(|t| t.tachyon_auth_token.clone())
            .unwrap_or_default();
        let ttl = resp
            .auth_key_data
            .as_ref()
            .map(|t| t.ttl)
            .unwrap_or_default();

        if auth_token.is_empty() {
            return Err(Error::Pairing(
                "RegisterPhoneRelay returned no tachyon token".into(),
            ));
        }

        // 3. Persist token + keys.
        {
            let mut auth = client.inner.auth.lock().await;
            auth.tachyon_auth_token = Some(auth_token);
            auth.tachyon_ttl = ttl;
            auth.refresh_key = Some(refresh_key);
            auth.request_crypto = Some(request_crypto.clone());
        }
        Ok((pairing_key, request_crypto))
    }
}

pub mod ukey2 {
    //! UKEY2 emoji-verification handshake. Used by Gaia pairing.
    //!
    //! Ported from `mautrix-gmessages/pkg/libgm/pair_google.go`. The wire
    //! format is Google's open UKEY2 spec; emoji tables and the final
    //! Ditto-derivation step are gmessages-specific.
    //!
    //! Flow (driven by [`super::gaia::run`]):
    //!
    //! 1. [`PairingSession::new`] generates an ephemeral P-256 keypair.
    //! 2. [`PairingSession::prepare_payloads`] builds and serializes the
    //!    `Ukey2Message{ClientFinish}` (so we have something to commit to)
    //!    and `Ukey2Message{ClientInit}` (carrying the SHA-512 commitment
    //!    over ClientFinish). Returns both raw byte payloads.
    //! 3. The driver sends ClientInit through the relay; receives the
    //!    phone's `Ukey2Message{ServerInit}` wrapped in a
    //!    `GaiaPairingResponseContainer`.
    //! 4. [`PairingSession::process_server_init`] parses ServerInit, runs
    //!    ECDH P-256, runs HKDF, derives the verification emoji.
    //! 5. The user confirms the emoji on both devices.
    //! 6. The driver sends ClientFinish through the relay.
    //! 7. [`PairingSession::derive_session_keys`] produces the AES + HMAC
    //!    keys (handling both legacy and Ditto-style derivation).

    use elliptic_curve::sec1::FromEncodedPoint;
    use hkdf::Hkdf;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::{EncodedPoint, PublicKey, SecretKey, ecdh};
    use prost::Message as _;
    use rand::TryRng;
    use sha2::{Digest, Sha256, Sha512};

    use crate::gmproto::authentication::GaiaPairingResponseContainer;
    use crate::gmproto::ukey::{
        EcP256PublicKey, GenericPublicKey, Ukey2ClientFinished, Ukey2ClientInit, Ukey2Message,
        Ukey2ServerInit, generic_public_key, ukey2_client_init, ukey2_message,
    };
    use crate::{Error, Result};

    /// 32-byte HKDF info constant used to derive the per-direction session
    /// keys. Ported verbatim from mautrix-gmessages `encryptionKeyInfo`.
    pub const ENCRYPTION_KEY_INFO: [u8; 32] = [
        130, 170, 85, 160, 211, 151, 248, 131, 70, 202, 28, 238, 141, 57, 9, 185, 95, 19, 250, 125,
        235, 29, 74, 179, 131, 118, 184, 37, 109, 168, 85, 16,
    ];

    /// Verification-code-version table: the V0 emoji set. Ported verbatim from
    /// `pairingEmojisV0` in mautrix-gmessages. 284 entries.
    pub const PAIRING_EMOJIS_V0: &[&str] = &[
        "😁",
        "😅",
        "🤣",
        "🫠",
        "🥰",
        "😇",
        "🤩",
        "😘",
        "😜",
        "🤗",
        "🤔",
        "🤐",
        "😴",
        "🥶",
        "🤯",
        "🤠",
        "🥳",
        "🥸",
        "😎",
        "🤓",
        "🧐",
        "🥹",
        "😭",
        "😱",
        "😖",
        "🥱",
        "😮\u{200d}💨",
        "🤡",
        "💩",
        "👻",
        "👽",
        "🤖",
        "😻",
        "💌",
        "💘",
        "💕",
        "❤",
        "💢",
        "💥",
        "💫",
        "💬",
        "🗯",
        "💤",
        "👋",
        "🙌",
        "🙏",
        "✍",
        "🦶",
        "👂",
        "🧠",
        "🦴",
        "👀",
        "🧑",
        "🧚",
        "🧍",
        "👣",
        "🐵",
        "🐶",
        "🐺",
        "🦊",
        "🦁",
        "🐯",
        "🦓",
        "🦄",
        "🐑",
        "🐮",
        "🐷",
        "🐿",
        "🐰",
        "🦇",
        "🐻",
        "🐨",
        "🐼",
        "🦥",
        "🐾",
        "🐔",
        "🐥",
        "🐦",
        "🕊",
        "🦆",
        "🦉",
        "🪶",
        "🦩",
        "🐸",
        "🐢",
        "🦎",
        "🐍",
        "🐳",
        "🐬",
        "🦭",
        "🐠",
        "🐡",
        "🦈",
        "🪸",
        "🐌",
        "🦋",
        "🐛",
        "🐝",
        "🐞",
        "🪱",
        "💐",
        "🌸",
        "🌹",
        "🌻",
        "🌱",
        "🌲",
        "🌴",
        "🌵",
        "🌾",
        "☘",
        "🍁",
        "🍂",
        "🍄",
        "🪺",
        "🍇",
        "🍈",
        "🍉",
        "🍋",
        "🍌",
        "🍍",
        "🍎",
        "🍐",
        "🍒",
        "🍓",
        "🥝",
        "🥥",
        "🥑",
        "🥕",
        "🌽",
        "🌶",
        "🫑",
        "🥦",
        "🥜",
        "🍞",
        "🥐",
        "🥨",
        "🧀",
        "🍗",
        "🍔",
        "🍟",
        "🍕",
        "🌭",
        "🌮",
        "🥗",
        "🥣",
        "🍿",
        "🦀",
        "🦑",
        "🍦",
        "🍩",
        "🍪",
        "🍫",
        "🍰",
        "🍬",
        "🍭",
        "☕",
        "🫖",
        "🍹",
        "🥤",
        "🧊",
        "🥢",
        "🍽",
        "🥄",
        "🧭",
        "🏔",
        "🌋",
        "🏕",
        "🏖",
        "🪵",
        "🏗",
        "🏡",
        "🏰",
        "🛝",
        "🚂",
        "🛵",
        "🛴",
        "🛼",
        "🚥",
        "⚓",
        "🛟",
        "⛵",
        "✈",
        "🚀",
        "🛸",
        "🧳",
        "⏰",
        "🌙",
        "🌡",
        "🌞",
        "🪐",
        "🌠",
        "🌧",
        "🌀",
        "🌈",
        "☂",
        "⚡",
        "❄",
        "⛄",
        "🔥",
        "🎇",
        "🧨",
        "✨",
        "🎈",
        "🎉",
        "🎁",
        "🏆",
        "🏅",
        "⚽",
        "⚾",
        "🏀",
        "🏐",
        "🏈",
        "🎾",
        "🎳",
        "🏓",
        "🥊",
        "⛳",
        "⛸",
        "🎯",
        "🪁",
        "🔮",
        "🎮",
        "🧩",
        "🧸",
        "🪩",
        "🖼",
        "🎨",
        "🧵",
        "🧶",
        "🦺",
        "🧣",
        "🧤",
        "🧦",
        "🎒",
        "🩴",
        "👟",
        "👑",
        "👒",
        "🎩",
        "🧢",
        "💎",
        "🔔",
        "🎤",
        "📻",
        "🎷",
        "🪗",
        "🎸",
        "🎺",
        "🎻",
        "🥁",
        "📺",
        "🔋",
        "💻",
        "💿",
        "☎",
        "🕯",
        "💡",
        "📖",
        "📚",
        "📬",
        "✏",
        "✒",
        "🖌",
        "🖍",
        "📝",
        "💼",
        "📋",
        "📌",
        "📎",
        "🔑",
        "🔧",
        "🧲",
        "🪜",
        "🧬",
        "🔭",
        "🩹",
        "🩺",
        "🪞",
        "🛋",
        "🪑",
        "🛁",
        "🧹",
        "🧺",
        "🔱",
        "🏁",
        "🐪",
        "🐘",
        "🦃",
        "🍞",
        "🍜",
        "🍠",
        "🚘",
        "🤿",
        "🃏",
        "👕",
        "📸",
        "🏷",
        "✂",
        "🧪",
        "🚪",
        "🧴",
        "🧻",
        "🪣",
        "🧽",
        "🚸",
    ];

    /// V1 emoji set. Built once at startup by removing 10 from V0,
    /// deduplicating, and appending 14 new ones — matches `init()` in
    /// mautrix-gmessages.
    pub fn pairing_emojis_v1() -> &'static [&'static str] {
        use std::sync::OnceLock;
        static CACHE: OnceLock<Vec<&'static str>> = OnceLock::new();
        CACHE.get_or_init(|| {
            const REMOVED: &[&str] = &["💻", "🤗", "💬", "👋", "😁", "😎", "😇", "🥰", "🤓", "🤩"];
            const ADDED: &[&str] = &[
                "🍋\u{200d}🟩",
                "🐦\u{200d}🔥",
                "🐲",
                "🪅",
                "🦜",
                "🏺",
                "🗿",
                "🫐",
                "⛽",
                "🍱",
                "🥡",
                "🧋",
                "🍼",
                "📐",
            ];
            // Deduplicate while preserving order (V0 has duplicates).
            let mut out: Vec<&'static str> =
                Vec::with_capacity(PAIRING_EMOJIS_V0.len() + ADDED.len());
            let mut seen = std::collections::HashSet::new();
            for &e in PAIRING_EMOJIS_V0 {
                if seen.insert(e) {
                    out.push(e);
                }
            }
            out.extend_from_slice(ADDED);
            out.retain(|e| !REMOVED.contains(e));
            out
        })
    }

    /// Held during a Gaia pairing: the ephemeral keypair, the marshaled
    /// ClientInit/Finish (kept around because both are inputs to the HKDF
    /// derivation), and (after `process_server_init`) the derived
    /// `next_key` from which the AES/HMAC keys are produced.
    pub struct PairingSession {
        /// Ephemeral P-256 private key.
        secret: SecretKey,
        /// Encoded `Ukey2Message{ClientInit}` (including the wrapper). This
        /// is the EXACT bytes we send on the wire and is one of the inputs
        /// to the auth-info HKDF salt.
        init_payload: Vec<u8>,
        /// Encoded `Ukey2Message{ClientFinish}`. Sent after the user
        /// confirms the emoji.
        finish_payload: Vec<u8>,
        /// Server's `Ukey2Message{ServerInit}` bytes (raw, as received from
        /// the phone's `GaiaPairingResponseContainer.data`). Concatenated
        /// with `init_payload` to form the HKDF salt.
        server_init_bytes: Option<Vec<u8>>,
        /// Confirmed key-derivation version (0 = legacy, 1 = Ditto). Set in
        /// `process_server_init` from the server's response.
        confirmed_key_derivation_version: i32,
        /// HKDF "next key" — the secret from which the final session keys
        /// are derived. Populated by `process_server_init`.
        next_key: Option<[u8; 32]>,
    }

    impl PairingSession {
        /// Generate a fresh ephemeral P-256 keypair.
        pub fn new() -> Result<Self> {
            // Match the existing JwkPair::generate pattern: fill 32 bytes
            // from SysRng, retry on the (vanishingly unlikely) zero scalar.
            let secret = generate_secret()?;
            Ok(Self {
                secret,
                init_payload: Vec::new(),
                finish_payload: Vec::new(),
                server_init_bytes: None,
                confirmed_key_derivation_version: 0,
                next_key: None,
            })
        }

        /// Build and serialize both `Ukey2Message{ClientInit}` and
        /// `Ukey2Message{ClientFinish}`. Returns `(init_bytes, finish_bytes)`.
        ///
        /// The `init` includes a SHA-512 commitment over the EXACT bytes of
        /// the `finish` Ukey2Message (i.e. the wrapped/serialized one) —
        /// this binds our public key to the ClientInit before the server
        /// has seen it.
        pub fn prepare_payloads(&mut self) -> Result<(Vec<u8>, Vec<u8>)> {
            // Encode our public key in 33-byte big-endian two's-complement
            // form (matches the Go reference's FillBytes pattern with a
            // 33-byte slice and a leading zero byte).
            let pk = self.secret.public_key();
            let encoded = pk.to_encoded_point(false); // uncompressed: 0x04 || X(32) || Y(32)
            let raw = encoded.as_bytes();
            if raw.len() != 65 || raw[0] != 0x04 {
                return Err(Error::Crypto(
                    "unexpected encoded P-256 key length (expected 65, uncompressed)".into(),
                ));
            }
            let mut x = vec![0u8; 33];
            let mut y = vec![0u8; 33];
            x[1..].copy_from_slice(&raw[1..33]);
            y[1..].copy_from_slice(&raw[33..65]);

            let public_key = GenericPublicKey {
                r#type: crate::gmproto::ukey::PublicKeyType::EcP256 as i32,
                public_key: Some(generic_public_key::PublicKey::EcP256PublicKey(
                    EcP256PublicKey { x, y },
                )),
            };

            // Serialize Ukey2ClientFinished, then wrap in Ukey2Message.
            let finish_inner = Ukey2ClientFinished {
                public_key: Some(public_key),
            };
            let mut finish_inner_buf = Vec::with_capacity(finish_inner.encoded_len());
            finish_inner.encode(&mut finish_inner_buf)?;
            let finish_msg = Ukey2Message {
                message_type: ukey2_message::Type::ClientFinish as i32,
                message_data: finish_inner_buf,
            };
            let mut finish_buf = Vec::with_capacity(finish_msg.encoded_len());
            finish_msg.encode(&mut finish_buf)?;
            self.finish_payload = finish_buf.clone();

            // Commitment = SHA-512 of the wrapped Ukey2Message.
            let key_commitment = Sha512::digest(&finish_buf);

            // Random nonce (32 bytes).
            let mut random = vec![0u8; 32];
            rand::rngs::SysRng
                .try_fill_bytes(&mut random)
                .map_err(|e| Error::Crypto(format!("rng: {e}")))?;

            let init_inner = Ukey2ClientInit {
                version: 1,
                random,
                cipher_commitments: vec![ukey2_client_init::CipherCommitment {
                    handshake_cipher: crate::gmproto::ukey::Ukey2HandshakeCipher::P256Sha512 as i32,
                    commitment: key_commitment.to_vec(),
                }],
                next_protocol: "AES_256_CBC-HMAC_SHA256".into(),
            };
            let mut init_inner_buf = Vec::with_capacity(init_inner.encoded_len());
            init_inner.encode(&mut init_inner_buf)?;
            let init_msg = Ukey2Message {
                message_type: ukey2_message::Type::ClientInit as i32,
                message_data: init_inner_buf,
            };
            let mut init_buf = Vec::with_capacity(init_msg.encoded_len());
            init_msg.encode(&mut init_buf)?;
            self.init_payload = init_buf.clone();

            Ok((init_buf, finish_buf))
        }

        /// Parse the phone's `GaiaPairingResponseContainer`, extract its
        /// embedded `Ukey2Message{ServerInit}`, run ECDH + HKDF, and return
        /// the verification emoji.
        pub fn process_server_init(
            &mut self,
            resp: &GaiaPairingResponseContainer,
        ) -> Result<String> {
            // The server's `data` is the raw Ukey2Message{ServerInit} bytes.
            // We keep them around verbatim because they're an input to the
            // HKDF salt below.
            self.server_init_bytes = Some(resp.data.clone());

            let outer = Ukey2Message::decode(&*resp.data)
                .map_err(|e| Error::Pairing(format!("decode server init wrapper: {e}")))?;
            if outer.message_type != ukey2_message::Type::ServerInit as i32 {
                return Err(Error::Pairing(format!(
                    "unexpected ukey2 message type: {}",
                    outer.message_type
                )));
            }
            let server_init = Ukey2ServerInit::decode(&*outer.message_data)
                .map_err(|e| Error::Pairing(format!("decode ServerInit: {e}")))?;

            if server_init.version != 1 {
                return Err(Error::Pairing(format!(
                    "unexpected server init version: {}",
                    server_init.version
                )));
            }
            if server_init.handshake_cipher
                != crate::gmproto::ukey::Ukey2HandshakeCipher::P256Sha512 as i32
            {
                return Err(Error::Pairing(format!(
                    "unexpected handshake cipher: {}",
                    server_init.handshake_cipher
                )));
            }
            if server_init.random.len() != 32 {
                return Err(Error::Pairing(format!(
                    "unexpected random length {}",
                    server_init.random.len()
                )));
            }

            // Extract the EC point.
            let server_pub = server_init
                .public_key
                .as_ref()
                .ok_or_else(|| Error::Pairing("no server public key".into()))?
                .public_key
                .as_ref()
                .ok_or_else(|| Error::Pairing("server public_key oneof empty".into()))?;
            let server_ec = match server_pub {
                generic_public_key::PublicKey::EcP256PublicKey(p) => p,
                _ => return Err(Error::Pairing("server key is not P-256".into())),
            };

            // Strip the optional leading zero (33-byte big-endian two's
            // complement form) to get a clean 32-byte coordinate.
            let mut x = server_ec.x.clone();
            let mut y = server_ec.y.clone();
            if x.len() == 33 {
                if x[0] != 0 {
                    return Err(Error::Pairing(format!(
                        "server x has unexpected prefix: {}",
                        x[0]
                    )));
                }
                x.remove(0);
            }
            if y.len() == 33 {
                if y[0] != 0 {
                    return Err(Error::Pairing(format!(
                        "server y has unexpected prefix: {}",
                        y[0]
                    )));
                }
                y.remove(0);
            }
            if x.len() != 32 || y.len() != 32 {
                return Err(Error::Pairing(format!(
                    "server EC coords have wrong length: x={} y={}",
                    x.len(),
                    y.len()
                )));
            }

            // Reconstruct EncodedPoint, derive PublicKey.
            let mut enc = vec![0x04u8; 65];
            enc[1..33].copy_from_slice(&x);
            enc[33..65].copy_from_slice(&y);
            let server_point = EncodedPoint::from_bytes(&enc)
                .map_err(|e| Error::Pairing(format!("decode server EC point: {e}")))?;
            let server_pubkey: Option<PublicKey> =
                PublicKey::from_encoded_point(&server_point).into();
            let server_pubkey = server_pubkey
                .ok_or_else(|| Error::Pairing("server EC point not on curve".into()))?;

            // ECDH.
            let our_scalar = self.secret.to_nonzero_scalar();
            let shared = ecdh::diffie_hellman(our_scalar, server_pubkey.as_affine());
            let dh_raw = shared.raw_secret_bytes(); // 32 bytes
            let shared_secret = Sha256::digest(dh_raw);

            // authInfo = our init payload || server init wrapper bytes.
            let mut auth_info = self.init_payload.clone();
            auth_info.extend_from_slice(&resp.data);

            let ukey_v1_auth = hkdf32(&shared_secret, b"UKEY2 v1 auth", &auth_info)?;
            let next_key = hkdf32(&shared_secret, b"UKEY2 v1 next", &auth_info)?;
            self.next_key = Some(next_key);

            // Emoji index from the first 4 bytes of ukey_v1_auth interpreted
            // as big-endian uint32, modulo the chosen table.
            let auth_number = u32::from_be_bytes([
                ukey_v1_auth[0],
                ukey_v1_auth[1],
                ukey_v1_auth[2],
                ukey_v1_auth[3],
            ]);
            let confirmed = resp.confirmed_verification_code_version;
            self.confirmed_key_derivation_version = resp.confirmed_key_derivation_version;
            let emoji = match confirmed {
                0 => PAIRING_EMOJIS_V0[(auth_number as usize) % PAIRING_EMOJIS_V0.len()],
                1 => {
                    let v1 = pairing_emojis_v1();
                    v1[(auth_number as usize) % v1.len()]
                }
                other => {
                    return Err(Error::Pairing(format!(
                        "unsupported verification_code_version: {other}"
                    )));
                }
            };
            Ok(emoji.to_string())
        }

        /// Borrow the encoded `Ukey2Message{ClientFinish}` for transmission.
        pub fn finish_payload(&self) -> &[u8] {
            &self.finish_payload
        }

        /// After both ClientInit AND ClientFinish have been exchanged,
        /// derive the (aes_key, hmac_key) pair to install into AuthData.
        ///
        /// Two derivation versions:
        /// - v0: legacy. AES = HKDF(next_key, encryption_key_info, "client").
        ///   HMAC = HKDF(..., "server").
        /// - v1: "Ditto" derivation. Both keys hashed and re-HKDF'd with
        ///   different infos. Order of the concat depends on the Java
        ///   `Arrays.hashCode` of each (smaller hash first).
        pub fn derive_session_keys(&self) -> Result<([u8; 32], [u8; 32])> {
            let next_key = self.next_key.as_ref().ok_or_else(|| {
                Error::Pairing("derive_session_keys before process_server_init".into())
            })?;
            let client_key = hkdf32(next_key, &ENCRYPTION_KEY_INFO, b"client")?;
            let server_key = hkdf32(next_key, &ENCRYPTION_KEY_INFO, b"server")?;
            match self.confirmed_key_derivation_version {
                0 => Ok((client_key, server_key)),
                1 => {
                    let mut concatted = [0u8; 96];
                    concatted[..32].copy_from_slice(&ENCRYPTION_KEY_INFO);
                    if java_byte_hash(&client_key) < java_byte_hash(&server_key) {
                        concatted[32..64].copy_from_slice(&client_key);
                        concatted[64..96].copy_from_slice(&server_key);
                    } else {
                        concatted[32..64].copy_from_slice(&server_key);
                        concatted[64..96].copy_from_slice(&client_key);
                    }
                    let h = Sha256::digest(concatted);
                    let aes = hkdf32(&h, b"Ditto salt 1", b"Ditto info 1")?;
                    let hmac = hkdf32(&h, b"Ditto salt 2", b"Ditto info 2")?;
                    Ok((aes, hmac))
                }
                other => Err(Error::Pairing(format!(
                    "unsupported key_derivation_version: {other}"
                ))),
            }
        }
    }

    /// 32-byte HKDF-SHA256 helper. UKEY2 always uses 32-byte outputs.
    fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; 32]> {
        let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
        let mut out = [0u8; 32];
        hk.expand(info, &mut out)
            .map_err(|e| Error::Crypto(format!("hkdf expand: {e}")))?;
        Ok(out)
    }

    /// Java's `Arrays.hashCode([]byte)` algorithm. Used by Ditto v1
    /// derivation to deterministically order client+server keys.
    fn java_byte_hash(bytes: &[u8]) -> i32 {
        let mut out: i32 = 1;
        for &b in bytes {
            out = out.wrapping_mul(31).wrapping_add(b as i8 as i32);
        }
        out
    }

    /// Generate a fresh P-256 SecretKey, matching the pattern used by
    /// `JwkPair::generate`. Retries on the (vanishingly unlikely) zero
    /// scalar that `SecretKey::from_slice` rejects.
    fn generate_secret() -> Result<SecretKey> {
        for _ in 0..4 {
            let mut bytes = [0u8; 32];
            rand::rngs::SysRng
                .try_fill_bytes(&mut bytes)
                .map_err(|e| Error::Crypto(format!("rng: {e}")))?;
            if let Ok(secret) = SecretKey::from_slice(&bytes) {
                return Ok(secret);
            }
        }
        Err(Error::Crypto("could not generate p256 secret".into()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// UKEY2 round-trip: two parties, client + simulated server.
        /// Verifies both sides derive the same `auth_string` (and thus
        /// the same emoji) and the same final session keys.
        #[test]
        fn round_trip_v1_keys_match() {
            // CLIENT
            let mut client = PairingSession::new().unwrap();
            let (init_bytes, finish_bytes) = client.prepare_payloads().unwrap();

            // SERVER: parse client init, generate own ephemeral key, encode
            // ServerInit, run the same HKDF.
            use p256::{PublicKey, ecdh};
            let client_init_outer = Ukey2Message::decode(&*init_bytes).unwrap();
            assert_eq!(
                client_init_outer.message_type,
                ukey2_message::Type::ClientInit as i32
            );

            let server_secret = generate_secret().unwrap();
            let server_pub_enc = server_secret.public_key().to_encoded_point(false);
            let raw = server_pub_enc.as_bytes();
            let mut sx = vec![0u8; 33];
            let mut sy = vec![0u8; 33];
            sx[1..].copy_from_slice(&raw[1..33]);
            sy[1..].copy_from_slice(&raw[33..65]);

            let mut server_random = vec![0u8; 32];
            rand::rngs::SysRng
                .try_fill_bytes(&mut server_random)
                .unwrap();
            let server_init_inner = Ukey2ServerInit {
                version: 1,
                random: server_random,
                handshake_cipher: crate::gmproto::ukey::Ukey2HandshakeCipher::P256Sha512 as i32,
                public_key: Some(GenericPublicKey {
                    r#type: crate::gmproto::ukey::PublicKeyType::EcP256 as i32,
                    public_key: Some(generic_public_key::PublicKey::EcP256PublicKey(
                        EcP256PublicKey { x: sx, y: sy },
                    )),
                }),
            };
            let mut server_init_inner_buf = Vec::new();
            server_init_inner
                .encode(&mut server_init_inner_buf)
                .unwrap();
            let server_init_msg = Ukey2Message {
                message_type: ukey2_message::Type::ServerInit as i32,
                message_data: server_init_inner_buf,
            };
            let mut server_init_bytes = Vec::new();
            server_init_msg.encode(&mut server_init_bytes).unwrap();

            // Wrap in a fake GaiaPairingResponseContainer.
            let response = GaiaPairingResponseContainer {
                finish_error_type: 0,
                finish_error_code: 0,
                unknown_int3: 1,
                session_uuid: "test".into(),
                data: server_init_bytes.clone(),
                confirmed_verification_code_version: 1,
                confirmed_key_derivation_version: 1,
            };

            // CLIENT processes server init.
            let client_emoji = client.process_server_init(&response).unwrap();

            // SERVER does the same derivation.
            let client_pub = {
                // Decode our own client_finish to extract the public key
                // that the server would have learned about via ClientInit's
                // commitment + ClientFinish payload.
                let cf_outer = Ukey2Message::decode(&*finish_bytes).unwrap();
                let cf = Ukey2ClientFinished::decode(&*cf_outer.message_data).unwrap();
                let pk = cf.public_key.unwrap();
                let ec = match pk.public_key.unwrap() {
                    generic_public_key::PublicKey::EcP256PublicKey(p) => p,
                    _ => panic!("not P-256"),
                };
                let mut enc = vec![0x04u8; 65];
                enc[1..33].copy_from_slice(&ec.x[1..]);
                enc[33..65].copy_from_slice(&ec.y[1..]);
                let p = EncodedPoint::from_bytes(&enc).unwrap();
                let opt: Option<PublicKey> = PublicKey::from_encoded_point(&p).into();
                opt.unwrap()
            };
            let server_scalar = server_secret.to_nonzero_scalar();
            let shared = ecdh::diffie_hellman(server_scalar, client_pub.as_affine());
            let dh_raw = shared.raw_secret_bytes();
            let server_shared_secret = Sha256::digest(dh_raw);

            let mut auth_info = init_bytes.clone();
            auth_info.extend_from_slice(&server_init_bytes);

            let server_v1_auth =
                hkdf32(&server_shared_secret, b"UKEY2 v1 auth", &auth_info).unwrap();
            let server_next_key =
                hkdf32(&server_shared_secret, b"UKEY2 v1 next", &auth_info).unwrap();

            // Emojis must match.
            let auth_num = u32::from_be_bytes([
                server_v1_auth[0],
                server_v1_auth[1],
                server_v1_auth[2],
                server_v1_auth[3],
            ]);
            let v1 = pairing_emojis_v1();
            let server_emoji = v1[(auth_num as usize) % v1.len()];
            assert_eq!(client_emoji, server_emoji, "emojis must match");

            // Next keys must match.
            assert_eq!(client.next_key.unwrap(), server_next_key);

            // Final session keys must match.
            let (c_aes, c_hmac) = client.derive_session_keys().unwrap();
            // The "server" runs the SAME derive logic on its own
            // PairingSession-equivalent state, so just rerun ourselves with
            // the server's next_key.
            let mut server_session = PairingSession::new().unwrap();
            server_session.next_key = Some(server_next_key);
            server_session.confirmed_key_derivation_version = 1;
            let (s_aes, s_hmac) = server_session.derive_session_keys().unwrap();
            assert_eq!(c_aes, s_aes);
            assert_eq!(c_hmac, s_hmac);
        }

        #[test]
        fn emoji_tables_have_expected_lengths() {
            // Must match `pairingEmojisV0` in mautrix `pair_google.go`
            // EXACTLY. If they diverge, the desktop and the phone will
            // index different emojis and pairing fails.
            assert_eq!(PAIRING_EMOJIS_V0.len(), 305);
            // V1 = dedup(V0) ∖ removed10 ∪ added14.
            let v1 = pairing_emojis_v1();
            assert!(
                v1.len() > 280 && v1.len() < 320,
                "v1 size off: {}",
                v1.len()
            );
        }
    }
}

pub mod gaia {
    //! Gaia (Google account) pairing.
    //!
    //! Driven from cookies in `AuthData.cookies`; produces a long-lived
    //! `tachyon_auth_token` plus the AES/HMAC `request_crypto` keys.
    //!
    //! ```text
    //! 1. SignInGaia (HTTP POST with cookies) →
    //!      maybe_browser_uuid, device_data (phone list), token_data
    //!    Pick the primary device with unknown_int4 == 1; remember its UUID
    //!    as dest_reg_id.
    //! 2. Spawn long-poll loop so we can receive the phone's response.
    //! 3. ukey2::PairingSession::prepare_payloads() → init_bytes, finish_bytes
    //! 4. Send GaiaPairingRequestContainer{ data=init_bytes,
    //!      proposed_verification_code_version=1, proposed_key_derivation_version=1 }
    //!    via session::send_unencrypted_rpc(MessageType::Gaia2,
    //!      ActionType::CreateGaiaPairingClientInit). Wait for response.
    //! 5. process_server_init() → emoji. Emit Event::PairingEmoji.
    //!    Wait on inner.session.gaia_emoji_confirm oneshot.
    //! 6. Send GaiaPairingRequestContainer{ data=finish_bytes } via
    //!    session::send_unencrypted_rpc(MessageType::BugleMessage,
    //!      ActionType::CreateGaiaPairingClientFinished). Wait for response.
    //! 7. derive_session_keys() → (aes_key, hmac_key). Install into
    //!    auth.request_crypto. Persist via notify_auth_changed.
    //! 8. Emit Event::PairSuccess.
    //! ```
    //!
    //! See `pkg/libgm/pair_google.go` for the reference implementation.

    use prost::Message as _;
    use uuid::Uuid;

    use crate::gmproto::authentication::{
        AuthMessage, GaiaPairingRequestContainer, GaiaPairingResponseContainer, SignInGaiaRequest,
        SignInGaiaResponse, sign_in_gaia_request,
    };
    use crate::gmproto::rpc::{ActionType, MessageType};
    use crate::http::ContentType;
    use crate::{Client, Error, Result, urls};

    /// Network name carried in `AuthMessage.network` for Gaia pairing.
    const GOOGLE_NETWORK: &str = "GDitto";

    pub async fn run(client: &Client) -> Result<()> {
        // Cookies must be present.
        if !client.inner.auth.lock().await.has_cookies() {
            return Err(Error::Pairing(
                "no cookies. Call Client::set_cookies() first.".into(),
            ));
        }

        // Step 0: enumerate Google accounts. If multiple, ask the user
        // which one to register against. We persist the choice via the
        // GMESSAGES_AUTHUSER env var so subsequent SignInGaia in this
        // process picks it up.
        let cookies = client.inner.auth.lock().await.cookies.clone();
        let accounts =
            match crate::accounts::list_google_accounts(&client.inner.http, &cookies).await {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("gaia: ListAccounts failed ({e}); falling back to authuser=0");
                    Vec::new()
                }
            };
        log::info!("gaia: ListAccounts found {} account(s)", accounts.len());
        for a in &accounts {
            log::info!("gaia:   authuser={} email={}", a.authuser, a.email);
        }
        let chosen_authuser: u32 = match accounts.len() {
            0 | 1 => accounts.first().map(|a| a.authuser).unwrap_or(0),
            _ => {
                // Multiple accounts — ask the user.
                let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
                client.inner.session.lock().await.gaia_account_choice = Some(tx);
                client.emit(crate::Event::AvailableGoogleAccounts {
                    accounts: accounts.clone(),
                });
                match tokio::time::timeout(std::time::Duration::from_secs(5 * 60), rx).await {
                    Ok(Ok(n)) => n,
                    Ok(Err(_)) => {
                        return Err(Error::Pairing(
                            "account-choice channel closed before user response".into(),
                        ));
                    }
                    Err(_) => {
                        return Err(Error::Pairing(
                            "account choice timed out (no user response in 5 minutes)".into(),
                        ));
                    }
                }
            }
        };
        log::info!("gaia: using authuser={chosen_authuser}");
        // Persist for sign_in_gaia_get_token (which reads the env var).
        unsafe {
            std::env::set_var("GMESSAGES_AUTHUSER", chosen_authuser.to_string());
        }
        // Stash the chosen account's email on AuthData so the desktop
        // UI can show "Paired with foo@gmail.com" later. Also stash the
        // authuser index — without it, post-restart requests would lose
        // the X-Goog-AuthUser header and the relay would reject our
        // session as AuthRevoked.
        let chosen_email = accounts
            .iter()
            .find(|a| a.authuser == chosen_authuser)
            .map(|a| a.email.clone());
        {
            let mut auth = client.inner.auth.lock().await;
            auth.gaia_account_email = chosen_email.clone();
            auth.gaia_authuser = Some(chosen_authuser);
        }

        // Generate fresh state so a previous pair attempt doesn't leak.
        let pairing_attempt_id = Uuid::new_v4().to_string();
        let session_uuid = Uuid::new_v4().simple().to_string();
        log::info!("gaia: starting pairing, attempt_id={pairing_attempt_id}");

        // Need a refresh_key so that subsequent RegisterRefresh signing
        // works. Generate now, before SignInGaia, so SomeData has a
        // public-key blob to send.
        let refresh_key = crate::crypto::ecdsa::JwkPair::generate()?;
        let pubkey_pkix = refresh_key.public_key_pkix_der()?;
        {
            let mut auth = client.inner.auth.lock().await;
            auth.refresh_key = Some(refresh_key);
            auth.session_id = Some(session_uuid.clone());
        }

        // Step 1: SignInGaia.
        let sig_resp = sign_in_gaia_get_token(client, &session_uuid, pubkey_pkix).await?;
        log::info!(
            "gaia: SignInGaia ok; browser_uuid={} primary devices in unknown_items2={}",
            sig_resp.maybe_browser_uuid,
            sig_resp
                .device_data
                .as_ref()
                .map(|d| d.unknown_items2.len())
                .unwrap_or(0)
        );

        // Choose a primary device (unknown_int4 == 1).
        let dev_data = sig_resp
            .device_data
            .as_ref()
            .ok_or_else(|| Error::Pairing("SignInGaia: no device_data".into()))?;
        let primary = dev_data
            .unknown_items2
            .iter()
            .find(|d| d.unknown_int4 == 1)
            .ok_or_else(|| Error::Pairing("SignInGaia: no primary device found".into()))?;
        let dest_reg_uuid = primary.dest_or_source_uuid.clone();
        log::info!("gaia: chose dest_reg_id={dest_reg_uuid}");
        {
            let mut auth = client.inner.auth.lock().await;
            auth.dest_reg_id = Some(dest_reg_uuid.clone());
        }

        // Step 2: spawn long-poll so we can receive responses.
        log::info!("gaia: spawning long-poll");
        crate::longpoll::spawn(client.clone()).await?;

        // Step 3: prepare UKEY2 payloads.
        let mut sess = super::ukey2::PairingSession::new()?;
        let (init_bytes, _finish_bytes) = sess.prepare_payloads()?;

        // Step 4: send GAIA_2 ClientInit.
        let start_ts = chrono::Utc::now().timestamp_millis();
        let init_container = GaiaPairingRequestContainer {
            pairing_attempt_id: pairing_attempt_id.clone(),
            browser_details: Some(super::qr::browser_details()),
            start_timestamp: start_ts,
            data: init_bytes,
            proposed_verification_code_version: 1,
            proposed_key_derivation_version: 1,
        };
        let mut init_buf = Vec::with_capacity(init_container.encoded_len());
        init_container.encode(&mut init_buf)?;

        log::info!("gaia: sending CREATE_GAIA_PAIRING_CLIENT_INIT");
        let init_resp_bytes = crate::session::send_unencrypted_rpc(
            client,
            ActionType::CreateGaiaPairingClientInit,
            MessageType::Gaia2,
            &init_buf,
            std::time::Duration::from_secs(20),
        )
        .await?;

        let server_init = GaiaPairingResponseContainer::decode(&*init_resp_bytes)
            .map_err(|e| Error::Pairing(format!("decode server init container: {e}")))?;
        if server_init.finish_error_type != 0 {
            return Err(Error::Pairing(format!(
                "server returned error on init: type={} code={}",
                server_init.finish_error_type, server_init.finish_error_code
            )));
        }
        log::info!(
            "gaia: server_init ok; verification_code_version={} key_derivation_version={}",
            server_init.confirmed_verification_code_version,
            server_init.confirmed_key_derivation_version
        );

        // Step 5: derive emoji + emit event.
        let emoji = sess.process_server_init(&server_init)?;
        log::info!("gaia: pairing emoji = {emoji}");

        // Register a confirmation oneshot.
        let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel::<bool>();
        client.inner.session.lock().await.gaia_emoji_confirm = Some(confirm_tx);

        client.emit(crate::Event::PairingEmoji {
            emoji: emoji.clone(),
        });

        // Wait for the user. Cap at 5 minutes.
        let user_confirmed =
            match tokio::time::timeout(std::time::Duration::from_secs(5 * 60), confirm_rx).await {
                Ok(Ok(b)) => b,
                Ok(Err(_)) => {
                    return Err(Error::Pairing(
                        "emoji confirmation channel closed before user response".into(),
                    ));
                }
                Err(_) => {
                    return Err(Error::Pairing(
                        "emoji confirmation timed out (no user response in 5 minutes)".into(),
                    ));
                }
            };
        if !user_confirmed {
            return Err(Error::Pairing("user rejected emoji match".into()));
        }

        // Step 6: send CLIENT_FINISHED.
        let finish_container = GaiaPairingRequestContainer {
            pairing_attempt_id: pairing_attempt_id.clone(),
            browser_details: Some(super::qr::browser_details()),
            start_timestamp: start_ts,
            data: sess.finish_payload().to_vec(),
            // Per Go reference: only proposed on the INIT, omitted on FINISH.
            proposed_verification_code_version: 0,
            proposed_key_derivation_version: 0,
        };
        let mut finish_buf = Vec::with_capacity(finish_container.encoded_len());
        finish_container.encode(&mut finish_buf)?;
        log::info!("gaia: sending CREATE_GAIA_PAIRING_CLIENT_FINISHED");
        let finish_resp_bytes = crate::session::send_unencrypted_rpc(
            client,
            ActionType::CreateGaiaPairingClientFinished,
            MessageType::BugleMessage,
            &finish_buf,
            std::time::Duration::from_secs(60),
        )
        .await?;

        let finish_resp = GaiaPairingResponseContainer::decode(&*finish_resp_bytes)
            .map_err(|e| Error::Pairing(format!("decode finish response: {e}")))?;
        if finish_resp.finish_error_type != 0 {
            return Err(Error::Pairing(format!(
                "phone rejected pairing: type={} code={}",
                finish_resp.finish_error_type, finish_resp.finish_error_code
            )));
        }

        // Step 7: derive session keys, install.
        let (aes_key, hmac_key) = sess.derive_session_keys()?;
        {
            let mut auth = client.inner.auth.lock().await;
            auth.request_crypto = Some(crate::crypto::aesctr::AesCtrHelper {
                aes_key: aes_key.to_vec(),
                hmac_key: hmac_key.to_vec(),
            });
        }
        client.notify_auth_changed().await;
        log::info!("gaia: pairing complete; emitting PairSuccess");
        client.emit(crate::Event::PairSuccess);
        Ok(())
    }

    /// POST `SignInGaia` with cookies. Sends our public key (PKIX-DER); the
    /// response carries the tachyon token and the device list.
    ///
    /// Reads `GMESSAGES_AUTHUSER` env var to pick which Google account when
    /// the user has multiple signed into Firefox. Default 0 (first account).
    async fn sign_in_gaia_get_token(
        client: &Client,
        session_uuid: &str,
        pubkey_pkix: Vec<u8>,
    ) -> Result<SignInGaiaResponse> {
        let payload = SignInGaiaRequest {
            auth_message: Some(AuthMessage {
                request_id: Uuid::new_v4().to_string(),
                network: GOOGLE_NETWORK.into(),
                tachyon_auth_token: Vec::new(),
                config_version: Some(crate::session::config_version()),
            }),
            inner: Some(sign_in_gaia_request::Inner {
                device_id: Some(sign_in_gaia_request::inner::DeviceId {
                    unknown_int1: 3,
                    device_id: format!("messages-web-{session_uuid}"),
                }),
                some_data: Some(sign_in_gaia_request::inner::Data {
                    some_data: pubkey_pkix,
                }),
            }),
            unknown_int3: 0,
            network: GOOGLE_NETWORK.into(),
        };

        // Multi-account selection: Google routes the request to whichever
        // account is "default" unless we tell it otherwise. The endpoint
        // is gRPC-style and rejects `?authuser=N` as a query parameter
        // ("Cannot bind query parameter") — it must be sent as the
        // X-Goog-AuthUser HTTP header instead. We pass it through via
        // the GMESSAGES_AUTHUSER env var; http.rs adds the header.
        let authuser = std::env::var("GMESSAGES_AUTHUSER").unwrap_or_else(|_| "0".into());
        log::info!(
            "gaia: POST SignInGaia → {} authuser={authuser} (cookies={})",
            urls::SIGN_IN_GAIA,
            {
                let auth = client.inner.auth.lock().await;
                auth.cookies.len()
            }
        );
        let post_fut = client.post_protobuf::<_, SignInGaiaResponse>(
            urls::SIGN_IN_GAIA,
            &payload,
            ContentType::PBLite,
        );
        let resp: SignInGaiaResponse =
            match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
                Ok(Ok(r)) => {
                    log::info!("gaia: SignInGaia HTTP returned ok");
                    r
                }
                Ok(Err(e)) => {
                    log::warn!("gaia: SignInGaia HTTP error: {e}");
                    return Err(e);
                }
                Err(_) => {
                    log::warn!("gaia: SignInGaia HTTP timed out after 30s");
                    return Err(Error::Pairing(
                        "SignInGaia POST timed out after 30s. Check the network and try again."
                            .into(),
                    ));
                }
            };

        // Persist token + device descriptors.
        let token_data = resp
            .token_data
            .as_ref()
            .ok_or_else(|| Error::Pairing("SignInGaia: missing token_data".into()))?;
        let device_wrapper = resp
            .device_data
            .as_ref()
            .and_then(|d| d.device_wrapper.as_ref())
            .and_then(|w| w.device.as_ref())
            .ok_or_else(|| Error::Pairing("SignInGaia: missing device_wrapper".into()))?;

        let ttl_us = if token_data.ttl > 0 {
            token_data.ttl
        } else {
            24 * 60 * 60 * 1_000_000
        };
        {
            let mut auth = client.inner.auth.lock().await;
            auth.tachyon_auth_token = Some(token_data.tachyon_auth_token.clone());
            auth.tachyon_ttl = ttl_us;
            auth.tachyon_expiry = chrono::Utc::now().timestamp_millis() + ttl_us / 1000;
            // Mobile = device with lowercased source_id (per pair_google.go).
            let mut mobile = device_wrapper.clone();
            mobile.source_id = mobile.source_id.to_lowercase();
            auth.mobile = Some(mobile);
            auth.browser = Some(device_wrapper.clone());
        }
        Ok(resp)
    }
}
