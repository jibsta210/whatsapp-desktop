# Final Review Report — audit-fix batches (`pre-audit-fixes..HEAD`, 13 commits)

Independent review pass over all ~3,000 changed lines across 16 files. Six parallel
reviewers, each owning a disjoint file set; every finding below was verified against
current HEAD code (not just the diff), and the three worst claims were re-verified
by hand. Line numbers are current-HEAD.

**Verdict:** the big structural work held up — persistence formats are byte-identical
to the pre-audit baseline for all WA `.bin` files (upgrade-safe), all new events are
emitted AND handled on both sides, no new panics on network/disk data, no forbidden
native dialogs, build is clean (zero new warnings), CHANGELOG covers every commit.
But the review found **4 critical**, **13 major**, and ~20 minor real bugs, mostly
in the newest code (typing indicator, live-name/reaction updates, gm parity, and one
residual edge of the offline-loss fix).

---

## CRITICAL

### C1 — Hidden/tray window silently blue-ticks incoming messages
`SetActiveChat` is sent from exactly 3 sites (window.rs:121, 149, 518) — always
`Some(...)`. **Nothing ever sends `None`.** Close-to-tray (window.rs:439–472) only
does `set_visible(false)`. While in the tray with a chat "active":
- runtime suppresses the persisted unread bump (runtime.rs:4188–4191) → wrong badges after restart;
- window.rs:952 `if (is_current_chat || auto_mark) && !msg.is_from_me { MarkRead }` fires
  → **real read receipts sent to the sender for messages never seen**;
- that MarkRead → `ChatReadOnOtherDevice` → withdraws the very notification just posted
  (window.rs:1124), so the new tray-chime chimes for a self-cancelling notification.

**Fix:** send `SetActiveChat { chat_id: None }` in the close-to-tray branch; re-send
`Some(current)` on present/restore; gate window.rs:952 on `inner.window.is_active()`.

### C2 — `contacts_directory.bin` silently wiped by new `name_priority` field
`ContactEntry` gained `name_priority: u8` (contacts.rs:~80). bincode ignores
`#[serde(default)]` — old files fail to decode; `load()` (contacts.rs:149–152) does
`.ok() → unwrap_or_default()` → empty directory; the next `save_if_dirty()` overwrites
the old file. Exact same schema-drift trap the gm_chats guard in this batch was built
to prevent. (Note: the installed build has likely already migrated/wiped the local
file once — the fix pattern still must land for any future field.)

**Fix:** on decode failure retry a `LegacyContactEntry` (without the field) and migrate
with priority 0; preserve undecodable files as `.corrupt` like `gm_load_chats_cache`.

### C3 — Opening a chat with a saved draft broadcasts "typing…" to the contact
`open_chat` sets `current_chat_id` (chat_view.rs:2709) then `buffer().set_text(&draft)`
(2727) → `connect_changed` fires → typing block sees non-empty text → `SetTyping{true}`
sent just from clicking a chat. Same false fire from `restore_failed_edit` (2637) and
`show_event_creator` (5185).

**Fix:** `suppress_typing: Cell<bool>` guard around every programmatic `set_text`.

### C4 — No typing cleanup on chat switch — old chat stuck on "typing…"
`open_chat` (chat_view.rs:2655–2711) never removes `typing_stop_source`, never sends
`SetTyping{false}` to the old chat, never resets `typing_last_true_ms`. Type in A,
switch to B, type within 4s → A's stop timer is stolen and **`false` for A is never
sent** (recipient sees "typing…" indefinitely); the global throttle also suppresses
B's `true` for up to 3s.

**Fix:** shared `cancel_typing(inner, flush: bool)` helper — call in `open_chat`,
`do_send`, and teardown; store the routed target with the SourceId so the flush hits
the right chat. (Fixes C3/C4 + two typing minors together.)

---

## MAJOR

### Offline-sync / semaphore (src/)
- **S1 — Disconnect during backlog drain still permanently drops acked messages.**
  `cleanup_connection_state()` (client.rs:1157) still calls `swap_message_semaphore(1)`
  → generation bump. The dispatcher acks the transport *before* the per-chat worker runs
  (client.rs:1556), so workers that fail the generation check at message.rs:601–609
  `return` — message acked to server, never decrypted, never retried. Same loss class
  as the fixed bug; trigger = network flap during a large offline backlog.
  **Fix:** on generation mismatch, don't drop — re-read `(generation, semaphore)` under
  the mutex and re-acquire on the new semaphore, then process (session store is durable).
- **S2 — CAS-then-widen race can leak 63 permits into the NEXT connection's offline sync**
  (sessions.rs:32–45): between the `offline_sync_completed` CAS and `widen(63)`, a
  disconnect's `swap(1)` can interleave → next sync runs at 64 permits (ordering broken,
  spurious decrypt failures — not loss). The comment also contradicts the code order.
  **Fix:** perform CAS + `add_permits` while holding the semaphore mutex.

### Read/unread core (runtime.rs)
- **R1 — Reseed-defense swallows live unread bumps** (runtime.rs:1491–1497): the
  watermark guard runs on EVERY upsert including `persist_new_message`; watermarks are
  second-resolution `wm >= incoming_ts`, so a message arriving in the same second as a
  local mark-read (or from a skewed sender clock) gets its unread bump zeroed.
  **Fix:** `from_reseed: bool` param on `upsert_chat`; live-message path passes false
  (and/or strict `>`).
- **R2 — First message of a brand-new chat persists unread=0** (runtime.rs:4206–4208):
  the new-chat else-branch hardcodes 0; new contact messaging while away shows read
  after restart. **Fix:** `unread = (!m.is_from_me && active != chat) as u32`.

### message_bubble.rs
- **B1 — `update_sender_name` renames the WRONG label** (:258 vs :354): the stored
  `sender_label` is the *quoted-reply* sender's caption; the real group sender label is
  never stored. Live name resolution no-ops on normal bubbles and overwrites the quoted
  person's name with the replier's on reply bubbles; `set_text` also wipes the colored
  markup. **Fix:** store the :354 label; update via `set_markup` +
  `name_to_colour(sender_id)` + escape.
- **B2 — Re-download of a deleted media file is a dead end** (:987 vs :596–608):
  `media_loaded` seeds from stale `media_local_path.is_some()`, so `set_media_loaded`
  early-returns when `MediaReady` arrives — placeholder stuck on "⏳ Downloading…"
  forever, for exactly the feature's target case. **Fix:** seed from the
  exists-filtered path.

### chat_view.rs
- **V1 — Typing in a send group sends `SetTyping` with virtual id `"sendgroup::Name"`**
  (:1517–1529 vs :2835) — malformed JID hits the bridge every 3s. **Fix:** skip typing
  when in send-group mode.
- **V2 — Event creator hijacks composer state** (:5177–5187): `set_text(&msg)` +
  `do_send` — a staged image gets sent with the event text as caption; an active edit
  turns the event into an edit of an old message; any draft is destroyed. **Fix:** send
  event text directly through SendText routing, bypassing the composer.
- **V3 — `restore_failed_edit` clobbers newly-typed text** (:2633–2641) and re-arms
  `editing_msg` so the next Enter fires an unintended edit. **Fix:** only restore when
  the buffer is empty; otherwise toast with a "Restore" action.

### window.rs / settings.rs
- **W1 — "Log out / Unlink device" neither logs out nor unlinks** (settings.rs:377–384
  → runtime.rs:5280 = plain `disconnect()`): device stays linked on the phone, session
  intact, silent reconnect on next launch — contradicting the dialog's "re-scan the QR
  code" copy. No feedback after confirming. **Fix (minimum):** delete `whatsapp.db` +
  `wa_*.bin`, route to QR screen, InfoToast; proper fix = companion remove-device IQ.

### gmessages / contacts (SMS side)
- **G1 — LID index-pollution fix doesn't survive restart** (contacts.rs:161–168):
  `load()` rebuilds `by_suffix_10/7` for every entry including `@lid`-keyed ones, so
  the wrong-contact fuzzy-match bug returns after one restart. **Fix:** persist an
  `is_lid` flag (fold into the C2 migration) and honor it in `load()`.
- **G2 — Resend button is a dead no-op for every failed SMS** — `ResendMessage` routes
  to the gm runtime which has no arm for it (dies at gmessages_runtime.rs:1931).
  **Fix:** handle it as `send_text`, re-emit `MessageFailed` on error.
- **G3 — `SendContact` to an SMS chat: bubble stuck on ⏳ forever** — same silent-drop
  class Batch 4 fixed for GIF/sticker/voice, missed for contact cards. **Fix:** add to
  the MessageFailed arm (gmessages_runtime.rs:1920–1928).
- **G4 — Longpoll backoff cap applied to only one of two branches** (longpoll.rs:233):
  the HTTP non-success branch still computes `(error_count + 1) * 5` uncapped — a
  sustained 5xx outage delays SMS recovery unboundedly. **Fix:** same `.min(60)`.
- **G5 — Mark-as-unread on an SMS chat is undone on restart** — `MarkUnread` routes
  only to the WA runtime (runtime.rs:1671–1674); gm watermark still says "read" and
  re-clamps unread to 0 at reseed. **Fix:** fan MarkUnread to the gm runtime and roll
  its watermark back.
- **G6 — 2FA/shortcode conversations can never be marked read** — live 2FA messages
  merge into synthetic `gm:verification-codes`, but reseed emits the underlying
  shortcode convs unfiltered with Google's unread flag, and MarkRead watermarks/ACKs
  the nonexistent conv id `"verification-codes"`. Shortcode rows re-flag unread every
  reseed. Residual of the flagship "SMS unread reappearing" bug. **Fix:** record
  conv→verification-codes in the merge map; fan MarkRead to the real conv ids; skip
  the bogus server ACK.

---

## MINOR (grouped by owner file)

**runtime.rs** — commands all `tokio::spawn`ed, so rapid chat switches can apply
`SetActiveChat` out of order (apply it inline in the select loop); watermark saves
race each other via per-call threads (use a single writer channel like `save_tx`);
`last_incoming_msg_id` never seeded by history-sync → no read receipt anchor after
restart; incoming reaction for a non-cached chat now dropped entirely (old code
emitted unconditionally); self-reaction dedup keys differ between send site and
phone-echo site (possible duplicate own reactions); `atomic_write` doesn't fsync the
parent dir and orphaned `.tmp` files are never swept.

**chat_view.rs** — typing throttle uses wall clock (use `glib::monotonic_time`);
`do_send` doesn't flush `SetTyping{false}`; phantom mention ping when one name
prefixes another (`@Ann` vs `@Anna` — reuse the boundary check for the keep-decision);
audio preview can't replay after ended (needs `seek(0)`); quick-reply dialog silently
discards on empty shortcut (disable Save until non-empty); event-creator window leaks
per open (strong ref cycle — use `#[weak]`); react-picker own-reaction highlight uses
a stale snapshot.

**message_bubble.rs** — `set_use_markup(true)` after `set_markup_safe` defeats the
plain-text fallback (:657, :1044 — delete both); tier-2 validation trusts unvalidated
original markup (harden by checking stripped `<a>` regions match generated shape);
download placeholder has no failure/retry path (one-way latch); poll `live_votes`
snapshot goes stale after live echoes (momentarily erases others' votes); preview-card
click passes sender-controlled URLs to xdg-open with **no scheme allowlist** (allowlist
http/https — do this one, it's security-adjacent) and both launchers hard-depend on
`setsid` with errors discarded; lazily-added edit caption renders below the timestamp.

**window.rs / chat_list.rs** — `app.open-chat` doesn't sync the sidebar selection;
brand-new chat row seeds unread=1 even when you're viewing it (chat_list.rs:621–635);
unread increment has no message-id dedup (same-id redelivery double-counts); no toast
dedup (reconnect loop = unbounded toast backlog).

**gmessages_runtime.rs / contacts.rs** — optimistic reaction event sends
`vec![("", emoji)]`, wiping other participants' reactions until reload (merge with
persisted set); `participant_i_ds.first()` collapses multi-reactor emojis;
clock-skew hole for message-less chats (watermark = desktop-now only);
empty-cache guard makes gm_chats.bin grow-only (no eviction of phone-deleted convs;
"Clear SMS cache" uses cwd-relative paths and skips gm_read_watermarks.bin);
`save_gm_watermarks` and `ContactDirectory::save_if_dirty` still use bare `fs::write`
(route through atomic_write); failed sends not persisted → red-✗/Resend record lost
on restart.

**Docs** — CHANGELOG revert instructions don't list per-batch commit hashes (add:
1b1a9b2, 7f6faec, 2103c19, e481f80, 67c5a3d, 9721014, 393ee62, 0ed99a0, aaca2c1,
d9632f2, a5cf996, 454d9fb, ef6f2af).

---

## VERIFIED CLEAN (no action)

- **bincode/upgrade safety for WA files**: all persisted structs in wa_chats.bin,
  wa_messages/*.bin, watermarks, gm_chats.bin byte-identical to baseline; WA02 header +
  legacy decoders + `.corrupt` preservation intact; tripwire tests present.
  (contacts_directory.bin is the one exception — C2.)
- **Markup injection**: text escaped before linkify; literal `<a href>` in a message
  stays inert; hrefs can't contain raw quotes. Path traversal via attachment names
  blocked by the write-time sanitizer.
- **Event wiring**: all new variants emitted + handled, no `_ =>` swallowing.
- **Lock discipline** in the semaphore code (std Mutex, never held across await).
- Mute gate polarity, 2FA `is_gm` guard, sort tiebreak, filter-chip veto, SourceId
  double-remove safety, HEIC fallback, SendReply→SMS downgrade, receipt-at-construction,
  `rebuild_reactions` dedup, no native dialogs, build clean.

---

## EXECUTION PLAN — work packets for Opus agents

Partitioned strictly by file ownership (parallel agents must never share a file).
P1–P5 can run in parallel. P6 (semaphore) should run alone, after, with full attention.

| Packet | Files (exclusive) | Items |
|---|---|---|
| **P1** | window.rs, chat_list.rs, settings.rs | C1, W1, open-chat sidebar sync, new-chat unread seed, unread id-dedup, toast dedup |
| **P2** | chat_view.rs | C3, C4, V1, V2, V3 + chat_view minors (one `cancel_typing` helper + `suppress_typing` guard covers C3/C4/throttle/do_send) |
| **P3** | contacts.rs | C2 (legacy decoder + `.corrupt`), G1 (persist `is_lid`), atomic save |
| **P4** | gmessages_runtime.rs, longpoll.rs | G2, G3, G4, G6 (gm side), G5 (gm handler arm), reaction merge, atomic watermark save, minors |
| **P5** | message_bubble.rs | B1, B2, `set_use_markup` deletions, URL scheme allowlist, minors |
| **P6** | src/client.rs, src/client/sessions.rs, src/message.rs | S1 (re-acquire on generation mismatch), S2 (CAS under mutex) — **solo, sequential, with a regression test** |
| **P7** | runtime.rs, bridge.rs (if needed), CHANGELOG.md | R1, R2, MarkUnread fan-out routing (G5 runtime side), runtime minors, changelog hashes |

Coordination note: G5 spans P4 (gm handler) and P7 (routing) — each packet touches only
its own file; the feature completes when both land. Same for C1's `is_active()` gate
(P1 only).

Ordering: P1–P5 + P7 in parallel → build + fix conflicts → P6 solo → full release
build → install → single CHANGELOG entry ("Review-pass cleanup") with this report
referenced → commit per packet or one commit, tagged `pre-cleanup` beforehand for
reversibility.
