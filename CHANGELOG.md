# Changelog — audit fix batches

Automated execution of the audit fix plan (`AUDIT.md`). Judgment-call items are **not**
applied here — see `DECISIONS.md`.

**Reversibility**
- Baseline before any of these fixes: tag **`pre-audit-fixes`** (commit `54aef73`).
- Undo *everything*: `git reset --hard pre-audit-fixes`.
- Undo *one* batch: `git revert <that batch's commit>` (each batch is a single, self-contained, build-verified commit).

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
