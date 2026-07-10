# Fix Plan — Read-state sync: offline-unread flood + phone notification persistence

**Status: PLAN ONLY — nothing here is implemented. Execution by Opus agents after review.**

## EXECUTION INSTRUCTIONS (read first, Opus)

Every mechanism below was verified against HEAD by independent investigators AND
adversarially re-verified (file:line evidence throughout). Do not re-derive the
diagnosis; do read the cited code before editing.

**Standing rules (user-mandated, non-negotiable):**
- Tag the baseline before starting: `git tag pre-readsync-fixes`.
- One packet = one commit, each independently revertable. Append a CHANGELOG.md entry
  per packet with a revert line (follow the existing "Review-pass cleanup" format).
- Push to `github main` ONLY (`git push github main`). NEVER push to `origin`.
- Build check MUST use `cargo build --release -p whatsapp-desktop` — the desktop crate
  is NOT in default-members; a plain `cargo build --release` compiles only the root
  crate and will happily report success while the desktop code is broken.
- After the final build: `pkill -x whatsapp-deskto`, then
  `cp /home/jakes/.cache/cargo-target/release/whatsapp-desktop ~/.local/bin/whatsapp-desktop`
  — pre-authorized, do NOT ask.
- Tests: root-crate tests via `cargo test -p whatsapp-rust --lib`; desktop-crate tests
  via `cargo test -p whatsapp-desktop --bins` (it is a bin crate — `--lib` fails).
- No native dialogs ever (alert/confirm/prompt equivalents); adw::AlertDialog if needed.

**Execution order: O → D → K → R+S → G** (rationale in Sequencing section).

**Parallelization constraints (if using workflows/agents):** one owner per file.
O and K are disjoint. D owns gmessages_runtime.rs + chat_view.rs. R and S MUST be one
agent (both edit src/receipt.rs handle_receipt). O and R+S both touch
desktop/src/ui/runtime.rs (persist_new_message vs MarkRead/ReadSelf arms) — run them
sequentially, not concurrently.

**Traps already caught by adversarial review — do not re-introduce them:**
1. Packet O: `unread = 0` in the summary is SILENTLY REVERTED by upsert_chat's
   stale-preserve heuristic (runtime.rs:1490). Use `mark_chat_read_local` AFTER
   persist_chat, exactly as specified.
2. Packet K: do NOT hoist `get_missing_key_ids` onto the client-side parse — external
   snapshots/mutations hide key ids until decode attaches the blobs. Detect inside the
   processor (typed `AppStateSyncError::KeyNotFound(ids)`), as specified.
3. Packet S: parsing the `t` attr is MANDATORY before stamping receipt watermarks —
   Receipt.timestamp is currently local arrival time; skipping this over-suppresses
   genuinely-unread messages at boot.
4. Packet D: prefix BOTH ids (`tmp_id` and `real_id`) with `CHAT_PREFIX`, and guard
   with `starts_with` for retransmits.

**Live-phone acceptance:** packets O, D, R are verifiable by the user immediately;
K and S need the user's phone (test script at the end of this document). Report
build + unit-test results per packet; flag K explicitly as "needs live phone test"
in the final summary — do not claim it verified without one.

Two user reports drive this plan:

1. **Offline-unread flood** — boot desktop after it was closed; offline messages sync in
   (S1 semaphore fix works), but every chat with offline-period activity shows unread —
   *including chats whose only activity was the user's own outbound message sent from the
   phone.* Chats read on the phone during the offline window stay unread on desktop.
2. **Phone notification persists after desktop read** — reading on desktop updates the
   phone's *chat list* to read, but the Android *system notification* stays until the
   phone app is opened.

## Diagnosis summary (evidence-verified)

### Root cause A — desktop cannot decrypt the phone's read state (CONFIRMED, empirical)

- Boot log (`~/.local/share/whatsapp-desktop/app.log`):
  `Regular collection resync failed: didn't find app state key`,
  `RegularLow collection resync failed: didn't find app state key`, and **every**
  MarkRead's patch send logs `Failed to re-sync regular_low after patch send: didn't
  find app state key`.
- `whatsapp.db` → `app_state_keys` contains **exactly one key** (`key_id 00000000D5B5`)
  while 5 collection versions exist. The phone has rotated its app-state key; every new
  patch in `regular_low` (which carries `markChatAsRead`) is encrypted with a key we
  don't have.
- The client has **no key-request recovery**: on missing key it warns and gives up.
  whatsmeow sends an `APP_STATE_SYNC_KEY_REQUEST` peer protocol message to the primary
  device in exactly this situation; the phone replies with an `APP_STATE_SYNC_KEY_SHARE`
  carrying the keys.
- Consequence: phone-side reads can **never** reach the desktop. Every chat read on the
  phone during a desktop-offline window re-appears unread on desktop, forever. This is
  the flagship "inconsistent read vs mobile" complaint.

### Root cause B — read receipts cover one message id, not all (CONFIRMED)

- Phone dismisses **system notifications** per message id from read receipts (the server
  fans a companion's receipt back to your own devices as `read-self`); the **chat list**
  read state is driven separately by the app-state `markChatAsRead` mutation. User's
  symptom split (chat list correct, notification stuck) proves both halves.
- Our MarkRead arm (desktop/src/ui/runtime.rs:5262-5349) sends receipts with **exactly
  one id** — `s.last_incoming_msg_id` (`HashMap<String, String>`, one id per chat,
  runtime.rs:1129) via `client.mark_as_read(&jid, sender, vec![one_id])`.
- The comment at runtime.rs:5296-5300 claiming a receipt acts as a "read-up-to watermark"
  is **wrong** for notification bookkeeping (whatsmeow and Baileys both send full id
  lists; official clients ack every unread id).
- The stanza builder **already supports** the multi-id extension:
  src/receipt.rs:164-202 emits `<receipt type="read" id={ids[0]}><list><item id=…/></list>`
  for `ids[1..]`. Capability exists; never exercised.
- Mirror-image gap: our incoming `handle_receipt` (src/receipt.rs:29-59) reads only the
  `id` attr and **ignores `<list><item>` children** — multi-id receipts from the phone
  are under-processed by us too (only first id updates tick status, runtime.rs:3058-3068).

### Root cause C — "outbound-only chats flagged unread" (VERIFIED — three mechanisms, two hypotheses refuted with proof)

**Perception correction (disk-proven):** decoding all 786 `wa_messages/*.bin` files for
the offline window shows own-phone-sent echoes are classified correctly (453 messages
`from_me=true`, ZERO own-LID/phone messages misclassified; LID comparison uses
`matches_user_or_lid` against both phone JID and LID — wacore/src/messages.rs:167,177-189,
jid.rs:448-450; own LID persisted at pairing and refreshed on every `<success>`). The
"outbound-only" chats actually had offline INCOMING the user read+replied on the phone;
the reply is the last message, so the row preview reads "You: …". H1 (LID
misclassification) REFUTED. H5 (gm reseed) REFUTED — gm unread comes straight from
Google's flag (gmessages_runtime.rs:3129) and the watermark is clamp-down-only.

The badges are *created* legitimately (offline incoming bumps unread) and then can never
be cleared, because ALL THREE phone→desktop read channels fail:

- **C-1 (= root cause A, H2 verified major):** app-state key recovery is DEAD CODE. The
  `AppStateSyncKeyRequest` sender EXISTS (src/client.rs:2661-2698) but both call sites
  (client.rs:2432, :2639) sit AFTER the decode `?` that hard-fails with KeyNotFound
  (processor.rs:95/120/229/255) — the request only runs when no key is missing. The typed
  error is stringified en route (`map_err(|e| anyhow!("{}", e))`, appstate_sync.rs:254/343)
  so the retry guard's downcast (client.rs:2173-2177) can never match, and
  `AppStateSyncError::KeyNotFound` is never even constructed on the fetch path. The
  key-share handler (src/message.rs:1360-1430) only notifies on the FIRST-ever share
  (atomic gate 1422-1428) and nothing re-triggers a failed collection sync on key arrival.
- **C-2 (H3 verified major):** offline self-read receipts race ahead of the serialized
  message backlog and are dropped. Receipts are spawned as DETACHED tasks
  (client.rs:1339-1351) while messages drain serially through the 1-permit semaphore —
  a self-read receipt arrives before the messages it covers exist in history.
  `handle_receipt` (src/receipt.rs:29-68) never parses the `recipient` attr (a DM
  self-receipt gets chat = own JID) NOR the `t` attr (timestamp = local arrival time);
  the desktop's fallback scan can wrongly clear a message-yourself chat; group
  `is_own_read` depends on own_lid/own_phone which populate late (after Connected).
- **C-3 (H4 verified major — the biggest perceived symptom):** an own outbound echo
  NEVER clears unread. persist_new_message carries the old count forward with the NEW
  timestamp (runtime.rs:4255-4263, 4330-4335) and upsert_chat re-sorts by timestamp —
  the from_me echo floats the stale badge to the top of the list. UI side identical
  (chat_list.rs:773-775 has no from_me else-reset). WhatsApp semantics: your own message
  marks the chat read. BONUS: system messages (group events, is_from_me=false,
  is_system_message=true) bump the UI badge ungated — zero `is_system_message` checks in
  chat_list.rs/window.rs.

---

## Work packets

### Packet K — Make app-state key recovery reachable (root cause A / C-1) — CRITICAL
**Owner files:** wacore/src/appstate_sync.rs, wacore/appstate/src/processor.rs (or top of
process_patch_list), src/client.rs, src/message.rs

The request/share machinery ALREADY EXISTS — it is unreachable. Fix per the
adversarially-corrected plan (do NOT hoist `get_missing_key_ids` onto the client-side
`parse_patch_list` output — that misses key ids living inside EXTERNAL blobs
(snapshot_ref / external_mutations), which only get attached inside decode_patch_list;
a hoisted check would report nothing missing for external snapshots and decode would
still hard-fail with no request sent):

1. **Detect missing keys inside the processor AFTER externals are attached** — at the
   top of process_patch_list (wacore/src/appstate_sync.rs:210-217) or in prefetch_keys:
   collect key ids whose `get_app_state_key` fails and return a typed
   `AppStateSyncError::KeyNotFound(ids)` instead of proceeding to decode. One choke
   point → fires for single, batched, AND post-patch-send resyncs alike.
2. **Preserve the typed error:** replace `.map_err(|e| anyhow!("{}", e))` at
   appstate_sync.rs:254 and :343 with `.map_err(anyhow::Error::new)` so the existing
   downcast guard at client.rs:2173-2177 finally works as written.
3. **Fire the request from that guard:** move the existing dedup+request block
   (client.rs:2620-2640) into the KeyNotFound branch; drop the
   `!initial_app_state_keys_received` precondition; allow the wait-retry on attempts
   2-3 (not just attempt==1). Throttle: one request per collection per connect session.
4. **Re-sync on key arrival:** in handle_app_state_sync_key_share (src/message.rs:
   1360-1430), notify unconditionally when stored_count>0 (the atomic first-share gate
   currently swallows later shares — benign for the only other listener, which is
   fresh-pairing-only and separately gated) and drain a `key_blocked_collections` set
   into a bounded resync (safe vs the app_state_syncing in-flight dedup: a skipped
   resync is covered by the in-flight waiter's own retry).
5. Log request-send and key-share arrival at info level (current receipt/appstate debug
   is invisible: main.rs:153 filters `whatsapp_rust=warn`).
6. Verified downstream (no changes needed): replayed regular_low mutations arrive with
   full_sync=false when collection version > 0 → live path runtime.rs:3193-3198 →
   mark_chat_read_local → watermark + ChatReadOnOtherDevice → UI clears. Snapshot
   (full_sync=true) mark-reads stay deliberately ignored (runtime.rs:3190-3192).

**Acceptance:** boot log shows KeyNotFound → key request → key share → resync success;
chats read on phone during desktop-offline show read at next boot without touching them.
`sqlite3 whatsapp.db "SELECT COUNT(*) FROM app_state_keys"` grows past 1.
**Risk:** protocol-level; needs a real phone test. Whole packet is one commit, revertable.

### Packet R — Multi-id read receipts (root cause B) — MAJOR
**Owner files:** desktop/src/ui/runtime.rs (MarkRead arm), src/receipt.rs (inbound list parse)

1. In the MarkRead arm, capture `prev_wm = read_watermarks[chat]` **before**
   `mark_chat_read_local` bumps it (bump at runtime.rs:5271 → 1379-1407).
2. Collect all unread incoming ids: `s.history[chat]` filtered
   `!is_from_me && timestamp >= prev_wm` (`>=` not `>` — second-granularity watermark;
   re-acking a read id is idempotent/harmless). Disk fallback via `load_messages(chat)`
   (outside the state lock) when the history LRU has evicted the chat. Ultimate fallback:
   existing `last_incoming_msg_id` anchor. Dedupe, chronological order, **cap ~100 ids**
   (newest guaranteed included).
3. DMs: one `mark_as_read` call with the full id vec (builder already emits list/item).
   Groups: **batch ids by sender_id**, one call per sender with that sender as
   `participant` (whatsmeow semantics). Keep the existing LID→phone retry ladder per batch.
4. Inbound mirror fix: `handle_receipt` (src/receipt.rs:29-59) must also parse
   `<list><item id=…/>` children so multi-id receipts from the phone update tick status
   for every id, not just the first.
5. Do NOT change receipt `type="read"` behavior in this packet (the read-self privacy
   variant for receipts-off DMs is a separate decision — noted in DECISIONS.md addendum).

**Acceptance:** phone notification tray holding N messages clears fully when the chat is
read on desktop. Ticks update for all messages when reading on phone.
**Risk:** low — builder already supports the format. Reversible: single commit.

### Packet O — Outbound-only unread mechanism (root cause C)

<!-- PENDING: exact packet from workflow verdicts. Candidates under investigation:
  H1: own-echo is_from_me mis-detection under LID addressing (wacore parse vs own_lid)
  H3: self-read receipts from offline backlog dropped / ordering race vs unread bumps
  H4: UI badge bump paths not gated on is_from_me / is_system_message; own outbound not
      clearing stale badge (WhatsApp Web behavior: own message clears chat unread)
  H5: gm reseed deriving unread from timestamp-vs-watermark alone, ignoring direction /
      Google's read flag -->

### Packet O — Own outbound message marks the chat read (C-3, H4) — MAJOR, biggest visible win
**Owner files:** desktop/src/ui/runtime.rs, desktop/src/ui/chat_list.rs

⚠️ **The naive fix is silently reverted — do it exactly this way.** Setting `unread = 0`
in persist_new_message's summary does nothing: persist_chat calls
`upsert_chat(authoritative_unread=false, from_reseed=false)`, whose stale-preserve
heuristic (runtime.rs:1490: non-authoritative summary with unread=0 while existing>0 →
restores the old count) undoes it. The watermark stamp is mandatory, not optional:

1. **runtime.rs — after `persist_chat(state, summary)` at ~4345:**
   `if m.is_from_me && msg_is_new && !m.is_system_message {
        state.lock().unwrap().mark_chat_read_local(&chat_id); }`
   mark_chat_read_local (1379-1407) is the rework's own primitive (already used by
   ReadSelf / live MarkChatAsReadUpdate / WaCommand::MarkRead): zeroes the count
   POST-upsert (1490 can't restore it), stamps the read watermark at the chat's
   now-updated timestamp, persists chats + watermark map. Composes with the from_reseed
   clobber defense (1510-1515) so later stale reseeds can't resurrect the badge.
2. **chat_list.rs:773 (update_last_message):** add `!msg.is_system_message` to the
   increment condition, plus an else-branch
   `else if msg.is_from_me && is_newer && !msg.is_system_message { row.set_unread(0); }`
   so the UI badge clears immediately on the echo (not only at next ChatsLoaded).
3. **runtime.rs:4256:** add `&& !m.is_system_message` to should_bump — dead-code defense
   today (GroupUpdate bypasses persist_new_message) but cheap insurance.

**Effect on the user's symptom:** during the offline flush the phone-sent reply replays
chronologically AFTER the incoming that bumped the badge → clears exactly the chats he
complained about, matching WhatsApp Web semantics. Also stops group-event system
messages from bumping badges.
**Acceptance:** send from phone while desktop closed → boot → chats where you replied
show read; group-event-only chats show no badge.

### Packet S — Offline self-read receipts: order-independent handling (C-2, H3) — MAJOR
**Owner files:** src/receipt.rs, desktop/src/ui/runtime.rs (ReadSelf arm), src/client.rs (identity init)

Adversarially-corrected plan (one mandatory correction included):

1. **src/receipt.rs (handle_receipt, 29-68):** parse the `recipient` attribute; if
   `from` matches own pn/lid user, set `source.chat = recipient` (non-AD) and
   `source.is_from_me = true` (whatsmeow parseMessageSource parity — fixes DM
   self-receipts resolving to chat = own JID). **MANDATORY:** also parse the stanza `t`
   attr into Receipt.timestamp (currently hardcoded `now_utc()` at :65 — without this,
   boot-time receipts stamp the watermark at connect time and would suppress badges for
   messages genuinely unread on the phone). Fall back to now_utc only when `t` absent.
2. **runtime.rs ReadSelf arm (~3123-3134):** stamp a receipt-time watermark:
   `wm = max(prev, receipt.timestamp)` + persist — decoupled from the chat's stale
   timestamp; remove the fallback that can wrongly clear a message-yourself chat
   (addendum from verification: the own-JID fallback push at 3120-3122 must no-op, not
   guess).
3. **Gate the later unread bump by the receipt watermark** so ordering stops mattering:
   in persist_new_message, suppress the bump when `m.timestamp <= receipt-derived wm`.
   ⚠️ Design tension (explicit decision): gating on ALL watermarks partially
   reintroduces the same-second-suppression hazard the from_reseed redesign avoided.
   Preferred: keep a SEPARATE receipt-derived watermark map (or a flag alongside the
   entry) and gate the live bump only on that; otherwise document the 1-second edge.
4. **Identity init hardening:** populate s.own_phone/s.own_lid from the persistence
   snapshot BEFORE bot.run() (currently set inside the concurrently-spawned Connected
   handler after awaits — group is_own_read races it).

**Acceptance:** read a chat on the phone while desktop is closed (and keep it closed a
minute), boot → chat shows read even though the receipt raced the backlog.
**Note:** shares src/receipt.rs with Packet R (multi-id inbound parse) — same owner
must implement both receipt.rs changes in one pass.

### Packet G — gm staleness hardening (H5 optional; H5 itself REFUTED)
**Owner files:** desktop/src/gmessages_runtime.rs

In the WaCommand::MarkRead handler (~1893, after save_gm_watermarks), also rewrite
gm_chats.bin with unread_count=0 for the marked conv(s) (reuse gm_load/save_chats_cache)
so a no-reseed boot (e.g. the observed `list_conversations failed: rpc timeout`) doesn't
depend solely on the watermark clamp to hide an already-read badge. Low priority.

### Packet D — SMS double-bubble on desktop send (display-only, intermittent)
**Owner files:** desktop/src/gmessages_runtime.rs, desktop/src/ui/chat_view.rs (+ window.rs handler)

User report: outbound SMS sent from desktop sometimes renders twice; first copy single
tick (optimistic), second double ticks (server echo). Phone shows a single send.

**VERIFIED mechanism (live-log confirmed — the reconcile path is dead code due to two
id-form mismatches):**

1. Optimistic bubble keyed `tmp-<nanos>-<seq>` (chat_view.rs:2387,2397; gen_tmp_id
   4277-4291). Bubbles map is keyed exactly by `msg.id`; the ONLY dedup is exact-id
   match (chat_view.rs:3249, 3523). No content/timestamp fallback.
2. `session::send_text` (gmessages-rust/src/session.rs:420-469) does NOT return a server
   id — it returns its own GM-protocol tmp id `tmp_XXXX`; the real id arrives later via
   longpoll.
3. RPC confirm path tags it `gm:tmp_XXXX` and re-keys the bubble to that
   (gmessages_runtime.rs:1583-1594 → confirm_bubble chat_view.rs:3226-3236). Fine so far.
4. **BUG:** the longpoll echo's `translate_event` (gmessages_runtime.rs:2615-2635) emits
   `MessageConfirmed { tmp_id: "tmp_XXXX", real_id: "180207" }` with **RAW un-prefixed
   ids** — but the bubble is keyed `gm:tmp_XXXX`, and the echo MessageReceived is tagged
   `gm:180207` (2862-2866). Both lookups miss → re-key never happens → the echo passes
   the RecentMsgRing (first sighting of `gm:180207`) and appends a SECOND bubble.
5. Happens on essentially **every** send where the chat stays open ~1-2s; perceived
   intermittency is self-healing: chat switch/reopen rebuilds bubbles from disk
   (chat_view.rs:2940-2942), which holds only ONE copy — persist site is the longpoll
   branch only (gmessages_runtime.rs:1425), deduped by `have` set on load (1730-1736).
   Confirmed display-only.

**Fix:**
1. Primary (two lines): at gmessages_runtime.rs:2623-2628, prefix both ids with
   `CHAT_PREFIX` (`gm:`) like every other site (guard `starts_with(CHAT_PREFIX)` for
   retransmits). `MessageConfirmed` is pushed before `MessageReceived` in the same batch
   and window.rs processes in order → bubble re-keys `gm:tmp_XXXX` → `gm:180207` right
   before the echo → exact-id dedup at chat_view.rs:3249 hits → no duplicate.
2. Hardening (residual race — echo lands before send_text returns, bubble still keyed
   `tmp-…`): in confirm_bubble (chat_view.rs:3226), if real_id already exists in the map,
   remove the optimistic widget from messages_box instead of stacking; optionally match
   is_from_me gm echoes that miss the map to a pending `tmp-` bubble by identical text +
   recency (few seconds) and update in place (append_bubble_to_inner_at chat_view.rs:3242).

**Acceptance:** send SMS from desktop, keep chat open → exactly one bubble that
transitions single→double tick in place. Reopen shows one bubble (already the case).

### Sequencing & verification

Recommended order (each packet = one commit, independently revertable):

1. **O first** — smallest diff, biggest immediate visible win (clears exactly the chats
   the user complained about, using an existing primitive). Zero protocol risk.
2. **D second** — two-line primary fix, kills the double bubble.
3. **K third** (root cause) — kills the whole "read on phone, unread on desktop" class.
   Protocol-layer; needs a live-phone test before calling it done.
4. **R + S together fourth** — both edit src/receipt.rs handle_receipt; ONE owner does
   both in one pass (R: multi-id `<list><item>` inbound parse + outbound full-id-list
   receipts; S: recipient/t attrs + receipt-time watermark).
5. **G last** — optional hardening.

File-ownership note for parallel execution: O and K don't overlap; D owns
gmessages_runtime.rs + chat_view.rs; R+S own receipt.rs + the runtime.rs MarkRead/
ReadSelf arms — if O and R+S run concurrently, coordinate the runtime.rs regions
(persist_new_message vs MarkRead/ReadSelf arms; disjoint but same file — prefer
sequential).

Verification requirements per packet:
- Build BOTH crates: `cargo build --release -p whatsapp-desktop` (desktop crate is NOT
  in default-members — a plain `cargo build --release` silently skips it).
- Unit-testable: receipt stanza construction incl. list/item (R); `t`-attr parse (S);
  own-echo clears persisted unread + watermark stamp survives upsert_chat heuristics (O);
  gm id-prefix reconcile (D — assert MessageConfirmed ids carry CHAT_PREFIX).
- Live-phone test script for the user after install:
  a) read a chat on phone while desktop closed → boot → chat shows read (K, S);
  b) reply from phone while desktop closed → boot → that chat shows read (O);
  c) 3+ messages in one Android notification → read on desktop → notification fully
     clears (R);
  d) send SMS from desktop, keep chat open → exactly one bubble, single→double tick in
     place (D);
  e) `sqlite3 whatsapp.db "SELECT COUNT(*) FROM app_state_keys"` > 1 after K.

## Open decisions (for DECISIONS.md)

- **read-self receipts for receipts-off DMs:** our `mark_as_read` hardcodes
  `type="read"`. If the user ever disables read receipts, official clients send
  `read-self` to sync own devices without blue-ticking the sender. Implement now or defer?
- **Key-request trust surface:** accept `APP_STATE_SYNC_KEY_SHARE` only from own primary
  device (verify sender JID/device) — must be part of Packet K's implementation.
