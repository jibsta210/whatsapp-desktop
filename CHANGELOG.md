# Changelog — audit fix batches

Automated execution of the audit fix plan (`AUDIT.md`). Judgment-call items are **not**
applied here — see `DECISIONS.md`.

**Reversibility**
- Baseline before any of these fixes: tag **`pre-audit-fixes`** (commit `54aef73`).
- Undo *everything*: `git reset --hard pre-audit-fixes`.
- Undo *one* batch: `git revert <that batch's commit>` (each batch is a single, self-contained, build-verified commit).

---

## Group SMS threads no longer merge into a 1:1 WhatsApp chat

A group SMS thread was matched to a WhatsApp DM by taking its FIRST non-me
participant, so a group Alexandra belongs to was folded into the private chat with
her — showing the group's roster as the chat header. Worse, the link is persisted in
the contact directory and `other_chat_id` drives send-routing, so an SMS sent from
her private chat could have gone to the entire group.
- **desktop/src/gmessages_runtime.rs**: never derive a merge phone for a group thread
  (`is_group_chat`, >1 visible non-me participant, or >1 other_participant), and heal
  any previously-recorded bad link on reseed (clears the directory entry + hot cache).
- **desktop/src/contacts.rs**: `clear_chat_id(source, chat_id)` removes stale
  cross-protocol links without dropping the contact. Test:
  `clear_chat_id_heals_a_bad_group_merge`.

## Save button fix: voice notes now actually expose it

The new "Save to Downloads" menu item gated on the message's persisted media path — which
voice notes never had: the download write-back silently skipped chats not in the RAM cache,
and the menu's captured message clone predates on-demand downloads anyway.
- **desktop/src/ui/message_bubble.rs**: bubbles track their live on-disk media path
  (set at build + on MediaReady) and expose it via `media_path()`.
- **desktop/src/ui/chat_view.rs**: the menu prefers the bubble's live path over the stale
  message clone.
- **desktop/src/ui/runtime.rs**: the download write-back loads history from disk when the
  chat isn't cached, so `media_local_path` always persists (voice notes no longer show
  re-download placeholders after restart).
- Note: voice notes downloaded before this fix still lack a persisted path — tapping one
  re-fetches and permanently heals it.
- Revert: `git revert <commit>`.

## Save media from the bubble menu

Voice notes (and all media) were auto-saved internally but unreachable from the UI — only
the image viewer had a save button.
- **desktop/src/ui/chat_view.rs**: right-click / chevron menu now offers **"Save to
  Downloads"** on any message with a downloaded media file (voice notes, images, video,
  documents); copies to ~/Downloads under a clean name (idempotent, collision-suffixed)
  and confirms with a toast showing the destination.
- Revert: `git revert <commit>`.

## Updater: "Restart now" button + don't clobber local builds

- **desktop/src/bridge.rs, desktop/src/updater.rs, desktop/src/ui/window.rs**: the
  update-ready toast now carries a **Restart now** button (wired to the existing
  `restart_to_apply`) and no longer auto-dismisses, instead of making the user find the
  button in Settings.
- **desktop/src/updater.rs**: a locally-built binary reports `build 0`, so EVERY channel
  release compared as newer and would silently replace a local build containing newer work
  (a Jul-15 release was staged over a Jul-24 build carrying the read-sync fixes). Local
  builds are no longer auto-staged; an explicit check in Settings still updates.

## Read-sync + mention regressions after protocol v0.6.0

The v0.6.0 merge resolved src/receipt.rs, wacore/src/messages.rs and src/message.rs to
"theirs", dropping local read-sync work (and its regression tests, which is why CI stayed
green). Root causes found and fixed:

### app-state key request was addressed to ourselves (CRITICAL)
`request_app_state_keys` used `device.pn` verbatim — which carries THIS desktop's device id
(`:2`) — so the request was encrypted to a session we can never hold. 81 log occurrences of
`session with <own-lid>:2@lid.0 not found`. The key never arrived, RegularLow stayed
permanently undecodable, its version froze, and the outbound markChatAsRead patch was built
on a stale base version the server rejects — so reads never reached the phone.
- **src/client.rs**: target `Jid::to_non_ad()` (device 0 = the primary phone), matching
  whatsmeow's `getOwnID().ToNonAD()`.
- **src/message.rs**: key-share handler notifies on EVERY share (not just the first-ever)
  and re-syncs the app-state collections, so a rotated key unblocks the deferred
  markChatAsRead patches immediately instead of waiting for a reconnect.
- **desktop/src/ui/runtime.rs**: the mark_chat_as_read failure log was `debug!` — invisible
  at the default level, which is how this hid for days. Now `warn!`.

### @mentions in media captions rendered as raw LID digits
`resolve_mentions` was applied to `text` and `quoted_text` but never `media_caption`, so an
@mention written on an image/video persisted and rendered as `@231696725180510` forever.
- **desktop/src/ui/runtime.rs**: new `resolve_message_mentions` covering all three fields,
  called from the live-incoming, LoadChat, and history-sync push paths (the third was
  missing, leaving synced captions raw until the chat was reopened).

### receipt parsing restored (last lost piece)
`handle_receipt` again parses the `recipient` attr (DM self-reads resolve to the real chat
instead of ourselves), the `t` attr (read watermarks use the phone's read time, not a local
wall clock that over-suppressed unread after a reconnect backlog), and the
`<list><item id=.../>` extension (every acked id updates, not just the first). Guard test
re-enabled.

### own-echo LID DMs could route into the wrong chat (regression the upgrade CREATED)
Upstream's v0.6.0 own-echo branch now sets `sender_alt` to OUR OWN identity (to warm the
LID-PN cache), but the surviving desktop consumer still assumed `sender_alt` was None there
and used it to resolve the peer's chat — so a LID-addressed self-echo resolved to our own
number.
- **wacore/src/messages.rs**: restore `peer_recipient_pn` -> `recipient_alt` (the peer's phone).
- **desktop/src/ui/runtime.rs**: `alt_phone` now branches on direction (recipient_alt for
  own-echoes, sender_alt for inbound) instead of `.or()`-ing them.

### guardrail
`desktop/tests/local_customizations.rs` asserts each load-bearing local protocol behavior
still exists. It lives in the desktop crate, which upstream does not ship, so a wholesale
protocol-core replacement cannot delete the test along with the code — the exact failure
mode that let this regression through CI. Verified to fail on genuine absence.

## Autocorrect: @mentions are untouchable

The inline autocorrector was overwriting `@name` while the user was mid-mention (the AI
pass fires after a 400 ms typing pause — exactly the pause of reading the mention popup —
and the local word-corrector treated `@An` as a typo). Four layers of protection:
- **desktop/src/ui/autocorrect.rs**: (1) the local word-corrector skips any token
  containing `@` (mentions + emails); (2) the AI pass doesn't even fire while the token at
  the cursor starts with `@` (mention in progress — re-arms on next keystroke); (3) HARD
  guarantee at apply time: if the corrected text does not preserve every `@token` from the
  live buffer verbatim, the entire correction is discarded; (4) the system prompt now
  instructs the model to reproduce `@mentions` character-for-character.
- Revert: `git revert <commit>`.

## Preview single-source-of-truth refactor (`FIXPLAN-PREVIEW-SSOT.md`)

The sidebar row was patched by 16 independent UI-side writers using 4 different clocks —
the log-proven bug: `bump_chat_to_top` (on MessageConfirmed) minted its own `now()` one
second ahead of the message's timestamp, so the follow-up preview update was discarded as
"older" and the row froze on stale text. Whack-a-mole ends here: rows are now a PURE
PROJECTION of the owning runtime's persisted summary. Baseline: tag **`pre-preview-ssot`**
(the contract commit immediately after it does not build alone; revert the whole feature
with `git reset --hard pre-preview-ssot`).

### Contract (bridge.rs)
`WaEvent::ChatRowChanged(ChatSummary)` — the ONLY event that writes row state; rendered
verbatim (no guards, no UI clocks). Single owner per id (`gm:` → gmessages runtime, rest →
WA runtime); producers guarantee monotonicity. `WaCommand::TouchChatSummary{..., ephemeral}`
— gm→WA route for merged-chat updates (retires the gm thread's behind-the-back
wa_chats.bin write); `ephemeral: true` renders without persisting (reaction previews).

### Producers (runtime.rs + gmessages_runtime.rs)
- `emit_row` choke points: upsert_chat (change-detected — no reseed floods; hardened: empty
  preview never clobbers non-empty, timestamp never moves backward), mark_chat_read_local,
  MarkUnread, new `set_chat_preview` (timestamp-preserving — edits/deletes no longer
  reorder), TouchChatSummary handler (monotonic + unread accounting + ephemeral).
- Coverage gaps CLOSED (froze rows otherwise): SendAudio now persists + echoes (voice notes
  were never persisted at all — pre-existing bug), MultiSend routes through
  persist_new_message, group system messages persist.
- Preview formatting moved producer-side (`row_preview`: "You:"/sender prefixes + mention
  resolution) — persisted previews finally match live rendering across restarts.
- Reactions are ephemeral overrides ("Reacted 👍" renders, never persists) from all three
  producers (send/incoming/gm).
- gm: send echoes + MarkRead/MarkUnread persist into gm_chats.bin AND emit; live SMS unread
  increment (active-conv + watermark gated) — badge now survives restart; reseed overlays
  newer live state before emitting; SetActiveChat fanned out and applied INLINE (no
  rapid-switch race); merged chats route via TouchChatSummary with merge_map redirect.

### Consumers (chat_list.rs + window.rs)
- `apply_summary` = the sole row writer; `note_recent_message` keeps stealth-peek +
  typing-overlay reset. ChatAdded is create-only (stale payloads ignored); ChatsLoaded
  applies verbatim.
- DELETED: update_last_message, bump_chat_to_top (the self-clocked racer), update_preview_
  text, clear_chat_messages, mark_chat_unread, typing_previews, optimistic click-clear,
  the ChatPreviewUpdated + ChatMarkedUnread events. `grep SystemTime::now chat_list.rs` → 0.
- Kept: reset_unread on ChatReadOnOtherDevice (gm badge clear), notifications/sound/2FA
  byte-identical.

Flagged follow-up (out of scope, same one-line fix as MultiSend): MultiForward still
pushes history without persist_new_message.

## Bulk LID↔phone population (fixes the 92%-unmapped root cause)

PL-A's sweep learned 0 because `get_user_devices` uses `DeviceListSpec`, which never
requests the `<lid/>` sidecar — and the deeper diagnosis found WhatsApp already PUSHES the
full lid↔phone table through three bulk channels that our code dropped on the floor
(fed display-name maps, never the protocol `lid_pn_cache`). Baseline: tag
**`pre-bulk-lid`** (commit `835c44e`).

### Packet BL-A — core bulk sources → lid_pn_cache
- **src/client/lid_pn.rs**: `learn_lid_pn(lid_user, phone_user)` public API (strips
  server/device suffixes; `MigrationSyncLatest` finally gets a production emit site).
- **src/client.rs** (dispatch_app_state_mutation): app-state `contact` mutations now write
  the cache — index[1] is the LID, `ContactAction.pn_jid` the phone; both were already
  decoded and discarded. Every contact the phone syncs now maps in bulk, zero extra IQs.
- **wacore/src/history_sync.rs + src/history_sync.rs**: decode the previously-discarded
  `HistorySync.phoneNumberToLidMappings` (field 15) table and persist every pair
  (`MigrationSyncOld`). Covers fresh pairings wholesale.
- Skipped (documented): live `LidMigrationMappingSyncMessage` handling — proto types exist
  but no decode path; a fresh protocol handler was out of scope.
- Revert: `git revert <BL-A commit>`.

### Packet BL-B/C — corrected resumable sweep + desktop consumers
- **src/features/contacts.rs**: `resolve_contact_lids(phones)` — executes
  `ContactInfoSpec` (the spec that actually requests `<lid/>`) and persists each returned
  lid via `add_lid_pn_mapping(Usync)`.
- **desktop/src/ui/runtime.rs**: `prewarm_contact_lids` rewritten — uses
  `resolve_contact_lids`; RESUMABLE via a persisted swept-set (`wa_lid_sweep_done.bin`) so
  it chips through the phonebook at 200 contacts/launch instead of timing out on 2000;
  contacts with an existing chat sort FIRST (active conversations heal early); failed
  chunks retry next launch; already-mapped contacts count as done.
- Desktop consumers that already hold (lid, pn) pairs — the ContactUpdate handler and the
  JoinedGroup lid+pn block — now also feed the core cache via `learn_lid_pn`.
- Revert: `git revert <BL-B/C commit>`.

## Proactive contact resolution — execution of `FIXPLAN-PROACTIVE-LID.md`

Every LID↔phone mapping write was reactive (needed a live message/notification), and the
desktop's LID resolvers queried usync with `@lid` JIDs — which usync never answers (it only
returns the `<lid>` child for phone-keyed queries). So a saved contact never messaged (or
messaged before LD-A) learned its mapping from nowhere → a phantom "+<lid digits>" chat.
Baseline: tag **`pre-proactive-lid`** (commit `a36f68d`).

### Packet PL-A — startup phone→LID prewarm sweep
- **src/client/lid_pn.rs**: added `get_lid_for_phone` (public phone→LID cache accessor).
- **desktop/src/contacts.rs**: added `saved_contact_phones` (real named contacts only) + test.
- **desktop/src/ui/runtime.rs**: on the post-connect background task (before the existing
  merge), `prewarm_contact_lids` usyncs saved contacts' PHONE numbers — the direction usync
  resolves — populating phone↔LID bidirectionally, then mirrors the results into the UI map
  and contact-directory `by_lid` so `merge_lid_chats` collapses matching phantoms the same
  launch. Guarded: once per session (`did_phone_lid_sweep`), skips already-mapped contacts,
  chunked (50) + 300ms throttle + 40-chunk cap, off the hot path.
- Effect: future LID messages for saved contacts resolve on arrival (no phantom); an existing
  phantom heals if its contact is in the phonebook AND its LID is still current (a rotated LID
  still needs a fresh message).
- Revert: `git revert <PL-A commit>`.

## LID phantom-chat fixes — execution of `FIXPLAN-LID.md`

Continuing a conversation from the phone with a saved contact created a duplicate
"+<lid digits>" chat instead of routing into the contact's existing chat, because the
peer's phone number (on the wire as `peer_recipient_pn`) was parsed and dropped. Baseline:
tag **`pre-lid-fixes`** (commit `3e222fd`). Undo whole pass: `git reset --hard pre-lid-fixes`.

### Packet LD-A — parse `peer_recipient_pn` + learn the mapping (core)
- **wacore/src/messages.rs** (own-echo DM branch): parse `peer_recipient_pn` (fallback
  `recipient_pn`) into `MessageSource.recipient_alt` — the whatsmeow-parity slot that was
  `None` at every construction site.
- **src/message.rs** (handle_incoming_message): when an own-echo DM's chat is a LID and
  `recipient_alt` is a phone JID, persist the peer LID→phone mapping to the core store
  (`add_lid_pn_mapping`, `PeerLidMessage`). This heals the EXISTING phantom at next startup
  (merge_lid_chats resolves via this cache) and prevents new ones. Test asserts
  `recipient_alt` is now populated.
- Revert: `git revert <LD-A commit>`.

### Packet LD-B — desktop routing from recipient_alt + resolver observability
- **desktop/src/ui/runtime.rs**: seed `lid_to_phone` from an own-echo DM's `recipient_alt`
  BEFORE chat-key resolution, so the echo routes straight into the contact's existing
  phone-keyed chat (no phantom, no usync). Extended `alt_phone` to fall back to
  `recipient_alt` (no-op on inbound/group paths). `resolve_lid_batch` now logs its outcome
  unconditionally (and warns on 0 resolved — the previously-silent LID→PN dead end).
- Revert: `git revert <LD-B commit>`.

### Packet LD-C — retroactive phantom merge (heals the existing "+<lid>" chat)
- **desktop/src/ui/runtime.rs**: extracted `merge_one_lid_chat` (folds message files with
  dedup-by-id + chronological sort, swaps the chat-list entry, deletes the @lid file) and
  made it emit `ChatDeleted` (drops the phantom row — `ChatsLoaded` never removes rows) +
  `ChatAdded` (refreshes the merged row). `merge_lid_chats` (startup) now falls back to the
  UI `lid_to_phone` map when the core cache misses, gated on the mapped phone chat already
  existing so a stale mapping can never merge into the wrong chat and delete the file.
- Heal path: after LD-A learns the mapping (from one more message to that contact), the next
  startup's `merge_lid_chats` folds the existing phantom into the contact's chat. New
  phantoms are prevented outright by LD-B. (Hot-path live-merge was intentionally left out
  to avoid a message-file write race — the off-path startup merge is sufficient and safe.)
- Revert: `git revert <LD-C commit>`.

### Packet LD-E — contact-directory as a merge source
- **desktop/src/contacts.rs**: added `resolve_lid_to_phone` (uses the explicit, non-fuzzy
  `by_lid` index).
- **desktop/src/ui/runtime.rs**: `merge_lid_chats` now consults the contact directory
  (trusted, ungated) between the core cache and the UI-map fallback; `merge_one_lid_chat`
  resolves the merged chat's name from the directory so a phantom with no prior phone chat
  still lands the contact's real name.
- Note: only heals phantoms whose LID was pinned to a real phone in the directory (via
  `record_lid_jid`). A phantom whose mapping exists NOWHERE on the desktop (core cache, UI
  map, and directory all miss) still needs the mapping learned from a fresh message (LD-A)
  before it can merge.
- Revert: `git revert <LD-E commit>`.

## Chat-list preview fixes — execution of `FIXPLAN-PREVIEW.md`

The sidebar preview tracked the latest *action* instead of the latest *message*: editing
didn't refresh it, and deleting an OLDER message hijacked it to "deleted" while the
bubble never updated. Baseline: tag **`pre-preview-fixes`** (commit `ed1ca43`). Undo whole
pass: `git reset --hard pre-preview-fixes`.

### Packet PV-B — delete/revoke: correct target + gated preview
The inbound revoke branch read `meta_info.target_id` (never populated), falling back to
the revoke stanza's OWN id — so the delete removed nothing (message resurrected on
restart) and the bubble never updated. The preview was also stamped "🚫 Message deleted"
unconditionally, regardless of whether the deleted message was the latest.
- **desktop/src/ui/runtime.rs** (revoke branch): resolve the original id from
  `ProtocolMessage{type=Revoke}.key.id` (via `msg.protocol_message` or `get_base_message`
  for wrapped echoes) — same nesting fix as the edit resolver. Load history from disk if
  the chat isn't in memory (removal now persists; also fixes never-opened chats). Compute
  `was_latest` and only rewrite the preview (row + persisted `last_message`) when the
  deleted message was the newest.
- The two desktop-initiated delete sites got the same `was_latest` treatment;
  **delete-for-me** recomputes the preview from the new latest *remaining* message (not a
  "deleted" stamp — nothing was revoked for the other party).
- **desktop/src/bridge.rs**: `MessageDeletedLocal` gained `new_preview: Option<String>`
  (the exact preview to show, or `None` to leave the sidebar untouched).
- **desktop/src/ui/window.rs**: gate the sidebar update on `new_preview`. Deleted the
  misleadingly-named `set_preview_to_previous` (chat_list.rs) — its only caller.
- Revert: `git revert <PV-B commit>`.

### Packet PV-A — edit refreshes the preview (only when latest)
The `MessageEdited` handler updated only the bubble; neither apply-site touched the
sidebar or the persisted `last_message`, so the row kept the pre-edit text (even after
restart).
- **desktop/src/bridge.rs**: `MessageEdited` gained `is_latest: bool`.
- **desktop/src/ui/runtime.rs** (both edit apply-sites — inbound + outbound Ok-path):
  compute `is_latest` from the timestamp-sorted history; when the edited message is the
  latest, also update the persisted `ChatSummary.last_message` (no timestamp/unread change
  — edits don't reorder). The inbound branch now loads history from disk if the chat isn't
  in memory, which also fixes a pre-existing bug where edits to evicted chats weren't
  persisted at all.
- **desktop/src/ui/window.rs**: refresh the sidebar preview (`update_preview_text`) only
  when `is_latest`.
- Revert: `git revert <PV-A commit>`.

### Packet PV-C — reactions stop hijacking the preview
A reaction on ANY message stamped "Reacted 👍" as the sidebar preview, even for an old
message.
- **desktop/src/bridge.rs**: `ReactionUpdated` gained `is_latest: bool`.
- **desktop/src/ui/runtime.rs** (both WA reaction sites) and
  **desktop/src/gmessages_runtime.rs** (SMS reaction site): compute `is_latest` from the
  chat's sorted history alongside the existing reaction persist.
- **desktop/src/ui/window.rs**: only set the "Reacted 👍" preview when `is_latest` (and it's
  an add, not a removal — an empty reaction set no longer stamps a preview).
- Revert: `git revert <PV-C commit>`.

## Message-edit fixes — execution of `FIXPLAN-EDIT.md`

Editing a sent WhatsApp message did nothing (no bubble update, no error, nothing on the
phone). Three independent bugs, each verified against HEAD. Baseline: tag
**`pre-edit-fixes`** (commit `15af073`). Undo whole pass: `git reset --hard pre-edit-fixes`.

### Packet EB-B — inbound edit resolver (peer edits + own echoes)
Inbound edits arrive wrapped a level deeper than the top-level `protocol_message`
(`Message.edited_message` → `.message.protocol_message`), so the handler read empty text
with a missing target for every peer edit, and dropped from_me edit echoes entirely.
- **desktop/src/ui/runtime.rs** (Event::Message edit branch): resolve the ProtocolMessage
  from BOTH nesting levels (`edit_pm`); detection, target id (original message id), and
  new_text now use it. `new_text` reads the wrapped `edited_message` (covers both
  `conversation` and `extendedTextMessage.text`). Added an empty-text guard so a failed
  extraction can never blank a bubble.
- Revert: `git revert <EB-B commit>`.

### Packet EB-A — tmp→real id remap (outbound edit + all bubble-menu actions)
Editing a just-sent message did nothing: the bubble's menu closures froze the optimistic
`tmp-…` id at creation time, so Edit sent `key.id="tmp-…"` on the wire — the server ACKed
it (no error) but the peer discarded an edit for an unknown id, and the local update
missed the re-keyed bubble. The same staleness silently mis-targeted star/pin/react/
delete/reply for freshly-sent messages.
- **desktop/src/ui/chat_view.rs**: new `id_remap` (tmp→real) on ChatViewInner, populated in
  `confirm_bubble`, cleared in `open_chat`. `show_message_menu` resolves the message id
  through it (fixes Edit + all other menu actions); the do_send edit branch resolves again
  at send time to cover the confirm-while-editing race.
- **desktop/src/ui/runtime.rs** (EditMessage handler): guard — if the id is still `tmp-…`
  (send not yet confirmed), emit `EditFailed` (bounces the text back to the composer)
  instead of putting an unmatchable key on the wire.
- With the real id supplied, the existing wire format and optimistic-update path work
  unchanged — no protocol change.
- Revert: `git revert <EB-A commit>`.

## Read-sync fixes — execution of `FIXPLAN-READSYNC.md`

Fixes the offline-unread flood, phone-notification persistence, and SMS double-bubble,
diagnosed by a verified multi-agent investigation (see FIXPLAN-READSYNC.md for the full
evidence chain). Baseline for this pass: tag **`pre-readsync-fixes`** (commit `060cf9d`).
Undo whole pass: `git reset --hard pre-readsync-fixes`. Each packet is a separate commit.

### Packet O — Own outbound message marks the chat read (biggest visible win)
Chats where the user's only offline-window activity was their own phone-sent reply (or
that they read + replied to on the phone) stayed unread on desktop forever, because an
own-outbound echo carried the stale unread count forward and floated it to the top of
the list.
- **desktop/src/ui/runtime.rs** (persist_new_message): after persisting the chat, an own
  new non-system message now calls `mark_chat_read_local` — zeroing the count POST-upsert
  (so the upsert stale-preserve heuristic can't restore it) and stamping the read
  watermark at the message timestamp so a later reseed can't resurrect the badge. Added
  `!is_system_message` to the unread-bump guard.
- **desktop/src/ui/chat_list.rs** (update_last_message): the badge increment is now gated
  on `!is_system_message`, and an own newer message clears the badge to 0 immediately so
  the UI matches the persisted state (previously only fixed at the next full reload). This
  also stops group-event system messages from bumping the badge.
- Revert: `git revert <O commit>`.

### Packet D — SMS double-bubble on desktop send (display-only)
Outbound SMS sent from desktop sometimes rendered twice (first single-tick optimistic,
second double-tick echo). The tmp→real reconcile in the longpoll path was dead code: it
emitted RAW un-prefixed ids while the bubble was keyed `gm:<tmp>` and the echo id was
`gm:<real>`, so both lookups missed and the echo appended a second bubble.
- **desktop/src/gmessages_runtime.rs** (translate_event): the `MessageConfirmed` fired
  from the longpoll echo now tags BOTH `tmp_id` and `real_id` with the `gm:` prefix
  (guarded by `starts_with`), so it re-keys the optimistic bubble to `gm:<real>` right
  before the echo arrives and the exact-id dedup hits.
- **desktop/src/ui/chat_view.rs** (confirm_bubble): hardening for the residual race — if
  the echo already rendered a bubble under `real_id`, drop the optimistic widget instead
  of stacking a second one.
- Display-only bug (disk always held one copy); revert: `git revert <D commit>`.

### Packet R + S — Full read receipts + order-independent self-read handling
**R (phone notification persistence):** reading a chat on desktop dismissed the phone's
chat-list unread but left the Android system notification stuck, because we sent a read
receipt for only ONE message id (the newest anchor). The phone clears notifications per
message id.
- **desktop/src/ui/runtime.rs** (MarkRead arm): now collects EVERY unread incoming id
  since the previous read watermark (history → disk fallback → last-incoming anchor),
  dedupes, caps at 100 newest, and sends them all in one receipt. Groups batch by sender
  (one receipt per participant, whatsmeow semantics) keeping the LID→phone retry ladder.
- **src/receipt.rs** (handle_receipt): inbound receipts now parse the `<list><item id/>`
  extension so a multi-id receipt from the phone updates tick state for every id, not
  just the first.

**S (offline self-read receipts race the message backlog):** during the offline flush a
self-read receipt (chat read on the phone) arrives before the messages it covers, so the
badge was re-created and never cleared. DM self-reads also resolved their chat to our own
JID.
- **src/receipt.rs**: parse the `recipient` attr and detect self-reads (from matches own
  pn/lid) → resolve the chat to `recipient` and set `is_from_me`; parse the `t` attr into
  the receipt timestamp (was local arrival time — mandatory so boot-time receipts don't
  over-suppress genuinely-unread messages).
- **desktop/src/ui/runtime.rs**: new in-memory `receipt_watermarks` map (SEPARATE from the
  durable read_watermarks to avoid same-second suppression of local reads); the ReadSelf
  arm stamps it at the receipt time; persist_new_message suppresses a live unread bump when
  the message is at-or-before the receipt watermark. Own phone/lid identity is now seeded
  before the event loop starts so a group self-read during the flush doesn't race an empty
  identity.
- Tests: `test_read_receipt_collects_list_item_ids_and_t` (multi-id + `t` parse).
- Revert: `git revert <R+S commit>`.

### Packet G — gm staleness hardening (optional)
- **desktop/src/gmessages_runtime.rs** (MarkRead arm): marking an SMS chat read now also
  clears `unread_count=0` in gm_chats.bin for the marked conv(s), so a boot that fails to
  reseed (e.g. a `list_conversations` rpc timeout) doesn't depend solely on the watermark
  clamp to hide an already-read badge.
- Revert: `git revert <G commit>`.

### Packet K — Make app-state key recovery reachable (root cause / phone→desktop read sync) — CRITICAL
The desktop held one stale app-state sync key while the phone had rotated to a newer one,
so every `regular_low` patch (which carries `markChatAsRead`) failed to decode. The
key-request machinery existed but was unreachable: `decode_patch_list` /
`decode_multi_patch_list` hard-failed (via the `get_keys` closures) with `?` BEFORE the
callers' `get_missing_key_ids` + `request_app_state_keys` block could run, so a key was
never requested and phone-side reads could never reach the desktop.
- **wacore/src/appstate_sync.rs** (process_patch_list): detect missing keys AFTER external
  blobs are attached (this is the choke point both decode paths funnel through) and return
  an EMPTY result with `has_more_patches=false` instead of hard-failing. The callers'
  existing request blocks (client.rs:2413, :2620) now fire. Also preserve the typed
  `AppStateError` at the two blocking-decode sites (`anyhow::Error::new`) for diagnostics.
- **src/message.rs** (handle_app_state_sync_key_share): now `self: &Arc<Self>`; notifies
  waiters on EVERY share (not just the first) and spawns a re-sync of the app-state
  collections when keys arrive, so the deferred reads apply promptly instead of waiting for
  the next server_sync notification.
- **src/client.rs** (request_app_state_keys): log the request at info level (app-state
  debug was invisible under the default `whatsapp_rust=warn` filter).
- **Needs a live-phone test** (protocol-level): boot log should show `missing app-state
  key(s)` → `Requesting … key(s)` → `key share: … stored` → resync; and
  `sqlite3 whatsapp.db "SELECT COUNT(*) FROM app_state_keys"` should grow past 1.
- Revert: `git revert <K commit>`.

**Pre-existing test note:** the whatsapp-rust lib suite has 4 failing tests unrelated to
this work — `bot::tests::test_bot_builder_with_{version,os,platform}_*` (assert a default
device-props version; `src/bot.rs` has zero diff from the `pre-readsync-fixes` baseline)
and `client::tests::test_fibonacci_backoff_max_900s` (a ±10% jitter timing assertion in
untouched code). These fail at baseline and should be triaged separately.

## Review-pass cleanup — fixes from `REVIEW-REPORT.md`

Independent second-pass review of the whole `pre-audit-fixes..HEAD` diff (6 parallel
reviewers) surfaced bugs the batches introduced or left. Fixed here as 7 file-partitioned
work packets executed in parallel, then compile-checked (root crate **and** the `whatsapp-desktop`
bin — the latter is *not* in `default-members`, so `cargo build --release` alone does not build it)
and regression-tested.

**Reversibility:** baseline for this pass is tag **`pre-cleanup`** (commit `57bc157`).
Undo the whole cleanup: `git reset --hard pre-cleanup`. Each packet is committed separately
below so a single packet can be `git revert`ed in isolation.

**Verification:** release build of both crates clean (no new warnings beyond the pre-existing
`send_remove_mutation`); new regression tests all green — `test_message_worker_reacquires_on_generation_bump_instead_of_dropping`,
`test_complete_offline_sync_widen_is_atomic_under_mutex` (S1/S2), and
`legacy_blob_decodes_and_migrates_without_wiping` + `current_format_round_trips_through_decode` +
`is_lid_row_skips_suffix_index_on_load` (C2/G1).

### Critical
- **C1 — Tray window no longer silently blue-ticks unseen messages.** `SetActiveChat{None}` is
  now sent on close-to-tray and re-sent with `Some(current)` on window refocus; the current-chat
  auto-mark-read is gated on window focus. Previously a chat left "active" while the window sat in
  the tray sent real read receipts (and mis-persisted unread counts) for messages never seen.
- **C2 — `contacts_directory.bin` no longer silently wiped on upgrade.** The new `name_priority`
  field broke bincode decode of old files; a legacy-layout fallback decoder now migrates existing
  directories, and an undecodable file is preserved as `.corrupt` instead of being overwritten empty.
- **C3 / C4 — Typing indicator no longer misfires.** Opening a chat with a saved draft (or restoring
  a failed edit / opening the event creator) no longer broadcasts a phantom "typing…"; switching
  chats or sending now flushes `SetTyping{false}` to the correct chat via a shared `cancel_typing()`
  helper, so a contact no longer stays stuck on "typing…" indefinitely.

### Major
- **S1 — Offline messages no longer lost on a mid-drain disconnect.** A worker whose semaphore
  generation was bumped by a reconnect during backlog drain now re-acquires on the new semaphore and
  processes the (already-acked, durable-session) message instead of silently dropping it.
- **S2 — Offline-sync completion CAS + permit widen are now atomic** under the semaphore mutex,
  closing the race that could leak 63 permits onto the next connection's semaphore and break ordering.
- **R1 — Live unread bumps no longer swallowed** by the reseed-defense watermark guard (now gated on
  a `from_reseed` flag), so a message arriving in the same second as a local mark-read is counted.
- **R2 — First message of a brand-new chat persists `unread=1`** when it arrives while away (was
  hardcoded 0), so a new contact's chat no longer shows already-read after restart.
- **W1 — "Log out / Unlink device" relabeled to "Disconnect this session"** with honest copy — it
  disconnects but does not unlink (a true remove-device is not yet implemented; see report for the
  runtime.rs work it needs).
- **B1 — `update_sender_name` renames the group sender label**, not the quoted-reply sender; refresh
  keeps its hashed colour + bold styling.
- **B2 — Re-downloading a deleted media file** no longer sticks on the download placeholder forever
  (`media_loaded` seeded from the exists-filtered path).
- **G2 — Failed-SMS Resend button actually re-sends** now (was a silent no-op).
- **G3 — Sending a contact card to an SMS chat** marks the bubble failed instead of hanging forever.
- **G4 — Longpoll HTTP non-success backoff capped at 60s** (matching the POST-failure branch).
- **G5 — Mark-as-unread on an SMS chat survives reseed** — runtime now fans `MarkUnread` to the gm
  runtime, which rolls its read watermark back.
- **G6 — The synthetic "Verification Codes" inbox can be marked read** — the watermark is fanned to
  the real underlying shortcode convs (bogus server ACK skipped) and those rows are suppressed on
  reseed so they stop reappearing unread.
- **V1 — No malformed `SetTyping` with a virtual `sendgroup::` id** while typing in a send group.
- **V2 — Event creator no longer hijacks composer state** (staged image, in-progress edit, or draft);
  event text is sent directly, bypassing the composer.
- **V3 — A failed message edit no longer clobbers text typed while the edit was in flight** — it only
  reclaims the composer when empty, otherwise surfaces the failed text via a toast.

### Minor (selected)
- Optimistic SMS reaction updates merge with the persisted set instead of wiping other participants'
  reactions; gm read-watermark and contacts directory now written atomically; `set_use_markup(true)`
  no longer defeats the plain-text markup fallback; sender-controlled URLs are http/https-allowlisted
  before `xdg-open`; typing throttle uses `glib::monotonic_time()`; `@Ann`/`@Anna` mention
  boundary fix; notification-open selects the sidebar row; brand-new chat row doesn't seed unread=1
  when you're viewing it; consecutive duplicate toasts suppressed; `SetActiveChat` applied inline
  (ordering); history-sync seeds the read-receipt anchor; event-creator dialog leak fixed;
  voice-note preview can replay; quick-reply Save disabled until non-empty.
- Removed the now-dead `widen_message_semaphore` (superseded by `try_complete_offline_sync_widen`).

**Deferred (in report, not applied):** a true device-unlink for W1 (needs a remove-device IQ +
session wipe + QR re-route); watermark single-writer channel; best-effort reactions for uncached
chats. See `REVIEW-REPORT.md` for the rationale on each.

---

## Batch 1 — Persistence hardening

Closes the silent data-loss / corruption holes so the later batches (which change persisted
structs) are safe to land. Findings: `non-atomic-writes-corrupt-on-crash`,
`gm-chats-headerless-no-fallback`, `gm-chats-cache-no-schema-migration`,
`message-file-invisible-on-schema-mismatch`, `serde-default-does-not-migrate-bincode` (prevention).

- **Atomic writes** — new `atomic_write()` (temp file → `sync_all` → `rename`) now backs `write_bin`,
  `write_bin_path`, and the gm chat-cache. A crash/power-loss mid-save can no longer truncate an
  aggregate file (empty sidebar / lost history / read→unread).
- **Never destroy on schema drift** — new `backup_corrupt_once()` preserves a present-but-undecodable
  `.bin` as `.corrupt` before anything can overwrite it. `load_messages` now backs up + shows empty
  on a decode failure instead of letting the next incoming message load→append→**overwrite** the old
  history.
- **gm chat cache** — new `gm_load_chats_cache` / `gm_save_chats_cache` centralize `gm_chats.bin`
  access: atomic write, backup-on-corrupt, and an **abort-on-empty guard** that refuses to overwrite a
  non-empty cache with an empty one (this turned a one-restart schema blip into *permanent* loss of
  every SMS-only chat).
- **Regression tripwire** — `bridge.rs` gains serde round-trip tests + a doc note requiring a
  `BIN_HEADER` bump + legacy decoder on any `ChatSummary`/`IncomingMessage` field change.

Deferred (bytes are preserved via `.corrupt`, so no data is lost — recovery is a safe follow-up):
full legacy-decoder *recovery* of pre-`media_download` message files.

## Batch 2 — Feedback / toast layer

Failures no longer vanish silently. Findings: F1/F2/GW-02 (ErrorToast swallowed), F5/GW-03
(detached multi-send toast), CL-03 (block no feedback), mb-03 (star no feedback).

- **App-wide `adw::ToastOverlay`** now wraps the main stack (`window.rs`); `WaEvent::ErrorToast`
  renders through it instead of a `log::warn!` + TODO. All ~13 runtime error emitters (calls,
  group ops, member edits, leave-group, phone-not-responding) are now visible.
- New **`WaEvent::InfoToast`** for neutral/positive confirmations, routed to the same overlay.
- **Multi-send** completion toast now uses the real overlay (was created on a detached
  `ToastOverlay::new()` that dropped immediately and never rendered).
- **Block contact** now emits `InfoToast("Contact blocked")` on success and `ErrorToast` on
  failure (was silent either way).
- **Star** action now shows a "Message starred/unstarred" toast (handler was empty `=> {}`).

## Batch 3 — Read/unread core (safe subset)

The flagship complaint. The **safe, isolated** wins are applied here; the interlocking
runtime-unread-store rework is **deferred** (see below) because it's the exact subsystem that has
repeatedly broken and needs active-chat plumbing + a live repro to land safely.

Applied:
- **read-ticks-gray-on-restart** — `MessageBubble::new` now paints the receipt tick from the
  persisted `receipt_status` at construction, so read (blue ✓✓) history renders correctly on
  restart / history load (was gray until a live receipt arrived).
- **read-receipt-uses-from-me-last-id / partial-read** — new `last_incoming_msg_id` map; the read
  receipt now anchors on the last **incoming** message, not `last_msg_id` (which includes our own
  sends and told the phone nothing → chat stayed unread on the phone). WhatsApp treats the receipt
  as a read-up-to watermark, so this also covers the partial-read case.
- **unread-badge-double-count** — the first message of a brand-new chat no longer counts to 2
  (`add_chat` already seeds 1); the live increment is now gated on `is_newer && !just_created`.
- **sort-tie-and-bump-time-stale** — equal-timestamp chats now tiebreak deterministically on
  chat_id instead of reshuffling between refreshes.

Deferred to a deliberate, repro-backed pass (all touch the fragile runtime unread store together):
`wa-live-unread-not-persisted` / `live-wa-unread-not-persisted` (needs active-chat plumbing so the
runtime, not the UI, owns the count — single source of truth), `wa-upsert-restores-stale-unread`
(phone-read authoritative-0 needs an "authoritative unread" signal plumbed through `upsert_chat`),
`seed-watermark-hides-unread-on-first-run` (depends on the persist-live-unread fix — seeding at
`ts-1` in isolation would REVERT the SMS reseed clamp), `mark-unread-not-persisted`,
`verification-inbox-markread-noop`. These are best done as one careful change with the app running.

## Batch 4 — SMS / gm parity

Findings: sms-reply-media-silent-drop, attachments-ignore-send-mode, gm-heic-blank-after-restart,
CL-02 (gm delete broken), gm-longpoll-unbounded-backoff.

- **SMS silent data-loss fixed** — a reply / voice note / GIF / sticker sent to an SMS chat no
  longer vanishes. `SendReply` to a gm chat is **downgraded to a plain text SMS** (the text still
  sends; SMS has no reply-quoting). GIF/sticker/voice (unsupported over SMS) now emit
  `WaEvent::MessageFailed` → the optimistic bubble flips to red ✗ + Resend instead of a stuck ⏳.
- **Attachments honor the send-mode toggle** — image/GIF/voice/sticker sends now route through
  `resolve_send_target` (6 sites) like text did, so on a merged chat they go over the chosen
  channel instead of always the raw open id (could have sent to the wrong person).
- **iPhone HEIC survives restart** — `message_to_incoming` now prefers the transcoded sibling `.jpg`
  for `image/heic`/`heif` instead of pointing at the unrenderable `.heic` (went blank on reload).
- **gm Delete works** — the WhatsApp `DeleteChat` handler no longer bails on a `gm:` id via `?`;
  local delete + `ChatDeleted` feedback now always run (WA-server delete only for a real JID).
- **gm long-poll backoff capped** at 60s (was unbounded linear growth during an outage).

Deferred (larger / riskier): gm media retry path, copy-outgoing-media-to-managed-dir,
gm phone-not-responding idle-timeout reconnect (delicate long-poll stream change),
SMS↔WA merge last-10 uniqueness (risky to the merge index), full gm-side delete persistence
(a deleted SMS chat may re-appear on restart until the gm cache removal lands), duplicate
runtime search cleanup.

## Batches 5 & 6 — per-chat state, states, notifications, media (high-value subset)

Applied:
- **Mute now suppresses notifications AND sound** (new `ChatListPanel::is_chat_muted`; gated in
  `window.rs`) — muting was purely cosmetic before. (mute-does-not-suppress-notifications /
  per-chat-mute-ignored)
- **2FA auto-copy gated to SMS** (`&& is_gm`) — a WhatsApp message containing "code"/"pin" no longer
  silently overwrites your clipboard. (2fa-autocopy-clobbers-clipboard)
- **No duplicate OTP notification** — an SMS OTP that fired the copy-OSD no longer also fires a
  normal banner + sound (`handled_as_2fa` gate). (duplicate-notification-for-otp-sms)
- **Notification click opens the chat** — new `app.open-chat` action; the banner now raises the
  window and opens the originating chat (was a dead click). (notification-click / GW-01)
- **Missing media file no longer a permanent blank** — the bubble checks the file actually exists on
  disk and falls through to the re-download placeholder instead of a blank box.
  (wa-missing-file-permanent-blank)
- **Filter-chip un-toggle veto** — clicking the active filter chip no longer leaves the list filtered
  with no chip highlighted. (CL-04)
- **Compose bar disabled with no chat selected** — typing/Send/attach no longer silently no-op on the
  "Select a chat" pane. (cv-03)

Deferred (medium/large or state-sync-sensitive — good follow-ups): star/label header buttons reflect
persisted state (F3/F4), disappearing-messages dropdown reflects setting (F6), transient-disconnect
offline banner instead of QR bounce (GW-04), empty-state placeholders (GW-05), startup connect
timeout + Retry (GW-06), drafts persistence, @lid duplicate-row dedup, media-download-failure
retry/MediaFailed + video error surfacing (download-failure-silent-stuck / mb-11), auto-download
size gate (unbounded-auto-download), hidden-window sound, secondary-path notification withdraw.

## Remaining work (batches 7–8, ~46 items) — NOT yet executed

Batches 7 (rendering/groups/names, 16 items) and 8 (polish/hardening, 30 items) are documented in
`AUDIT.md` but were **not** auto-applied in this run — they are mostly P2/P3 and better done as a
follow-up so this run stays reviewable and low-risk. See `AUDIT.md` Part B for the full list.

## Batch 7a — Read/unread CORE rework (the flagship, single source of truth)

The deferred core, now done deliberately. The runtime now OWNS the unread count.

- **Live WA unread is persisted** — new `WaCommand::SetActiveChat` tells the runtime which chat is
  open; `persist_new_message` bumps the persisted `unread_count` for a genuinely-new incoming message
  unless that chat is being viewed or is auto-mark-read. The count now survives restart instead of
  living only in the GTK badge Cell. (wa-live-unread-not-persisted / live-wa-unread-not-persisted)
- **Phone-read clears the desktop badge** — `upsert_chat` gained an `authoritative_unread` flag;
  history sync that carries an explicit `conv.unread_count` (a phone-side read arrives as Some(0)) is
  now trusted over the local stale-preserve heuristic, via `persist_chat_authoritative`.
  (wa-upsert-restores-stale-unread)
- **Seed-hides-unread fixed for free** — because genuinely-unread chats now carry `unread_count > 0`,
  the first-run watermark seeding (which only seeds `unread_count == 0` chats) no longer stamps them
  read. (seed-watermark-hides-unread-on-first-run)
- **Mark-as-unread persists** — the handler no longer bails on a `gm:` id; it sets `unread=1`, rolls
  the read watermark back below the last message (so a reseed/restart doesn't re-clamp it read), and
  persists. Routed to the WA runtime for any chat. (mark-unread-not-persisted)
- **Verification-inbox flash gone** — auto-mark-read chats are excluded from the unread bump.
  (verification-inbox-markread-noop)

⚠️ This is the subsystem that's broken before — please test: read on phone → desktop clears; read on
desktop → open chat, restart, stays read; unread chat → restart, stays unread; mark-unread → stays.

## Batch 7b — Approved decisions (from DECISIONS.md)

Per your direction: destructive-action confirmations = NO change; the rest applied.

- **Call buttons hidden** — the video/voice header buttons (a false affordance — click did nothing)
  are now hidden until WebRTC calling is implemented.
- **Log out / Unlink in settings** — new Account group under Behaviour with a destructive "Log out"
  row that sends `WaCommand::Logout` behind a styled `adw::AlertDialog` confirm (unlinking needs a
  fresh QR scan, so this one keeps a confirm).
- **Outbound "typing…" indicator** — now broadcast: `SetTyping{true}` (throttled to once / 3s) on
  keystrokes, auto `SetTyping{false}` after 4s idle, routed via the send-mode toggle.
- **Event creator built** — the attach-menu "Event" row now opens a composer (name / date / time /
  location), formats a tidy event message, and sends it through the normal send path (optimistic
  bubble + channel routing). Escape/Cancel dismiss.

## Batch 7c — Group event labels (subset)

- **Group rename applies live** — the `Subject` group-notification no longer hits the early-return
  catch-all; it now emits a "changed the group name to …" system message AND falls through to the
  metadata refresh that renames the chat. (group-subject-change-ignored)
- **"You" in group events** — your own account shows as "You" instead of your phone number in
  add/remove/promote/demote system messages. (self-not-shown-as-you-in-group-events)

## Remaining (batches 7–8) — documented, not yet applied

The rest of batches 7–8 (~40 items) are mostly P3 micro-polish (chevron hover, per-message
long-press menu, Escape-to-close on secondary dialogs, compose placeholder, star-toggle, live
reaction dedup, poll-vote merge, name-resolution heuristics, per-message perf under lock, etc.).
They are fully listed in `AUDIT.md` Part B and can be executed as a follow-up. The high-value,
user-visible work (persistence, feedback, read/unread core, SMS parity, notifications, groups/names,
and all four approved decisions) is done.

## Offline message loss — messages that arrived while the app was closed

Long-standing bug: messages a contact sent while the desktop was closed never appeared after
reopening (permanently missing, not just un-notified). Root cause (source-verified, corroborated by
two independent traces): a semaphore **generation-guard race** in the core message pipeline.

During offline sync the message-processing semaphore is set to permits=1 (serialized). Offline
`<message>` stanzas queue in per-chat workers, each capturing `generation = G` then blocking on the
permit. When the server's "offline complete" marker arrives, `complete_offline_sync` **swapped** the
semaphore Arc and bumped the generation to G+1 — *while the backlog was still draining*. Each
still-queued worker then saw `generation(G) != G+1` and `return`ed at `message.rs:608`, dropping its
message **before decryption** — no decrypt, no retry receipt, no event. Since the transport `<ack>`
was already sent, the server considered it delivered and dropped it from the offline queue →
permanent loss. (Reproduces only for the offline window; live + history-sync never hit a mid-flight
generation flip.)

Fix: `complete_offline_sync` now **widens** the existing semaphore (`add_permits`, 1→64) instead of
swapping the Arc + bumping the generation (new `Client::widen_message_semaphore`). In-flight offline
workers keep a valid permit and finish decoding. The generation guard still fires on a genuine
reconnect (which still uses `swap_message_semaphore`). Files: `src/client.rs`, `src/client/sessions.rs`.

To confirm from logs after an offline→reopen repro: previously each lost message logged
`"Semaphore generation changed during acquire, dropping stale permit"` (message.rs); that line should
now be absent for offline messages, and each `DIAG msg arrival ... offline` should be followed by a
`MSG routed` on the desktop.

## Batch 8 — remaining polish (40 of 44, via 7 parallel file-owner agents)

Implemented by a workflow with one agent per file (disjoint files → no clobbering); verified to compile
together (0 errors after one glue fix). Highlights:
- **New-chat button icon fixed on KDE** — was `chat-message-new-symbolic` (GNOME-only) rendering as a
  broken-image placeholder on Breeze; now a themed-icon fallback chain.
- Markup no longer corrupts bubbles (URLs with `_`/`*`/`~`) + `set_markup` plain-text fallback;
  poll voting merges instead of wiping other voters; quoted-reply thumb no longer scans all of
  wa_media; edited badge works on media-only messages.
- Quoted-reply tap-to-jump (mb-01); long-press message menu (mb-08); chevron hover/cursor (mb-09);
  inline audio playback (mb-10); unified hardened URL launcher (mb-12).
- Compose: send-button disabled when empty (cv-05); "Type a message" placeholder (cv-06); Audio attach
  + document filter (cv-02); inline voice-note preview (cv-08); Escape de-overloaded (cv-12); GIF
  loading feedback (cv-10); Save-Quick-Reply is now a styled adw::AlertDialog (mb-07/cv-09/F10).
- Star toggle (mb-04); reaction picker highlight+remove (mb-05); copy for captions (mb-06);
  phantom-mention-on-delete + word-safe @-substitution; gen_tmp_id collision fix.
- Names/groups: unresolved @mentions/@lid resolve instead of raw JID; group membership actor +
  kick-vs-left labels; history-sync groups fetch their subject; push_name won't overwrite phonebook;
  directory "longer name wins" + unmapped-@lid fuzzy-match tightened (data-integrity).
- Perf: per-message history no longer deep-cloned + resorted under the lock; MarkRead watermark write
  off the lock. Reliability: dual suspend detectors deduped; sync-spinner race fixed.
- Escape-to-close on secondary windows/profile panel (GW-07/GW-08); Share-Contact hidden on
  group/self (F8); group subject not re-sent unchanged (F12); gm context-menu optimistic feedback
  (CL-06); stealth-peek popover orphan guard (CL-07).

Skipped (4) — each genuinely needs a NEW WaEvent variant + cross-file wiring the parallel agents were
barred from adding; small follow-up: `live-reaction-duplicate-row-no-dedup`, `edit-failure-loses-text`
(the 15-min edit-window gate half WAS done), `group-participant-name-never-live-refreshes`, `F5`.

## Batch 8 completion — the last 4 skipped items

The 4 cross-file items (needed new WaEvent variants) are now done:
- **F5** — already fixed in Batch 2 (multi-send toast routed to the shared overlay); the agent had
  mis-scoped it. Verified.
- **live-reaction-duplicate-row-no-dedup** — factored the reaction row into a shared
  `build_reaction_row` (tagged `reaction-row`); `ReactionUpdated` now carries the message's FULL
  deduped reactions vec (both emit sites capture `m.reactions` and emit ALWAYS, incl. removals);
  new `MessageBubble::rebuild_reactions` removes the old row and rebuilds grouped pills. No more
  duplicate rows; reactions dedup/group/count and removals clear.
- **edit-failure-loses-text** — new `WaEvent::EditFailed{chat_id,msg_id,new_text}`, emitted from the
  runtime edit-failure path; `ChatViewPanel::restore_failed_edit` puts the text back in the composer
  and re-opens edit mode (the 15-min edit-window gate was already done).
- **group-participant-name-never-live-refreshes** — new `WaEvent::SenderNameResolved`, emitted from
  the usync member-resolution block; `MessageBubble` stores its group-sender label +
  `update_sender_name`; `ChatViewPanel::refresh_sender_name` updates open bubbles so a raw number
  becomes the real name without reopening the chat.

All 44 batch-7/8 items now complete.

## Fix — URLs stopped rendering as clickable links (Batch 8 regression)

Batch 8's `set_markup_safe` (added to stop malformed markup blanking a bubble) validated the markup
with `pango::parse_markup` before applying it. But `<a href>` is a **GtkLabel** link extension that
the raw Pango parser rejects (verified: `parse_markup("<a href…>")` → false, `<b>` → true). So every
message containing a URL failed validation and fell back to plain `set_text` — links rendered as
plain, unclickable text.

Fix: `set_markup_safe` now also accepts the markup if it validates with the `<a>`/`</a>` tags stripped
(new `strip_link_tags`) — real corruption is still caught, but valid links pass and `GtkLabel` renders
them. Also wired `connect_activate_link` on the message text label to open URLs through the app's
hardened launcher (`open_url`: setsid + detached) for consistent behaviour (mb-12).
