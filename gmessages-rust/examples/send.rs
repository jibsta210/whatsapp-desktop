//! Send example: connects with stored AuthData and sends a single text.
//!
//! ```bash
//! AUTH_PATH=./gmessages-auth.json \
//!   cargo run -p gmessages-rust --example send -- "+15551234567" "hello"
//! ```

use gmessages_rust::{AuthData, Client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let mut args = std::env::args().skip(1);
    let to = args.next().context("missing recipient")?;
    let body = args.next().context("missing body")?;

    let auth_path = std::env::var("AUTH_PATH").unwrap_or_else(|_| "gmessages-auth.json".into());
    let auth: AuthData = serde_json::from_slice(&std::fs::read(&auth_path)?)?;

    let client = std::sync::Arc::new(Client::new(auth));
    client.connect().await?;
    let id = client.send_text(&to, &body).await?;
    println!("sent: {id}");
    Ok(())
}

use anyhow::Context;
