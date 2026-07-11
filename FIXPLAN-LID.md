# Fix Plan — LID phantom chats ("+88077045383286" instead of the saved contact)

**Status: PLAN ONLY — execution by Opus. Standing rules identical to FIXPLAN-READSYNC.md
(tag `pre-lid-fixes` first; one packet = one commit; push `github main` only; build with
`cargo build --release -p whatsapp-desktop`; kill+cp install pre-authorized; changelog
entries with revert lines).**

User repro: continued an existing conversation from the phone with a saved Canadian
contact → desktop created a DUPLICATE chat titled "+88077045383286" (the peer's **LID**
digits formatted as a phone number) holding the phone-sent echoes, instead of routing
into the existing phone-JID chat. Log-confirmed: the purpose-built phantom recovery
("Queued LID resolution…" → "LID resolver: resolving 1 JIDs via usync") fired and then
**silently died**. All three findings adversarially verified at HEAD (hold, major).

## Root causes (verified, file:line)

- **LD-1 — the peer's phone number is ON THE WIRE and dropped.** An own-echo LID DM
  stanza carries `peer_recipient_pn=<peer>@s.whatsapp.net` — proven by the repo's own
  test fixture from a real capture (src/message.rs:2891-2896). The own-echo parse branch
  (wacore/src/messages.rs:177-189) reads only `recipient` and leaves
  `MessageSource.recipient_alt` (the whatsmeow-parity slot, types/message.rs:39) `None`
  at every construction site. Desktop seeding reads ONLY `sender_alt`
  (runtime.rs:2351-2360 — which for an own echo is your own number, correctly None-ish),
  so `lid_to_phone` never learns the peer mapping; chat-key resolution
  (runtime.rs:2368-2439) misses every branch → phantom LID chat.
- **LD-2 — the fallback resolver is a guaranteed silent no-op.** resolve_lid_batch
  (runtime.rs:2016-2077) does a usync device query then `resolve_lid_to_phone_jid`,
  which is **cache-only** (src/client/lid_pn.rs:97-104), and usync mapping extraction
  requires a PN-keyed user node (wacore/src/iq/usync.rs:565-578) — a LID-keyed query can
  NEVER populate the cache. `resolved==0`, and both the success log and the ChatsLoaded
  refresh sit behind `if resolved > 0` — hence the observed log silence.
- **LD-3 — an existing phantom is never retroactively merged.** The only merge
  (`merge_lid_chats`, runtime.rs:8745-8846, startup-only via :2250) resolves through the
  same core cache (dead per LD-2), and the live resolver on success only inserts the
  mapping + re-sends an upsert-only ChatsLoaded — **nothing ever deletes the phantom
  row or merges its message file**. (`remove_lid_duplicate` matches by display name —
  useless for "+digits" phantoms.)

## Work packets

### Packet LD-A — parse `peer_recipient_pn` + learn the mapping at the CORE level
**Files:** wacore/src/messages.rs, src/message.rs (+ its tests)
1. wacore/src/messages.rs:177-189 (own-echo DM branch): after
   `let recipient = attrs.optional_jid("recipient");` add
   `let recipient_alt = attrs.optional_jid("peer_recipient_pn").or_else(|| attrs.optional_jid("recipient_pn"));`
   and include `recipient_alt` in the MessageSource literal. (whatsmeow parity:
   parseMessageSource → MessageInfo.RecipientAlt.)
2. src/message.rs (~304-360, mirror the existing sender-side learning): when
   `info.source.is_from_me`, chat is `@lid`, and `recipient_alt` is a PN
   (`DEFAULT_USER_SERVER`), call `add_lid_pn_mapping(&chat.user, &recipient_alt.user,
   LearningSource::PeerLidMessage)` (variant exists, used at message.rs:311; persists to
   the SQLite-backed store). **This is what heals the user's EXISTING phantom** — at next
   startup `merge_lid_chats` resolves via this core cache and folds it in.
3. Tests: extend the two existing fixtures — src/message.rs:2903-2929 assert
   `recipient_alt == Some("559985213786@s.whatsapp.net".parse().unwrap())`; the
   note-to-self case (:3080-3084) learns own-lid→own-pn (truthful, harmless).

### Packet LD-B — desktop seeding + direct routing + resolver observability
**Files:** desktop/src/ui/runtime.rs
1. Seeding block (~2351-2360): after the sender_alt seeding, for
   `info.source.is_from_me && raw_chat_id.ends_with("@lid")`, if
   `recipient_alt` is a `@s.whatsapp.net` JID → `insert_lid_phone(raw_chat_id, pn)`
   (runtime.rs:1296-1318 — already normalizes device suffixes + maintains phone_to_lid).
   The very next lookup (:2377 `cached`) then hits, the
   `Some(phone) if s.chats.iter().any(...)` guard (:2416) matches the existing chat, and
   the echo routes into it with the contact's name. No phantom, no usync.
2. alt_phone (:2369-2374): extend to
   `info.source.sender_alt.as_ref().or(info.source.recipient_alt.as_ref())` for from_me
   (belt-and-braces if seed/lookup ever race). Safe: recipient_alt is None on all
   inbound-DM and group paths.
3. resolve_lid_batch (:2042-2058): log the outcome UNCONDITIONALLY
   (`resolved {n}/{total}`), plus `warn!` when 0 ("usync cannot map lid→pn"), keeping
   persist/ChatsLoaded gated on `resolved > 0`.

### Packet LD-C — retroactive phantom merge on mapping discovery
**Files:** desktop/src/ui/runtime.rs (+ bridge/window already have ChatDeleted/ChatAdded)
1. Extract the per-chat body of `merge_lid_chats` (runtime.rs:8771-8846 — file merge with
   dedup-by-id + timestamp sort + save_messages_scoped, state swap via upsert_chat,
   lid-file deletion) into
   `async fn merge_one_lid_chat(state, tx, lid_id, phone_jid)`.
   At the end: emit `WaEvent::ChatDeleted { chat_id: lid_id }` (bridge.rs:140 →
   window.rs:1272 → chat_list.remove_chat — ChatsLoaded alone never removes rows) **and**
   refresh the merged phone row (emit ChatAdded(merged_summary) or one ChatsLoaded after
   the batch) so its preview/timestamp update.
2. `merge_lid_chats` startup resolve (:8767): when the core cache misses, fall back to
   the UI `lid_to_phone` map — **GATED on the mapped phone chat already existing in
   s.chats** (verifier safety amendment: an ungated fallback would merge into a wrong JID
   from a stale persisted entry and then delete the lid file; the exists-guard is exactly
   the phantom scenario). Core-cache-resolved mappings stay ungated as today.
3. resolve_lid_batch success loop (:2049-2056): after `insert_lid_phone`, collect
   `(lid, phone)` pairs where a chat row exists under the lid; after releasing the lock,
   `merge_one_lid_chat` each. **Ordering matters:** mapping inserted BEFORE merging, so
   new echoes route to the phone key during the merge (closes the mid-merge race).
4. Same hook in the LD-B seeding block: when `insert_lid_phone` newly maps a lid that has
   an existing phantom chat row, trigger the merge.
5. Known-acceptable UX (matches existing ChatDeleted behavior): if the phantom is open in
   chat_view when merged, the row vanishes without redirecting the view.

### Packet LD-D (optional hardening) — make the usync fallback real
**Files:** wacore/src/iq/usync.rs
- Parse the `<pn>` child for LID-keyed usync user nodes (second whatsmeow parity gap,
  extraction currently requires a PN-keyed node — :565-578) so the background resolver
  genuinely works as a fallback for phantoms where no new message ever arrives. Low
  priority once LD-A/B are in.

### Sequencing & verification
- LD-A → LD-B → LD-C (→ LD-D). A+B prevent new phantoms; A also heals the existing one
  at next startup; C collapses phantoms live and retroactively.
- Regression-safety notes (verified): merge goes through upsert_chat (readsync watermark
  logic preserved); edit id_remap and preview is_latest gating are untouched; message-file
  merge uses the existing atomic write path; recipient_alt is already on the Serialize
  struct — no bincode schema change.
- Unit tests: the two fixture assertions (LD-A); merge_one_lid_chat dedup/sort on a
  synthetic pair of message files.
- Live tests: (a) message the affected contact from the phone → echo lands in the NAMED
  chat, no new "+numbers" row; (b) restart → the existing "+88077045383286" phantom is
  merged into the contact's chat (messages present once, phantom row gone); (c) log shows
  "LID resolver: resolved …" outcomes instead of silence.
