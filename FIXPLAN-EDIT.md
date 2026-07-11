# Fix Plan — Message edit never updates (WhatsApp)

**Status: PLAN ONLY — execution by Opus. Standing rules identical to FIXPLAN-READSYNC.md
(tag `pre-edit-fixes` first; one packet = one commit; push `github main` only; build with
`cargo build --release -p whatsapp-desktop`; kill+cp install pre-authorized; changelog
entries with revert lines).**

User repro: send message → chevron on bubble → Edit → change text → save → nothing
updates, no error. Diagnosed by a 3-path investigation, all findings adversarially
verified against HEAD (all hold, all severity=major). THREE independent bugs — each alone
breaks editing:

## Root causes (verified, file:line)

### EB-1 — Outbound edit sends the optimistic `tmp-…` id as the protocol key
- do_send builds the optimistic bubble with `id = gen_tmp_id()` (`tmp-{nanos:x}-{seq:x}`,
  chat_view.rs:2387-2431, 4284-4298).
- ALL bubble menu paths (right-click 3274-3283, long-press 3290-3298, chevron 3317-3324)
  capture `msg.clone()` AT BUBBLE-CREATION TIME. `confirm_bubble` (3226-3243) re-keys only
  the bubbles map + search_texts to the real id — the closures' clones keep `tmp-…`
  forever. Edit stores `(chat_id, tmp_id)` into editing_msg (5186-5187); the 15-minute
  edit window (5167-5173) restricts Edit to exactly these in-session tmp-keyed bubbles.
- The wire builder itself is whatsmeow-parity CORRECT (src/client.rs:3283-3317,
  wacore/src/send.rs:456-467, 515-519 — `edit="1"` attr, fresh stanza id): it just gets
  fed `key.id = "tmp-…"`. Server ACKs (hence NO EditFailed in logs); phone/peer silently
  discards the edit for an unknown id.
- The Ok-path optimistic update EXISTS (runtime.rs:7521-7538) but misses BOTH targets:
  history is keyed by real_id (runtime.rs:4692-4739) and update_message_text's
  `bubbles.get("tmp-…")` misses the re-keyed map (window.rs:1312-1320 →
  chat_view.rs:3705-3731).
- **Same staleness silently breaks star / pin / react / delete / reply-quote for
  freshly-sent messages** (all use the captured `msg_c.id`).

### EB-2 — Inbound edit parser reads protocol_message at the wrong nesting level
- Wire shape (identical to what our own edit_message builds): `Message.edited_message`
  (FutureProofMessage, proto tag 58) → `.message.protocol_message{type=MESSAGE_EDIT,
  key.id=ORIGINAL id, edited_message=replacement}`. Only DeviceSentMessage is unwrapped
  pre-dispatch (src/message.rs:1280, wacore/src/messages.rs:86-98) — the wrapper reaches
  the desktop handler intact.
- Peer edits (stanza attr `edit="1"`, wacore/src/messages.rs:235-238) DO enter the edit
  branch (runtime.rs:2496) but:
  - `new_text` extraction (2513-2522): `msg.text_content()` peels the wrapper via
    get_base_message and lands on an inner Message whose only field is protocol_message
    → None; the `msg.protocol_message` fallback is top-level → None; → `""`.
    **Matches the log: every "Message edit received" has `new_text=""`.**
  - `target_id` (2499-2510): `info.meta_info.target_id` is never populated inbound
    (MessageInfo built with `..Default::default()`, wacore/src/messages.rs:224-240), and
    the `msg.protocol_message` fallback is top-level None → wrong/missing target.
- from_me edit ECHOES (no edit attr): `has_edited_msg` (2491-2495) checks top-level only
  → falls through to map_message → returns None → **DROPPED entirely** (log signature b).

### EB-3 — Apply path lacks an empty-text guard
- Given a REAL id and NON-empty text the apply path is sound (persist + is_edited +
  MessageEdited event + bubble update). But nothing guards `new_text == ""` — EB-2's
  parse failure would destructively persist "" once ids resolve. (Also: the DROPPED
  `A5…` ids in the log are phone-generated ids, NOT desktop echoes — desktop ids are
  `3EB0…`/`tmp-…`; no separate bug there.)

## Work packets

### Packet EB-A — tmp→real id remap (fixes EB-1) — chat_view.rs + runtime.rs
1. Add `id_remap: RefCell<HashMap<String, String>>` to ChatViewInner (~line 149 area);
   populate in `confirm_bubble` (3226-3243): `id_remap.insert(tmp_id → real_id)`.
   Clear it in `open_chat` alongside the bubbles map (it's per-chat-session state; a
   modest cap or clear-on-switch prevents unbounded growth).
2. Translate once at the top of `show_message_menu` (chat_view.rs:~4822) on a LOCAL
   cloned msg (`msg.id = remap.get(&msg.id).cloned().unwrap_or(msg.id)`) so Edit, star,
   pin, react, delete, reply-quote all get the real id. Note the `is_failed` lookup at
   ~4832 must use the same translated id.
3. **UNCONDITIONALLY translate again in do_send's edit branch (2376-2380) at send time**
   (verifier correction: this is NOT optional — it is the only point covering the
   confirm-lands-while-the-user-is-typing race), falling back to the stored id when no
   remap entry exists.
4. Defensive guard in the runtime EditMessage handler (runtime.rs:7509): if msg_id starts
   with `"tmp-"`, emit **EditFailed** (not just ErrorToast) so the existing
   restore_failed_edit path (window.rs:1321-1328) returns the text to the composer —
   editing a not-yet-confirmed message degrades gracefully instead of putting an
   unmatchable key on the wire.
   With the real id supplied, the existing wire format AND the existing optimistic
   update path both just work — no protocol change.

### Packet EB-B — inbound edit resolver (fixes EB-2 + EB-3) — runtime.rs
Insert before the edit branch (~2491):
```rust
let edit_pm = msg.protocol_message.as_deref().or_else(|| {
    msg.edited_message
        .as_ref()
        .and_then(|fp| fp.message.as_deref())
        .and_then(|m| m.protocol_message.as_deref())
});
```
(Type-checks: `as_deref()` on `Option<Box<T>>` unifies both arms to
`Option<&ProtocolMessage>`; NLL ends the borrow before `map_message(*msg, info)` moves
`msg` at ~2880 — verified.)
1. `has_edited_msg` (2491-2495) → `edit_pm.map_or(false, |pm| pm.edited_message.is_some())`
   — strict superset of current detection; routes attr-less from_me echoes into the edit
   branch instead of map_message-drop.
2. `target_id` (2499-2510): add fallback
   `.or_else(|| edit_pm.and_then(|pm| pm.key.as_ref()).and_then(|k| k.id.clone()))` —
   yields the ORIGINAL message id (keep the meta_info priority first; it's always None
   inbound, harmless).
3. `new_text` (2513-2522):
   `edit_pm.and_then(|pm| pm.edited_message.as_deref()).and_then(|em| em.text_content().map(String::from)).or_else(|| msg.text_content().map(String::from)).unwrap_or_default()`
   — `text_content()` (wacore/src/proto_helpers.rs:243-256) covers BOTH
   `conversation` and `extendedTextMessage.text`.
4. EB-3 guard: if the extracted `new_text` is empty, log a warning and skip the persist
   overwrite + MessageEdited emit (media-edit replacement bodies are out of scope; never
   blank a bubble).

### Sequencing & verification
- EB-B first (pure inbound, unblocks peer-edit rendering), EB-A second — or one combined
  pass since both are small; separate commits regardless. Different files mostly; both
  touch runtime.rs (EditMessage handler vs inbound handler — disjoint regions).
- Unit-testable: edit_pm resolver against a synthesized wrapped Message (both
  conversation and extendedTextMessage variants + empty-body media edit → guard).
- Live test script: (a) send a message on desktop, edit it → bubble updates in place with
  "(edited)" and THE PHONE SHOWS THE EDIT; (b) edit before the send confirms (fast) →
  either succeeds via remap or restores text to composer via EditFailed — never a silent
  no-op; (c) have the peer edit a message → desktop bubble updates (was: new_text="");
  (d) star/react/delete a just-sent message → acts on the right message (remap
  side-benefit); (e) restart after an edit → edited text persisted.
