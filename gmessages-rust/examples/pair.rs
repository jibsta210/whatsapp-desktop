//! Pairing example.
//!
//! ```bash
//! cargo run -p gmessages-rust --example pair
//! ```
//!
//! Renders a QR code in the terminal — scan with Google Messages on your
//! Android phone (Settings → Device pairing → QR code scanner).
//!
//! On success, writes `gmessages-auth.json` to the current directory, which
//! `--example listen` and `--example send` will pick up.

use std::sync::Arc;

use gmessages_rust::{AuthData, Client, Event};
use qrcode::QrCode;
use qrcode::render::unicode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let client = Arc::new(Client::new(AuthData::default()));
    let mut events = client
        .take_event_receiver()
        .await
        .expect("event receiver should be available");

    // Drive the pairing handshake on a background task so we can pump events
    // here on the main task.
    let pair_task = {
        let c = client.clone();
        tokio::spawn(async move { c.start_pairing().await })
    };

    while let Some(event) = events.recv().await {
        match event {
            Event::QrCode { url } => {
                println!("\nScan this with Google Messages on your phone:\n");
                let code = QrCode::new(url.as_bytes())?;
                let rendered = code
                    .render::<unicode::Dense1x2>()
                    .dark_color(unicode::Dense1x2::Light)
                    .light_color(unicode::Dense1x2::Dark)
                    .quiet_zone(true)
                    .build();
                println!("{rendered}");
                println!("(also: {url})\n");
            }
            Event::Ready => {
                println!("[long-poll connected, waiting for phone to scan QR…]");
            }
            Event::PairingEmoji { emoji } => {
                println!("[verify emoji] {emoji}");
            }
            Event::PairSuccess => {
                println!("[paired]");
                let auth = client.auth_snapshot().await;
                let json = serde_json::to_vec_pretty(&auth)?;
                std::fs::write("gmessages-auth.json", &json)?;
                println!("wrote gmessages-auth.json ({} bytes)", json.len());
                break;
            }
            Event::PairFailed { reason } => {
                eprintln!("[pair failed] {reason}");
                break;
            }
            Event::AuthRevoked => {
                eprintln!("[auth revoked during pair]");
                break;
            }
            other => {
                log::debug!("event during pair: {other:?}");
            }
        }
    }

    pair_task.await??;
    Ok(())
}
