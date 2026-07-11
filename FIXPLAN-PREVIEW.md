# Fix Plan — Chat-list preview tracks "latest action" instead of "latest message"

**Status: PLAN ONLY — execution by Opus. Standing rules identical to FIXPLAN-READSYNC.md
(tag `pre-preview-fixes` first; one packet = one commit; push `github main` only; build
with `cargo build --release -p whatsapp-desktop`; kill+cp install pre-authorized;
changelog entries with revert lines).**

User reports (post edit-fix, which works): (1) editing a message doesn't update the
sidebar preview; (2) deleting an OLDER message from the phone hijacked the preview to
"deleted" AND the open chat's bubble never updated. User's frame — "preview = latest
action, should be latest message" — is correct in effect. All findings adversarially
verified at HEAD `7870896`; both major.

## Root causes (verified, file:line)

### PV-1 — MessageEdited updates only the bubble
- window.rs:1312-1320 — the ENTIRE handler is one `chat_view.update_message_text` call.
  No chat_list touch. (Contrast ReactionUpdated at 1283-1296 which calls
  `chat_list.update_preview_text` — the primitive exists, unused for edits.)
- BOTH runtime edit apply-sites — inbound (runtime.rs:2538-2555) and outbound Ok-path
  (7550-7567) — mutate `s.history` + `queue_save_messages` only; `ChatSummary.last_message`
  (wa_chats.bin via `save_tx`) is never updated → stale sidebar even after restart.
- The correct "is latest" signal is runtime-side: `s.history[chat_id]` is kept
  timestamp-sorted (sorted insert, runtime.rs:4300-4305) so `msgs.last()` is exact. The
  chat_list `recent_messages` cache is unsuitable (processing-order, cap 10, empty on
  restart). Do NOT reuse `touch_wa_chat_preview` (timestamp-gated + unread bump — wrong
  for edits).

### PV-2 — Revoke branch: wrong target id + unconditional preview stamp
- runtime.rs:2461-2485 — target_id comes from `info.meta_info.target_id`, which is NEVER
  populated anywhere (stanza parser ends `..Default::default()`, wacore/src/messages.rs:
  224-240; only other MsgMetaInfo constructions are `::default()`), so it always falls
  back to the revoke stanza's OWN fresh id. The original id lives in
  `ProtocolMessage{type=Revoke}.key.id` — never read. **Same class as the edit_pm nesting
  bug just fixed.** Consequences: `history.retain` removes nothing → deleted message
  resurrects on restart; `MessageDeletedLocal` carries the wrong id → `remove_message`
  (chat_view.rs:3692+) finds no bubble → the existing "🚫 This message was deleted"
  placeholder never renders.
- window.rs:1310 → `set_preview_to_previous` (chat_list.rs:1136-1141) — despite the name,
  it UNCONDITIONALLY sets the row to "🚫 Message deleted"; no was-latest check, no
  recompute. That's the preview hijack (not a timestamp race — the revoke branch returns
  before update_last_message ever runs).

### PV-3 — ReactionUpdated has the same unconditional hijack (verifier bonus finding)
- window.rs:1291-1295 sets `"Reacted {emoji}"` as the preview for reactions on ANY
  message, old or latest.

## Work packets

### Packet PV-A — edit → preview (fixes PV-1)
1. **bridge.rs:79-83**: add `is_latest: bool` to `WaEvent::MessageEdited`. (window.rs:654
   debug arm destructures `{ msg_id, .. }` — safe.)
2. **runtime.rs inbound branch (2538-2555)**, inside the existing lock, after applying
   the text: `let is_latest = s.history.get(&chat_id).and_then(|m| m.last()).map(|l| l.id == target_id).unwrap_or(false);`
   If is_latest: update `c.last_message = new_text.clone()` on the matching `s.chats`
   entry WITHOUT touching timestamp/unread (no reorder), then `save_tx.send(s.chats.clone())`
   (only when found). Include `is_latest` in the emit.
   **Disk fallback (verifier caveat):** if `s.history` lacks the chat (LRU-evicted /
   never opened), mirror the poll-vote pattern (runtime.rs:2822-2829: `load_messages` +
   insert) BEFORE the apply — this also fixes the pre-existing bug that an inbound edit
   to an evicted chat isn't persisted at all (queue_save_messages currently sits inside
   the if-let).
3. **runtime.rs outbound Ok-path (7550-7567)**: identical (target is `msg_id`).
4. **window.rs:1312-1320**: destructure `is_latest`; after the chat_view call:
   `if is_latest { inner.chat_list.update_preview_text(&chat_id, &new_text); }`
   (update_preview_text changes label text only — no sort bump; correct edit semantics).
5. Optional polish: prefix `"You: "` when the edited message is from_me (read
   `m.is_from_me` from the history entry under the same lock; outbound path is trivially
   from_me) to match the live-row convention (chat_list.rs:723-724).

### Packet PV-B — revoke target + gated preview (fixes PV-2)
1. **runtime.rs:2461-2468 — pm resolver parity with edit_pm:**
   ```rust
   let base = msg.get_base_message();
   let revoke_pm = msg.protocol_message.as_deref().or(base.protocol_message.as_deref());
   let target_id = info.meta_info.target_id.as_ref().map(|id| id.to_string())
       .or_else(|| revoke_pm.and_then(|pm| pm.key.as_ref()).and_then(|k| k.id.clone()))
       .unwrap_or_else(|| msg_id.clone());
   ```
   (base fallback is redundant today — DSM unwrapped upstream — but harmless parity.)
   This alone fixes the bubble placeholder AND the resurrect-on-restart.
2. **Disk fallback (verifier GAP 1):** if `s.history` lacks the chat, `load_messages` +
   insert (poll-vote pattern) so the retain + was_latest work for never-opened chats.
3. **was_latest:** inside the lock, BEFORE retain:
   `let was_latest = msgs.iter().max_by_key(|m| m.timestamp).map(|m| m.id == target_id).unwrap_or(false);`
   After retain, if was_latest: set persisted `c.last_message = "🚫 Message deleted"` +
   `save_tx` (restart consistency).
4. **bridge.rs:165-168**: add `was_latest: bool` to `MessageDeletedLocal`; set at ALL
   THREE emit sites (runtime.rs:2479, 4956, 5917 — compute the same way before each
   retain). window.rs:653 `{ msg_id, .. }` is compatible.
5. **window.rs:1307-1311**: keep `chat_view.remove_message` unconditional; replace the
   `set_preview_to_previous` call with
   `if was_latest { inner.chat_list.update_preview_text(&chat_id, "🚫 Message deleted"); }`.
   DELETE `set_preview_to_previous` (chat_list.rs:1136-1141 — single caller).
6. **DeleteForMe semantics (verifier GAP 2):** at the runtime.rs:5917 emit site
   (delete-for-me), showing "deleted" is wrong UX — nothing was revoked for the other
   party. There, when was_latest, recompute the persisted preview from the new latest
   REMAINING message (`media_preview(last)`) instead of the deleted stamp, and have the
   UI path use that (either a separate event field carrying the replacement preview, or
   emit was_latest=false and update the preview runtime-side via the persisted summary +
   a ChatUpdated-style refresh — implementer's choice, keep it minimal).

### Packet PV-C — reaction preview gating (fixes PV-3)
- window.rs:1283-1296 (ReactionUpdated): only call `update_preview_text("Reacted {emoji}")`
  when the reacted-to message is the chat's latest. The event flows from runtime emit
  sites that already look up the target in history — carry an `is_latest: bool` on
  `ReactionUpdated` (bridge.rs) computed the same `msgs.last().id == target` way at both
  emit sites (runtime.rs ~2523+ and the SendReaction optimistic site), gm site too
  (gmessages_runtime.rs ReactionUpdated emit — compute from its persisted messages or
  pass false conservatively). Reaction removal (empty vec) should not stamp a preview at
  all.

### Sequencing & verification
- PV-B first (it's the data-loss/restart-consistency one), then PV-A, then PV-C.
  Separate commits. All three touch bridge.rs (additive fields) + window.rs (disjoint
  arms) + runtime.rs (disjoint branches) — sequential execution, no parallel agents
  needed at this size.
- Unit-testable: was_latest/is_latest computation given a sorted history vec; revoke
  target extraction from a synthesized Revoke protocolMessage.
- Live tests: (a) edit the LATEST message → sidebar preview updates to new text (no
  reorder); edit an OLDER message → preview unchanged; both survive restart. (b) delete
  an OLD message from phone → bubble shows "🚫 This message was deleted", preview
  unchanged, stays deleted after restart; delete the LATEST message → preview shows
  "🚫 Message deleted". (c) react to an old message → preview unchanged; react to the
  latest → "Reacted 👍". (d) delete-for-me the latest message → preview falls back to
  the new latest remaining message.
