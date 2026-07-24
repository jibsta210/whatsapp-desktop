//! Guard against upstream syncs silently reverting local protocol customizations.
//!
//! The v0.6.0 merge resolved src/receipt.rs, wacore/src/messages.rs and
//! src/message.rs to "theirs", deleting local fixes *and the tests that covered
//! them in the same commit* — so CI stayed green while phone-side read sync was
//! dead for days.
//!
//! This file lives in the DESKTOP crate, which upstream does not ship, so a
//! wholesale replacement of the protocol core cannot delete it. Each entry
//! asserts a load-bearing local behavior still exists. A failure here means an
//! upstream merge dropped it — re-apply, don't delete the assertion.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("desktop/ has a parent")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

struct Guard {
    file: &'static str,
    needle: &'static str,
    why: &'static str,
}

const GUARDS: &[Guard] = &[
    Guard {
        file: "src/client.rs",
        needle: "Jid::to_non_ad",
        why: "app-state key request must target the PRIMARY device (device 0). Using \
              device.pn verbatim carries this companion's device id, so the request is \
              encrypted to a session we never hold, dies with 'session not found', and \
              markChatAsRead can never reach the phone.",
    },
    Guard {
        file: "src/message.rs",
        needle: "sync_collections_batched",
        why: "handle_app_state_sync_key_share must re-sync collections when a key \
              arrives, otherwise patches deferred on a rotated key wait for a reconnect.",
    },
];

#[test]
fn local_protocol_customizations_survive_upstream_merges() {
    let mut lost = Vec::new();
    for g in GUARDS {
        if !read(g.file).contains(g.needle) {
            lost.push(format!("  {} :: missing `{}`\n      why: {}", g.file, g.needle, g.why));
        }
    }
    assert!(
        lost.is_empty(),
        "\n\nLocal protocol customization(s) reverted by an upstream sync:\n\n{}\n\n\
         Re-apply the behavior; do not delete these assertions.\n",
        lost.join("\n\n")
    );
}

/// The desktop stamps read watermarks from the receipt timestamp, so a receipt
/// carrying only a local wall clock over-suppresses genuinely-unread messages
/// after a reconnect backlog. Kept separate: this one is currently KNOWN-LOST to
/// the v0.6.0 merge and is tracked, not enforced, until re-applied.
#[test]
#[ignore = "known lost to v0.6.0 merge; re-enable when receipt t/recipient parsing is restored"]
fn receipt_parses_recipient_and_timestamp_attrs() {
    let src = read("src/receipt.rs");
    assert!(
        src.contains("optional_u64(\"t\")"),
        "handle_receipt must parse the stanza 't' attr as the read time"
    );
    assert!(
        src.contains("optional_jid(\"recipient\")"),
        "handle_receipt must parse 'recipient' so DM self-reads resolve to the real chat"
    );
}
