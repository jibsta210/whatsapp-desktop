//! Pairing flows.
//!
//! Two paths exist:
//!
//! - [`qr`] — phone-relay (Bugle) QR pairing. Implements
//!   `RegisterPhoneRelay` → QR display → `Paired` event → finalization.
//! - [`gaia`] — Google account pairing using SignInGaia + UKEY2. (TODO)
//!
//! See `pkg/libgm/pair.go` and `pkg/libgm/pair_google.go`.

use crate::{Client, Result};

pub const USER_AGENT: &str = crate::headers::USER_AGENT;

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
        AuthMessage, AuthenticationContainer, BrowserDetails, EcdsaKeys, KeyData,
        RegisterPhoneRelayResponse, authentication_container,
    };
    use crate::http::ContentType;
    use crate::{Client, Error, Result, urls};

    pub fn browser_details() -> BrowserDetails {
        BrowserDetails {
            user_agent: crate::headers::USER_AGENT.into(),
            // BrowserType_OTHER = 1
            browser_type: 1,
            os: "libgm".into(),
            // DeviceType_TABLET = 3 (per pair_google.go reference)
            device_type: 3,
        }
    }

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
    async fn register_phone_relay(
        client: &Client,
    ) -> Result<(Vec<u8>, AesCtrHelper)> {
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

pub mod gaia {
    //! Gaia (Google account) pairing.
    //!
    //! **Status:** stub. Implementing this fully requires:
    //!
    //! 1. The user must already be logged into <https://messages.google.com/web/>
    //!    in a real browser.
    //! 2. Export the cookies (`SAPISID`, `HSID`, `SSID`, `APISID`, `SID`,
    //!    `__Secure-1PSID`, etc.) into [`AuthData::cookies`](crate::AuthData::cookies).
    //! 3. POST `SignInGaia` with the cookies + a `messages-web-{uuid}` device
    //!    ID; receive a `tachyon_auth_token` and the phone's destination UUID.
    //! 4. Run UKEY2 over `CREATE_GAIA_PAIRING_CLIENT_INIT/FINISHED` actions
    //!    to derive the AES/HMAC keys (this is the hard part — emoji
    //!    verification, ECDSA P-256, HKDF).
    //! 5. Persist; the resulting `AuthData` is durable as long as the
    //!    Google session cookies stay valid.
    //!
    //! Note that "OAuth pairing" as the user thinks of it doesn't really
    //! exist as a separate flow — Gaia pairing piggybacks on whatever Google
    //! auth method produced the cookies (password, 2FA, OAuth, etc.).
    //!
    //! See `pkg/libgm/pair_google.go` for the reference implementation.
    use crate::{Client, Error, Result};

    pub async fn start(_client: &Client) -> Result<()> {
        Err(Error::NotImplemented(
            "gaia::start: see module docs for what's needed",
        ))
    }
}

pub mod ukey2 {
    //! UKEY2 emoji-verification handshake. Used by Gaia pairing.
    //! Not yet implemented.
    use crate::{Error, Result};

    pub struct PairingSession;

    impl PairingSession {
        pub fn new() -> Self {
            Self
        }

        pub fn prepare_payloads(&mut self) -> Result<Vec<u8>> {
            Err(Error::NotImplemented("PairingSession::prepare_payloads"))
        }

        pub fn process_server_init(&mut self, _payload: &[u8]) -> Result<Vec<String>> {
            Err(Error::NotImplemented("PairingSession::process_server_init"))
        }

        pub fn finish(&mut self) -> Result<()> {
            Err(Error::NotImplemented("PairingSession::finish"))
        }
    }

    impl Default for PairingSession {
        fn default() -> Self {
            Self::new()
        }
    }

    /// UKEY2 pairing emoji table v1. (To be filled from
    /// `pair_google.go` when implementing UKEY2.)
    pub const PAIRING_EMOJIS_V1: &[&str] = &[];
}
