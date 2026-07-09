//! Long-poll receive loop.
//!
//! Mirrors `pkg/libgm/longpoll.go`. Behaviour:
//!
//! 1. POST `ReceiveMessages` with the long client. The server returns a
//!    streaming JSON response that opens with `[[`, followed by
//!    comma-separated PBLite-encoded `LongPollingPayload`s, and ends with
//!    `]]`.
//! 2. As each complete JSON value lands, dispatch it (data event → event
//!    handler, ack/heartbeat/start_read → noop trace).
//! 3. When the stream closes (timeout/EOF), reconnect with backoff.
//! 4. On 401/403, give up and emit `Event::AuthRevoked`.
//!
//! Also runs the **ditto pinger** as a sibling background task: pings the
//! phone every 60s; emits `PhoneNotResponding` after 3 consecutive misses,
//! `PhoneRespondingAgain` when it comes back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::time::sleep;
use uuid::Uuid;

use crate::gmproto::authentication::AuthMessage;
use crate::gmproto::client::{ReceiveMessagesRequest, receive_messages_request};
use crate::gmproto::rpc::LongPollingPayload;
use crate::http::ContentType;
use crate::{Client, Error, Event, Result, urls};

pub const PING_INTERVAL: Duration = Duration::from_secs(60);
pub const PING_TIMEOUT: Duration = Duration::from_secs(60);
pub const ALERT_AFTER_FAILS: u32 = 3;

/// Spawn the long-poll task + the ditto pinger task. Returns once both are
/// running; cancels via `client.disconnect()`.
pub async fn spawn(client: Client) -> Result<()> {
    let connected = Arc::new(AtomicBool::new(false));

    // Long-poll task.
    {
        let client = client.clone();
        let connected = connected.clone();
        let mut shutdown = client.inner.shutdown.subscribe();
        tokio::spawn(async move {
            let result = tokio::select! {
                r = run_long_poll(client.clone(), connected.clone()) => r,
                _ = shutdown.recv() => Ok(()),
            };
            if let Err(e) = result {
                log::warn!("long-poll exited with error: {e}");
                client.emit(Event::AuthRevoked);
            }
        });
    }

    // Ditto pinger task.
    {
        let client = client.clone();
        let connected = connected.clone();
        let mut shutdown = client.inner.shutdown.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = run_pinger(client, connected) => {},
                _ = shutdown.recv() => {},
            }
        });
    }

    // Ack flush task.
    {
        let client = client.clone();
        let mut shutdown = client.inner.shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sleep(Duration::from_secs(5)) => {
                        if let Err(e) = crate::session::flush_acks(&client).await {
                            log::trace!("ack flush failed: {e}");
                        }
                    }
                    _ = shutdown.recv() => break,
                }
            }
        });
    }

    // Proactive token-refresh task. Runs every 15 minutes regardless of
    // long-poll state. Also detects laptop suspend/resume by comparing
    // wall-clock between ticks: if the gap is much longer than the sleep
    // duration (system was suspended), force-refresh the tachyon token
    // immediately so we don't get revoked while sleeping.
    {
        let client = client.clone();
        let mut shutdown = client.inner.shutdown.subscribe();
        tokio::spawn(async move {
            const TICK: Duration = Duration::from_secs(15 * 60);
            const SUSPEND_THRESHOLD: Duration = Duration::from_secs(60); // wake jump > tick + this = suspended
            let mut last_wall = std::time::SystemTime::now();
            loop {
                tokio::select! {
                    _ = sleep(TICK) => {
                        let now = std::time::SystemTime::now();
                        let elapsed = now.duration_since(last_wall).unwrap_or(TICK);
                        last_wall = now;
                        let suspended = elapsed > TICK + SUSPEND_THRESHOLD;
                        if suspended {
                            log::warn!(
                                "gmessages: detected wall-clock jump of {}s (likely suspend resume); force-refreshing auth",
                                elapsed.as_secs()
                            );
                        } else {
                            log::trace!("gmessages: periodic auth refresh tick");
                        }
                        if let Err(e) = crate::session::refresh_auth_token(&client).await {
                            log::warn!("background auth refresh failed: {e}");
                            // If suspended-and-failed, the long-poll's
                            // current connection is almost certainly dead.
                            // Trigger an explicit reconnect by emitting
                            // shutdown, which the run_long_poll loop
                            // catches → re-enters its outer loop → opens
                            // a fresh ReceiveMessages stream.
                            if suspended {
                                let _ = client.inner.shutdown.send(());
                            }
                        }
                    }
                    _ = shutdown.recv() => break,
                }
            }
        });
    }

    Ok(())
}

async fn run_long_poll(client: Client, connected: Arc<AtomicBool>) -> Result<()> {
    let mut error_count = 0u32;
    let mut shutdown = client.inner.shutdown.subscribe();

    loop {
        if shutdown.try_recv().is_ok() {
            return Ok(());
        }

        // Refresh the tachyon token if it's close to expiring. Mirrors Go's
        // `refreshAuthToken(nil)` call at the top of each long-poll iteration.
        if let Err(e) = crate::session::refresh_auth_token(&client).await {
            log::warn!("refresh_auth_token failed: {e}; continuing with old token");
        }

        let request_id = Uuid::new_v4().to_string();
        let auth = client.inner.auth.lock().await;
        let payload = ReceiveMessagesRequest {
            auth: Some(AuthMessage {
                request_id,
                tachyon_auth_token: auth.tachyon_auth_token.clone().unwrap_or_default(),
                network: auth.auth_network().into(),
                config_version: Some(crate::session::config_version()),
            }),
            unknown: Some(receive_messages_request::UnknownEmptyObject2 {
                unknown: Some(receive_messages_request::UnknownEmptyObject1 {}),
            }),
        };
        let cookies = auth.cookies.clone();
        let authuser = auth.gaia_authuser;
        let url = if auth.has_cookies() {
            urls::RECEIVE_MESSAGES_GOOGLE
        } else {
            urls::RECEIVE_MESSAGES
        };
        drop(auth);

        // Use the long client. We need the raw response stream, so go below
        // RelayHttp::post_long here and call reqwest directly.
        let body = crate::pblite::marshal(&payload)?;
        let mut headers = crate::headers::relay(ContentType::PBLite.as_str(), "*/*");
        // For Gaia (cookie) auth, apply Cookie + SAPISIDHASH +
        // X-Goog-AuthUser via the shared helper. Without the latter two
        // the clients6.google.com receive endpoint replies 401 and the
        // pair flow bails with AuthRevoked partway through.
        crate::headers::apply_cookie_auth(&mut headers, url, &cookies, authuser);
        let req = client
            .inner
            .http
            .long
            .post(url)
            .headers(headers)
            .body(body);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                error_count += 1;
                // Cap the linear backoff so a sustained outage doesn't push SMS
                // recovery out to many minutes.
                let secs = ((error_count + 1) * 5).min(60);
                log::warn!("ReceiveMessages POST failed (#{error_count}): {e}; retrying in {secs}s");
                tokio::select! {
                    _ = sleep(Duration::from_secs(secs as u64)) => {}
                    _ = shutdown.recv() => return Ok(()),
                }
                continue;
            }
        };

        let status = resp.status();
        // Capture any Set-Cookie rotation Google sent on the long-poll
        // response — this keeps our cookie cache fresh while the
        // long-poll is the only HTTP traffic on the socket.
        {
            let set_cookies: Vec<&str> = resp
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();
            if !set_cookies.is_empty() {
                let parsed =
                    crate::cookies::parse_set_cookie_headers(set_cookies.iter().copied());
                crate::cookies::merge_into_cache(parsed);
            }
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            client.emit(Event::AuthRevoked);
            return Err(Error::AuthRevoked);
        }
        if !status.is_success() {
            let body = resp.bytes().await.unwrap_or_default();
            error_count += 1;
            let secs = (error_count + 1) * 5;
            // Log up to first 256 bytes of body for diagnostics.
            let body_preview = String::from_utf8_lossy(&body[..body.len().min(256)]);
            log::warn!(
                "ReceiveMessages got HTTP {status} (#{error_count}); body[..{}]={body_preview:?}; retrying in {secs}s",
                body.len().min(256)
            );
            tokio::select! {
                _ = sleep(Duration::from_secs(secs as u64)) => {}
                _ = shutdown.recv() => return Ok(()),
            }
            continue;
        }
        if error_count > 0 {
            log::info!("ReceiveMessages reconnected (cleared {error_count} errors)");
        }
        error_count = 0;
        connected.store(true, Ordering::Relaxed);
        log::info!(
            "ReceiveMessages opened: status={status} content-type={:?}",
            resp.headers().get("content-type")
        );
        client.emit(Event::Ready);

        // Stream the body.
        match read_stream(&client, resp, &mut shutdown).await {
            Ok(()) => log::info!("long-poll stream closed cleanly; reconnecting"),
            Err(e) => log::warn!("long-poll stream ended with error: {e}"),
        }
        connected.store(false, Ordering::Relaxed);
    }
}

/// Read the streaming JSON body. Wire format:
///
/// ```text
/// [[<value>,<value>,...,<value>]]
/// ```
///
/// where each `<value>` is itself a JSON array (PBLite-encoded
/// `LongPollingPayload`). Strategy mirrors `pkg/libgm/longpoll.go`:
///
/// 1. Read first 2 bytes, expect `[[`.
/// 2. Accumulate bytes into a buffer; after each TCP chunk, try to parse the
///    buffer as JSON. If it parses → one value done; reset buffer.
/// 3. New values arrive comma-prefixed; trim the leading comma.
/// 4. The 2-byte chunk `]]` (or `]` once trailing) is the stream end marker.
///
/// We log every byte received at trace level so failures are diagnosable.
async fn read_stream(
    client: &Client,
    resp: reqwest::Response,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    let mut stream = resp.bytes_stream();

    // Step 1: pull bytes until we have at least the opening `[[`.
    let mut prelude: Vec<u8> = Vec::with_capacity(8);
    while prelude.len() < 2 {
        let chunk = next_chunk(&mut stream, shutdown).await?;
        let chunk = match chunk {
            Some(c) => c,
            None => {
                log::warn!("long-poll: stream ended before opening `[[`");
                return Ok(());
            }
        };
        log::trace!("long-poll: prelude chunk {} bytes", chunk.len());
        prelude.extend_from_slice(&chunk);
    }
    if &prelude[..2] != b"[[" {
        let preview = String::from_utf8_lossy(&prelude[..prelude.len().min(64)]);
        log::error!("long-poll: opening is not `[[`: got {preview:?}");
        return Err(Error::Protocol("long-poll opening is not [[".into()));
    }
    log::debug!("long-poll: opening `[[` confirmed");
    let mut accumulated: Vec<u8> = prelude[2..].to_vec();

    // Step 2: stream values.
    loop {
        // Drain as many complete values as the accumulated buffer holds.
        // A "value" is `[...]` (top-level JSON array). Values are separated
        // by `,`; the stream ends with `]`.
        loop {
            // Skip a single leading comma if present.
            let scan_start = if accumulated.first() == Some(&b',') { 1 } else { 0 };
            if scan_start >= accumulated.len() {
                break; // need more bytes
            }
            // Stream-end marker.
            if accumulated[scan_start] == b']' {
                log::debug!("long-poll: got stream end marker, exiting cleanly");
                return Ok(());
            }
            match find_first_value_end(&accumulated[scan_start..]) {
                Some(rel_end) => {
                    let abs_end = scan_start + rel_end;
                    let body = &accumulated[scan_start..abs_end];
                    log::trace!("long-poll: parsed value of {} bytes", body.len());
                    if let Err(e) = handle_payload(client, body).await {
                        log::warn!("long-poll: failed to handle payload: {e}");
                    }
                    accumulated.drain(..abs_end);
                    // loop and try to drain another value from the same buffer
                }
                None => break, // value not yet complete
            }
        }

        let chunk = next_chunk(&mut stream, shutdown).await?;
        match chunk {
            Some(c) => {
                log::trace!("long-poll: chunk +{} bytes (accum now {})", c.len(), accumulated.len() + c.len());
                accumulated.extend_from_slice(&c);
            }
            None => {
                log::info!(
                    "long-poll: stream ended; {} bytes leftover",
                    accumulated.len()
                );
                return Ok(());
            }
        }
    }
}

/// Scan a buffer that begins with a JSON value and return the byte index
/// **just past** the end of that value. Returns `None` if the value isn't
/// complete yet.
///
/// Tracks bracket depth ([] and {}), string boundaries, and `\` escapes.
/// Whitespace is allowed inside but values must start with `[`, `{`, `"`, or
/// a digit/minus (PBLite always emits arrays so the latter cases are mostly
/// for safety).
fn find_first_value_end(buf: &[u8]) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut started = false;

    for (i, &b) in buf.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                started = true;
            }
            b'[' | b'{' => {
                depth += 1;
                started = true;
            }
            b']' | b'}' => {
                if depth == 0 {
                    // Stream-end `]`, not part of a value.
                    return None;
                }
                depth -= 1;
                if depth == 0 && started {
                    return Some(i + 1);
                }
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => {
                started = true;
            }
        }
    }
    None
}

async fn next_chunk(
    stream: &mut (impl futures::Stream<Item = std::result::Result<Bytes, reqwest::Error>>
              + Unpin),
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<Option<Bytes>> {
    tokio::select! {
        next = stream.next() => match next {
            Some(Ok(b)) => Ok(Some(b)),
            Some(Err(e)) => Err(Error::Http(e)),
            None => Ok(None),
        },
        _ = shutdown.recv() => Ok(None),
    }
}

async fn handle_payload(client: &Client, json_bytes: &[u8]) -> Result<()> {
    let payload: LongPollingPayload = match crate::pblite::unmarshal(json_bytes) {
        Ok(p) => p,
        Err(e) => {
            let preview = String::from_utf8_lossy(&json_bytes[..json_bytes.len().min(512)]);
            log::error!(
                "failed to unmarshal LongPollingPayload ({} bytes): {e}; preview={preview:?}",
                json_bytes.len()
            );
            return Err(e);
        }
    };
    log::trace!(
        "long-poll payload: data={} ack={} hb={} sr={}",
        payload.data.is_some(),
        payload.ack.is_some(),
        payload.heartbeat.is_some(),
        payload.start_read.is_some()
    );
    if let Some(data) = payload.data {
        // Queue an ack for this incoming message.
        {
            let mut session = client.inner.session.lock().await;
            session.ack_queue.push(data.response_id.clone());
        }
        crate::event_handler::handle_incoming_rpc(client, data).await?;
    } else if let Some(ack) = payload.ack {
        let count = ack.count.unwrap_or(0);
        let mut session = client.inner.session.lock().await;
        session.skip_count = count;
        log::debug!("got startup ack: skip_count={count}");
    } else if payload.heartbeat.is_some() {
        log::trace!("heartbeat");
    } else if payload.start_read.is_some() {
        log::trace!("start_read");
    }
    Ok(())
}

async fn run_pinger(client: Client, connected: Arc<AtomicBool>) {
    let mut fails: u32 = 0;
    let mut not_responding_emitted = false;
    let mut interval = tokio::time::interval(PING_INTERVAL);
    interval.tick().await; // first tick fires immediately

    loop {
        interval.tick().await;
        if !connected.load(Ordering::Relaxed) {
            continue;
        }
        match tokio::time::timeout(PING_TIMEOUT, crate::session::notify_ditto_activity(&client))
            .await
        {
            Ok(Ok(())) => {
                if not_responding_emitted {
                    client.emit(Event::PhoneRespondingAgain);
                    not_responding_emitted = false;
                }
                fails = 0;
            }
            Ok(Err(e)) => {
                log::warn!("ditto ping failed: {e}");
                fails += 1;
                if fails >= ALERT_AFTER_FAILS && !not_responding_emitted {
                    client.emit(Event::PhoneNotResponding);
                    not_responding_emitted = true;
                }
            }
            Err(_) => {
                log::warn!("ditto ping timed out (>{}s)", PING_TIMEOUT.as_secs());
                fails += 1;
                if fails >= ALERT_AFTER_FAILS && !not_responding_emitted {
                    client.emit(Event::PhoneNotResponding);
                    not_responding_emitted = true;
                }
            }
        }
    }
}
