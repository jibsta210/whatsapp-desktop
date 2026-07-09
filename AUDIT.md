# WhatsApp / Google-Messages Desktop — UX + Code Audit & Fix Plan

> **Status: review-only.** Nothing in this document has been changed in the codebase. It is a plan to be
> executed later. Two independent read-only audits were run as multi-agent workflows, each finding
> adversarially re-verified against the actual widget wiring / source before it was allowed into this plan.

## What was audited & how

| Audit | Lens | Coverage | Verified findings |
|---|---|---|---|
| **Part A — UI/UX interaction usability** | Hands-on: *does clicking/typing each thing actually work, with feedback and proper empty/offline/error states?* | 6 surfaces (headerbar/toolbar, chat list, message bubbles, compose bar, dialogs/settings/profile, global window states) — 13 agents | **58** |
| **Part B — Code / logic bugs** | Correctness: state machines, read/unread, sync-vs-mobile, persistence, media, groups, panics/races, perf | 12 dimensions — 25 agents | **68** |

Every entry carries `file:line` evidence and an effort tag (**S**/**M**/**L**). Findings that could be false
positives were rejected during a dedicated adversarial verification pass, so this list is high-signal.

## The five things that matter most (across both audits)

1. **Read/unread vs the mobile app** — the flagship complaint — is a five-link causal chain (two directions +
   a rendering bug), *not* one bug. Fixing links in isolation just shifts the symptom. See Part B's deep-dive.
2. **A P0 data-loss cluster:** `#[serde(default)]` does **not** migrate `bincode` files (reproduced) — any new
   field on `ChatSummary`/`IncomingMessage` can silently blank history or wipe the SMS list; `gm_chats.bin` then
   re-persists the empty vec, making it *permanent*; and all writes are non-atomic. **Recent watermark/media
   commits added fields — this risk is live and must be de-risked before any further schema change.**
3. **The SMS/gm runtime silently eats data and inputs** — one catch-all (`gmessages_runtime.rs:1868-1873`) drops
   replies, voice notes, GIFs, stickers, and most chat-list actions with no error, no Resend, no row update.
   This single root cause surfaces in *both* audits (silent data loss in B, dead controls in A).
4. **No error-surfacing layer** — `WaEvent::ErrorToast` is a no-op TODO and there is **no `ToastOverlay` in the
   widget tree at all**, so ~13 failure paths (calls, group ops, block, member edits, multi-send,
   phone-not-responding) vanish into the log. Adding one overlay unblocks feedback app-wide, and is the vehicle
   the logic-side error handling needs to become visible.
5. **Dead / lying controls & missing confirmations** — video/voice call, the "Event" attach row, notification
   click, and most SMS context-menu items do nothing; destructive actions (delete-for-everyone, leave group,
   remove participant, clear SMS cache) fire with **no confirmation**, despite a styled confirm dialog already
   shipping.

## Where the two audits overlap (fix once)

- **gm silent catch-all** — Part A "dead gm context-menu / compose" == Part B "SMS silently drops sends /
  inert chat-list actions." Same root (`gmessages_runtime.rs:1868-1873`).
- **Notification click does nothing** — Part A dead-control == Part B P2.
- **Silent media-download failure / no retry** — appears in both.
- **`gm:` Delete/Clear routing bug** — Part A flow-friction == Part B chat-list-actions.
- **Read ticks render gray on restart** — a Part B rendering bug that is also a Part A "lies about state" defect.

---

# Part A — UI/UX Interaction Usability

# WhatsApp Desktop — UI/UX Interaction Usability Fix Plan

_Hands-on interaction audit across headerbar/chat toolbar, chat list, message bubbles, compose bar, dialogs/settings/profile panels, and global window states. Every entry carries file:line evidence for the widget and its handler (or the absence of one)._

---

## 1. Executive summary

When you actually click around, the app looks complete but a surprising number of prominent controls are dead, silently no-op, or lie about state. The single most damaging defect is that **`WaEvent::ErrorToast` is a no-op TODO** and **no `ToastOverlay` is wired into the widget tree at all** — so failures from calls, group management, member edits, blocks, and multi-send all vanish into the log and the user never learns anything failed (`window.rs:1148-1151`). On top of that, several always-enabled header/menu controls do nothing visible — video/voice call buttons, the "Event" attach row, notification clicks, and most SMS/RCS (`gm:`) context-menu actions are silently dropped by the gmessages runtime. Destructive actions are inconsistently guarded: chat-list Block/Clear/Delete correctly reuse a styled `adw::AlertDialog`, but **message delete-for-everyone, leave-group, remove-participant, and clear-SMS-cache all fire with no confirmation**. Finally, state feedback is thin across the board: the favourite/label header buttons never reflect the open chat, the compose bar stays live with no chat selected, a transient network blip ejects the user to the QR screen, and there are no empty-state placeholders anywhere.

**Biggest themes:** (1) a missing toast/error-surfacing layer that starves the whole app of failure feedback; (2) dead/silent controls that create false affordances; (3) missing confirmations on destructive actions despite a reusable styled dialog already shipping; (4) header/menu buttons that ignore persisted per-chat state; (5) absent empty/offline/loading states.

**Confidence:** high. Nearly every finding was directly traced to the widget and its handler (or verified absence). A handful are edge cases (CL-07 orphaned hover popover) or medium-confidence timing gaps (GW-06 stalled-connect spinner) and are flagged as such.

---

## 2. Findings by kind

> Note on IDs: the raw findings reuse `F1…F12` across two audit surfaces. Below they're disambiguated; the ToastOverlay defect (F1/F2/GW-02) and the orphaned multi-send overlay (F5/GW-03) are each **one** underlying bug counted once.

### (a) Dead / broken controls — click does nothing or the wrong thing

| Element | Expected vs actual | Fix (concrete) · effort |
|---|---|---|
| **Video/voice call buttons** (chat header) — `chat_view.rs:208-217`, handlers `1011-1018`/`1024-1029` | Click should start a call or say why not. Actual: sends `InitiateCall`; runtime stub only logs + emits an `ErrorToast` that is swallowed (`runtime.rs:7266-7275`). Always enabled ⇒ false affordance. | Hide/disable until WebRTC lands, and surface the toast (depends on ToastOverlay). Optionally a styled `adw::AlertDialog`. · **S** |
| **Attach menu "Event" row** — `chat_view.rs:1707-1712`, closure `1733-1767` | Should open an event creator (Poll does). Actual: closure has no `"Event"` branch → popover just closes, no view/toast/log. | Add `else if label_str == "Event"` (or remove the entry / show styled "coming soon"). · **S** |
| **Quoted-reply snippet** in reply bubbles — `message_bubble.rs:198-325` | Tap should scroll to + highlight the original. Actual: plain `Box`, no gesture/cursor, completely inert; no scroll-to-message-by-id path exists. | Add `GestureClick`(btn 1) + pointer cursor; bubble callback to look up `quoted_msg_id`, drive vadjustment, flash highlight. · **M** |
| **SMS/RCS (`gm:`) context-menu actions** — Archive/Mute/Pin/Label/Favourite/Block/Mark-unread — `chat_list.rs:1122` (menu attached unconditionally) | Should perform the action + update the row. Actual: commands route to gmessages runtime which drops all but SendText/LoadChat/MarkRead/etc. at `gmessages_runtime.rs:1868-1873`. Row never updates. `SetAutoMarkRead` is _worse_ — flips the 👁 indicator locally (`chat_list.rs:1491`) so it looks toggled but nothing persists. | Pass `is_gm` flag into the menu builder; skip/disable unsupported items or implement them in gmessages. Stop the lying auto-mark-read flip for `gm:`. · **M** |
| **Notification click** — `send_desktop_notification` `window.rs:1805-1810` | Click should raise window + open the chat. Actual: notification has no `set_default_action`; `chat_id` discarded for routing; the registered `show-window` action (`window.rs:428-437`) is orphaned dead code. | Add `app.open-chat` action with `set_default_action_and_target_value(chat_id)`; handler presents window + opens chat. · **M** |
| **Logout / unlink** — `WaCommand::Logout` defined `bridge.rs:317`, handled `runtime.rs:4913` | User should be able to sign out. Actual: **zero** UI dispatch site; no Account/Logout row in settings. Capability unreachable. | Add an "Account" group in `settings.rs` with a destructive, confirmed "Log out / Unlink" row that sends `Logout`. · **M** |

### (b) Silent failures / missing feedback

| Element | Expected vs actual | Fix · effort |
|---|---|---|
| **`ErrorToast` sink** — `window.rs:1148-1151` (the keystone) | Should show an `adw::Toast`. Actual: bare `log::warn!` + TODO; **no `ToastOverlay` in the tree** (content is `ToolbarView` directly, `window.rs:255-259`). ~11 `ErrorToast` emitters in `runtime.rs` (4427/6920/6974/7027/7062/7097/7119/7134/7153/7270/7388) plus gmessages (`68`, `2335`) are all invisible: calls, edit, add/remove member, promote/demote, leave-group, phone-not-responding. | **Wrap `stack`/content in one `adw::ToastOverlay`, store on `MainWindowInner`, and `overlay.add_toast(...)` in the arm.** This one fix restores feedback app-wide. · **M** |
| **Multi-send completion toast** — `window.rs:1282-1291` | Should show "Sent to N chats, M failed". Actual: builds a **fresh throwaway** `adw::ToastOverlay::new()` never parented into the tree (drops immediately); even finds the real `ToolbarView` and discards it. Toast never renders. | Reuse the shared overlay from the fix above. · **S** |
| **Block contact (WA)** — `chat_list.rs:1554-1572`, handler `runtime.rs:5040-5045` | Should confirm the block took effect / surface failure. Actual: no `ChatBlocked` event exists anywhere; success emits nothing, failure only `log::warn!`. UI unchanged either way. | Add `WaEvent::ChatBlocked{ok}`; handle in `window.rs` with toast/row update, mirroring archive/mute echo. · **M** |
| **Star menu action** — `chat_view.rs:4636-4647`, echo `window.rs:1130` | Star should show a glyph. Actual: sends command; **`WaEvent::MessageStarred` handler is literally empty (`=> {}`)** — no feedback ever, even on echo. | Populate the `MessageStarred` handler + add optimistic star glyph. · **M** |
| **Menu actions lack optimistic feedback** — delete-for-me/pin/mute/archive — `chat_view.rs:4744-4754`, `chat_list.rs:1406-1457` | Instant visual feedback (WhatsApp is optimistic). Actual: only send+popdown; row/bubble updates on server echo. On slow/offline it feels ignored. Delete-for-me _does_ tombstone but only via round-trip (`window.rs:1134`→`chat_view.rs:3253`). Auto-mark-read (`chat_list.rs:1491`) proves the optimistic pattern is available. | Apply local state change on click, reconcile on echo. · **S** |
| **Video inline play errors** — `message_bubble.rs:1489-1512` | Missing/corrupt file should error/spinner. Actual: `MediaFile::play()` with no error/prepared signals; overlay hidden unconditionally → black box, no message. | Wire error/prepared signals; keep ▶/spinner until playback starts; error label on failure. · **M** |
| **GIF/sticker cell clicks** — `chat_view.rs:3445-3474` / `3519-3537` | Spinner/placeholder while preview downloads; optimistic sticker bubble. Actual: blank preview bar during background download; sticker fire-and-forget, no bubble. | Spinner in preview bar; optimistic sticker bubble. · **M** |
| **Voice-note "Preview"** — `chat_view.rs:1086-1090` | Play inline or confirm. Actual: `xdg-open` with discarded `Result`; silent if no handler; steals focus to external app. | Play via in-app audio path; toast on spawn failure. · **M** |

### (c) Missing confirmations on destructive actions

_A styled `adw::AlertDialog` helper already ships — `show_confirm_dialog` at `chat_list.rs:1697-1728` (Destructive appearance, close-response=cancel). These paths bypass it._

| Element | Expected vs actual | Fix · effort |
|---|---|---|
| **Delete for everyone / Delete for me** (message menu) — `chat_view.rs:4744-4754` / `4763-4771` | Irreversible ⇒ confirm. Actual: fire immediately + popdown, no dialog, no undo. A single misclick unsends permanently. | Generalize `show_confirm_dialog` to accept `&impl IsA<Widget>`; only send from `on_confirm`. · **S** |
| **Leave group** — `profile_panel.rs:428-434` | Consequential ⇒ confirm + feedback. Actual: sends `LeaveGroup` directly; no confirm; success silent, failure swallowed. | Route through `show_confirm_dialog` (Destructive); toast/close panel on success. · **S** |
| **Remove from group** (member popover) — `profile_panel.rs:1125-1131` | Visible-to-others destructive ⇒ confirm. Actual: sends `RemoveGroupParticipant` immediately, no undo/signal. | Styled confirm "Remove &lt;name&gt;?" before sending. · **S** |
| **Clear SMS cache** — `settings.rs:759-771` | Wipes cached history ⇒ confirm + result. Actual: deletes `gm_*` files + `gm_chats.bin` on click; no confirm, no post-toast. | Confirm via `adw::AlertDialog`; toast "Cleared N conversations". · **S** |
| **Delete send-group** (broadcast list) — `window.rs:2354-2361` | Config loss ⇒ confirm + live refresh. Actual: `remove`+`save` immediately; row stays until panel reopen. | Confirm; call rebuild closure so the row disappears. · **S** |
| **Multi-file drag-drop** — `chat_view.rs:1647-1666` | Stage/review before send (single-file drop stages one). Actual: loops `SendImage` for every file instantly, no preview/bubbles; mis-drop unrecoverable + invisible. | Stage into a reviewable queue or styled confirm modal listing N files; add optimistic bubbles. · **M** |

### (d) Missing / poor states — empty / offline / loading / error

| Element | Expected vs actual | Fix · effort |
|---|---|---|
| **Star button ignores persisted state** — `chat_view.rs:191-194`, handler `906-923` | Star reflects the chat's real favourite state; one click toggles correctly. Actual: `fav_button` is a local (not in `ChatViewInner`), never reset on `open_chat`; handler derives state from the icon (`is_fav = icon=="starred-symbolic"`, line 909) so on a non-favourite chat the **first click is a no-op un-favourite** and the icon goes stale across chats. `send_mode_btn` (`chat_view.rs:67`, re-synced via `apply_send_mode_btn`) is the correct contrast. | Store in `ChatViewInner`; set icon/tooltip from real `is_favorite` in `open_chat`; track true state; re-sync on `ChatFavorited`. · **M** |
| **Label button never reflects current label** — `chat_view.rs:197-200`, handler `972-987` | Button shows the applied label; popover marks active. Actual: local var, never re-init on open; only the tooltip changes, and the `None` branch skips even that (guard at `980`) ⇒ stale "Label: X" after clearing. | Store in `ChatViewInner`; set from persisted label on open; mark active row; clear (not skip) tooltip on None. · **M** |
| **Filter chip toggle-off** — `chat_list.rs:107-119`, `1742-1752`, chips `1732-1740` | Radio semantics: exactly one chip always active. Actual: bare `ToggleButton`s, no group; both radio loop and `wire_filter_chip` guarded by `if b.is_active()` ⇒ clicking the active chip flips it off, list stays filtered with **no chip highlighted** (unrecoverable-looking). | In `connect_toggled`, veto un-toggle: if would-deactivate and no other active, `set_active(true)` (or route to All). · **S** |
| **Compose bar live with no chat** — `chat_view.rs:1877/1911`, `do_send` `1991-1995`, `open_chat` `2422-2481` | On "Select a chat", compose should be disabled/hidden. Actual: `input_bar` never gated on `current_chat_id`; typing/Send/emoji/attach all no-op silently (emoji even fires `SearchGifs`, attach stages a file with no chat). | `input_bar.set_sensitive(false)` while `current_chat_id` is None; re-enable in `open_chat`; guard emoji/attach/mic. · **S** |
| **Transient disconnect → QR screen** — `runtime.rs:2054-2056`, `window.rs:568-574` | WiFi blip shows an offline banner, keeps the chat, auto-recovers. Actual: `Event::Disconnected` and `Event::LoggedOut` collapse to the same `WaEvent::Disconnected`; any drop forces `stack→"login"`, losing place. No offline banner exists; `sync_revealer` left pulsing until the 60s timeout. | Split transient vs LoggedOut in runtime; keep main view + "Connecting…" banner for transient; hide `sync_revealer` in the arm. · **M** |
| **No empty states** — `window.rs:575-577`, `chat_list.rs:129-137`, `chat_view.rs:165/233` | "No chats yet" / welcome pane / "No results". Actual: `StatusPage` appears **nowhere** in `desktop/src/ui`; empty account = blank sidebar, no chat selected = blank pane, empty filter/search = silently empty list. | Add `adw::StatusPage` placeholders for empty list, empty filter/search, and no-chat-selected pane. · **M** |
| **Disappearing-messages dropdown never reflects state** — `profile_panel.rs:180-181`, `404-423` | Shows the group's real timer. Actual: `from_strings([...])` with **no `set_selected`** anywhere and no ephemeral-duration read path ⇒ always shows "Off". | On group data arrival, map duration→index + `set_selected` while blocking the notify handler. · **M** |
| **Startup stalled-connect spinner** — `login.rs:33/42-44/84-97` | Connection failure at launch → error + Retry. Actual: subtitle "Connecting…" set once, never mutated; spinner spins forever on a silent/stalled connect; no timeout, no Retry. (A connect that _emits_ Disconnected does update `status_label` — medium confidence on the pure-stall case.) | Startup timeout → error state + Retry button; add Retry even to the Disconnected status. · **M** |
| **Accept incoming call is a no-op** — dialog `window.rs:2014-2044`, handlers `runtime.rs:7276-7284` | Accept joins the call. Actual: dialog is correctly styled, but `AcceptCall`/`RejectCall`/`EndCall` are log-only stubs; Accept closes the dialog then nothing happens. | Until WebRTC lands, don't surface the dialog or show "not supported" toast; else implement accept. · **L** |
| **Stealth-peek hover popover orphan** — `chat_list.rs:1204`, `1184-1192` | Reliably dismisses. Actual: `set_autohide(false)` + only `connect_leave` dismisses; if leave never fires (scroll-away while hovered, pointer grab) it persists with no Escape/outside-click path. Low-probability edge case. | Add Escape controller / re-enable autohide / safety timeout. · **S** |

### (e) Affordance & keyboard / focus

| Element | Expected vs actual | Fix · effort |
|---|---|---|
| **Escape doesn't close message-search bar** — `chat_view.rs:1264-1267` | `stop-search`/Escape closes + clears. Actual: only `connect_search_changed`; no `connect_stop_search`; dismiss only via toolbar button. | Add `connect_stop_search` → hide revealer, clear text, `apply_search_filter("")`. · **S** |
| **No long-press/touch/keyboard path to message menu** — `chat_view.rs:2846-2858` | Long-press (touch) + keyboard open the menu. Actual: only `GestureClick(3)` + faint chevron; only `GestureLongPress` in the module is on `mic_btn` (`1228`). | Add `GestureLongPress` on the bubble → `show_message_menu`; optional Menu/Shift+F10. · **S** |
| **Chevron "More" — faint, no pointer, no hover** — `message_bubble.rs:852-864` | Discoverable: pointer cursor + hover emphasis. Actual: `set_opacity(0.5)` always, no `set_cursor_from_name`; hover motion controller (`995-1013`) only toggles `hover_actions`, never raises chevron. | Raise opacity on hover in existing enter/leave; set pointer cursor. · **S** |
| **Copy missing for media captions** — `chat_view.rs:4512-4523` | Any readable text (incl. captions) offers Copy. Actual: gated on `msg.text` only; caption in `media_caption` gets no Copy (bubble already stores the fallback at `message_bubble.rs:1019`). | Guard on `msg.text.or(media_caption)`. · **S** |
| **No compose placeholder** — `chat_view.rs:432-444` | "Type a message" hint. Actual: bare `TextView`, no placeholder overlay. | Overlay dim label that hides on focus/non-empty. · **S** |
| **Escape over compose doesn't close emoji popover; overloaded to cancel attachment** — `chat_view.rs:1304-1312`, emoji popover `594-598` | Escape closes whatever popover is open, no side effects. Actual: Escape handled only in mention/slash branch, and that branch _also_ clears `pending_image`/`pending_gif` + hides preview bar; emoji popover relies on default autohide. | Also popdown emoji popover on Escape; separate "cancel attachment" from popover dismissal. · **S** |
| **Send button never disabled** — `chat_view.rs:455-461`, `do_send` `2060-2066`, buffer `1416-1540` | Empty field ⇒ disabled send (or mic). Actual: always suggested-action; never `set_sensitive`; false affordance. (Not a pure no-op — staged image/GIF sends before the empty-text guard.) | In `connect_changed`, `set_sensitive(!empty || attachment)`; optional mic swap. · **S** |
| **Profile side panel — no Escape, no real close button** — `window.rs:343-355`, `profile_panel.rs:65-68` | Escape closes; visible close control. Actual: no Escape/ShortcutController anywhere in `profile_panel.rs`; comment says "close button" but both title-button sets are disabled and **no close `Button` is packed**; only header re-click / chat-switch close it. | Add Escape controller → `set_reveal_child(false)`; add a real `window-close-symbolic` button. · **S** |
| **Attach menu missing Audio/GIF/Sticker; Document chooser has no filter** — `chat_view.rs:1707-1712`, `1737-1748` | Full documented media set, each with its picker/filter. Actual: only Photo&Video/Document/Poll/Event; no audio-file attach; Document adds no `FileFilter`; everything staged sends `SendImage` (`2022/2035`) — no `SendDocument`/`SendVideo` from the UI. | Add Audio (→`SendAudio`); surface GIF/Sticker; give Document a non-image filter. · **M** |
| **"Share Contact" shown on groups/self** — `profile_panel.rs:119/123`, `open()` `639-741` | Only meaningful for individual contacts. Actual: appended unconditionally, `open()` never toggles its visibility; on a group/self the handler builds a broken vCard from subtitle text. | `set_visible(false)` for `"self"` and `*@g.us`. · **S** |

### (f) Flow friction & inconsistency

| Element | Expected vs actual | Fix · effort |
|---|---|---|
| **"Save Quick Reply" dialog** — `chat_view.rs:4682-4718` | Styled modal, Save + Cancel, Escape/Enter, transient to window. Actual: bare `gtk4::Window`, single Save, **no Cancel, no Escape controller, no `connect_activate`, no `transient_for`**; empty shortcut silently early-returns; no save confirmation. Violates the standing styled-dialog rule. | Rebuild as `adw::AlertDialog` with Cancel+Save, close-response=cancel, `connect_activate`→save, inline validation, toast on save. · **M** |
| **Own-profile & global-search windows** — `window.rs:1495-1500`, `1944-1950` | libadwaita-styled, Escape-to-close, transient. Actual: raw `gtk4::Window`; own-profile has **no `transient_for`** (Wayland mis-parenting) and no Escape; global-search has no adw HeaderBar and no Escape. (`show_multi_send_window` at `2050` is `#[allow(dead_code)]`, zero callers — cleanup only, not user-facing.) | Convert to `adw::Window`/`adw::Dialog` + HeaderBar; add `transient_for` + Escape ShortcutController. Delete dead multi-send window. · **M** |
| **Delete vs Clear asymmetry on `gm:` chats** — `runtime.rs:1495-1522`, `5062-5063`, `5047-5060` | Both route through the owning (gmessages) runtime. Actual: neither `DeleteChat` nor `ClearChat` is in `command_chat_id` ⇒ both go to the WA runtime for `gm:` ids. **Delete** parses a JID (`5063`), the `gm:...` string fails, `?` aborts → confirm dialog shown, user confirms, **nothing deleted, zero feedback** (CL-02, high). **Clear** doesn't parse a JID so it runs against the _wrong_ (WA) store and emits `ChatCleared` (CL-05, low). | Add both to `command_chat_id` so `gm:` routes to gmessages; implement clear/delete there; guard the WA delete handler to emit a visible failure instead of swallowing the parse `?`. · **M** |
| **Star is one-way** — `chat_view.rs:4636-4647` | Toggle to Unstar (backend supports it, `runtime.rs:5164-5173`). Actual: always sends `starred:true`; no Unstar label/path ⇒ irreversible from this menu. | Track per-message starred; render "Unstar"→`starred:false`, mirroring the `is_failed`→Resend swap at `4359`. · **S** |
| **Reaction picker can't remove/highlight** — `chat_view.rs:5263-5284` | Highlight current reaction; tap-again clears (empty reaction). Actual: fixed 6 emojis, each unconditionally `SendReaction`; no clear/toggle/selection. | Pass current reaction in; render selected; tapping it sends empty emoji to clear. · **S** |
| **Audio "play" is external, fake waveform** — `message_bubble.rs:1667-1678` | Inline play/pause/seek. Actual: play-triangle icon shells to `open_with_xdg` (copies to Downloads, external app); "waveform" is a hard-coded `▬▬▬` Label. Tooltip "Open audio player" partly mitigates. | Use `gtk4::MediaFile` (as `build_video_widget` does), or at least swap the icon to "open externally". · **M** |
| **Three inconsistent URL-open paths** — `message_bubble.rs:812-816` / `2111-2134` / `1980` | One consistent sandbox-safe launcher. Actual: link-preview uses bare `xdg-open` (no setsid/null-stdio hardening); files use hardened `open_with_xdg`; inline `<a href>` links have **no `connect_activate_link`** → GTK default `gtk_show_uri`. | Route all three through the hardened launcher. · **S** |
| **Outgoing "typing…" never broadcast** — `chat_view.rs:1416-1540` | Peers see typing (SetTyping exists). Actual: `WaCommand::SetTyping` is **defined + handled but never sent from any UI code** (grep: only def/handler/filter sites). | Debounced `SetTyping{true}` on buffer change + idle `false`. · **M** |
| **Group subject re-sent on every focus-out** — `profile_panel.rs:378-401` | Save only when changed + acknowledge. Actual: `save_name` sends `SetGroupSubject` on any non-empty text, no dirty-check, wired to both `connect_activate` and focus-leave. | Track last-loaded subject; send only if changed; checkmark/toast on success. · **S** |
| **Mic button triple-wires gestures (race)** — `chat_view.rs:1228-1258` | Mutually-exclusive hold-to-talk vs click-to-toggle. Actual: `GestureLongPress`, `GestureClick(1) released`, and `connect_clicked` all on `mic_btn`; a quick tap can start (via clicked) then immediately stop+review (via released) → non-deterministic, 1-second review bar for a zero-length note. | Drive from ONE model: either hold-to-talk (long-press start, gesture-end stop) OR click-to-toggle; drop the overlapping pair. · **M** |

---

## 3. Quick wins (small effort, high impact — do first)

1. **Add the `ToastOverlay`** (`window.rs:1148-1151`, wrap content at `255-259`). Medium-ish but unlocks feedback for _dozens_ of currently-silent paths (calls, group ops, block, multi-send). Highest leverage in the codebase — treat as the prerequisite for everything in kind (b).
2. **Fix the multi-send toast** to reuse that overlay — `window.rs:1282-1291`. **S**
3. **Confirm the destructive one-liners** by reusing `show_confirm_dialog`: leave-group (`profile_panel.rs:428`), remove-participant (`1125`), clear-SMS-cache (`settings.rs:759`), message delete-for-everyone/me (`chat_view.rs:4744/4763`), delete-send-group (`window.rs:2354`). Each is **S**.
4. **Filter-chip un-toggle veto** — `chat_list.rs:107-119` / `1742-1752`. Removes a real reachable dead state. **S**
5. **Disable compose bar with no chat selected** — `chat_view.rs open_chat` + `input_bar`. Kills the biggest first-run "dead app" impression. **S**
6. **"Event" attach row** — add a branch or remove the entry — `chat_view.rs:1733-1767`. **S**
7. **Escape closes search bar** (`connect_stop_search`, `chat_view.rs:1264`) and **Escape closes profile panel** (`profile_panel.rs`). **S** each.
8. **Copy for captions** (`chat_view.rs:4512`) + **Unstar toggle** (`chat_view.rs:4636`) + **populate the empty `MessageStarred` handler** (`window.rs:1130`). **S** each.

---

## 4. Recommended execution order (impact/risk ordered)

**Batch 1 — Feedback foundation (unblocks the most).**
Add the single persistent `adw::ToastOverlay` (`window.rs`), render `ErrorToast`, and reuse it for multi-send. Populate the empty `MessageStarred` handler. _Everything in kind (b) depends on this; do it first._ Risk: low, contained to `window.rs`.

**Batch 2 — Destructive-action confirmations (safety, all reuse an existing helper).**
Generalize `show_confirm_dialog` to accept any widget/root, then gate: message delete-for-everyone/for-me, leave-group, remove-participant, clear-SMS-cache, delete-send-group. Add live refresh where noted. Risk: low; pattern already proven.

**Batch 3 — Dead / false-affordance controls.**
Call buttons (hide/disable + toast), attach "Event" branch, notification click routing (`app.open-chat`), Logout row in settings, `gm:` context-menu gating (skip/disable unsupported + stop the lying auto-mark-read flip), and the Delete/Clear `command_chat_id` routing fix for `gm:` chats. Risk: medium (touches gmessages routing) — keep the routing fix reviewable in isolation.

**Batch 4 — Per-chat state correctness.**
Store `fav_button` + `label_button` in `ChatViewInner` and re-sync on `open_chat`/echo; fix the disappearing-messages dropdown `set_selected`; "Share Contact" visibility. Risk: medium (state-sync races) — mirror the existing `apply_send_mode_btn` pattern.

**Batch 5 — Missing states.**
`adw::StatusPage` empty states (list / filter-search / no-chat pane), transient-disconnect offline banner (split `Disconnected` vs `LoggedOut`), startup connect timeout + Retry. Risk: medium; the disconnect split touches runtime event mapping.

**Batch 6 — Affordance, keyboard, and dialog consistency polish.**
Quoted-reply jump-to-original, long-press/keyboard message menu, chevron hover/cursor, compose placeholder, send-button sensitivity, Escape-over-emoji de-overloading, Save-Quick-Reply + own-profile + global-search dialogs → adw styling/transient/Escape (and delete the dead multi-send window), optimistic feedback for pin/mute/archive, GIF/video/sticker loading + error states.

**Batch 7 — Deeper interaction rework (larger/riskier, schedule separately).**
Mic-button gesture consolidation, outgoing `SetTyping`, multi-file drag-drop staging, inline audio playback (`MediaFile`), unified hardened URL launcher, reaction add/remove/highlight. WebRTC-blocked items (real call initiation + accept) are **L** and gated on the protocol library — until then, keep them behind the Batch 3 hide/disable + toast.

---

### Confidence & scope notes
- The ToastOverlay defect is reported three times (F1/F2 and GW-02, plus the orphaned overlay as F5/GW-03) — it is **one** root cause; fixing Batch 1 clears all four.
- Edge/medium-confidence items explicitly flagged: CL-07 (orphaned hover popover, needs an unusual event sequence), GW-06 (perpetual spinner only on a truly silent/stalled connect), mb-11 (black-frame is a partial cue).
- Two items are cleanup, not user-facing bugs: `show_multi_send_window` (`window.rs:2050`, dead code) and the orphaned `show-window` action (`window.rs:428-437`).
- No finding was invented beyond the verified evidence; every row above traces to a cited widget and its handler (or a verified absence).

Key evidence files: `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/window.rs`, `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/chat_view.rs`, `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/chat_list.rs`, `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/message_bubble.rs`, `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/profile_panel.rs`, `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/settings.rs`, `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/ui/login.rs`, and the runtime/bridge at `desktop/src/runtime.rs`, `desktop/src/gmessages_runtime.rs`, `desktop/src/bridge.rs`.

---

# Part B — Code / Logic Bugs

# WhatsApp/Google-Messages Desktop — Audit Fix-Plan

## Executive summary

The app is functionally rich but structurally fragile in three areas that a user feels daily: **read/unread accounting, on-disk persistence, and cross-protocol (SMS↔WhatsApp) parity**. Two independent representations of "unread" (a persisted scalar in `wa_chats.bin`/`gm_chats.bin` versus a UI-only GTK `Cell` on each row) drift apart the entire time the app runs and are silently reconciled only by a server reseed — the root of the user's flagship complaint that **"read vs unread is inconsistent vs the mobile app."** Compounding that, the persistence layer relies on `#[serde(default)]` to migrate `bincode` 1.x files (which it provably cannot for structs inside a `Vec`), so any schema change can blank a chat's history or wipe every SMS-only chat, and all writes are non-atomic (a crash mid-save corrupts the whole file). The SMS runtime silently drops most compose actions (replies, voice notes, GIFs, stickers) and most chat-list actions (pin/mute/archive/delete) with no failure feedback. There are also several genuinely-shipping bugs — read ticks render gray on every restart, live reactions duplicate, mute never suppresses notifications, and 2FA auto-copy clobbers the clipboard on ordinary WhatsApp messages. The good news: most fixes are small and localized, the data model already carries the signals needed (read watermarks, `media_download` keys, `own_lid`/`own_phone`), and the highest-impact work clusters into a few coherent batches.

**Confidence:** the read/unread chain and the persistence findings were reproduced or traced end-to-end and are high-confidence. A few items (linked-device app-state range, the merged-SMS clobber under real timing) would benefit from a runtime/log repro before coding — flagged below.

---

## Cross-cutting themes

These recur across dimensions and should be understood before fixing individual entries:

1. **Two unread representations.** Persisted scalar (`unread_count` in `ChatSummary`) vs a UI-only `Rc<Cell<u32>>` on the chat-list row. The row is incremented on receive; the store never is. On cold start the badge reseeds from the stale scalar. (`chat_list.rs:733-736` vs `runtime.rs:3901-3916`)
2. **Reseed overwrites local state.** `upsert_chat` (`runtime.rs:1205-1300`) unconditionally flushes the whole in-memory `self.chats` vector on ~20 call sites, and a heuristic at `runtime.rs:1278-1280` restores stale local unread over the server's authoritative `0`. Any writer that isn't routed through `RuntimeState` (e.g. the gmessages thread's direct RMW of `wa_chats.bin`) gets clobbered.
3. **Late name resolution is never refreshed live.** Message bubbles cache the sender name at build time; usync/PushName/Contact updates deliberately skip re-rendering the open chat (GTK4 GL paint workaround) and there is no per-bubble name setter, so a participant shows a phone number until the chat is reopened.
4. **SMS/gm runtime is a partial implementation with silent catch-alls.** `gmessages_runtime.rs:1868-1873` swallows every unhandled command with a `log::debug!` and `Ok(())` — no `MessageFailed`, no toast. This produces silent data loss (compose) and inert menus (chat-list).
5. **`bincode` 1.x + `#[serde(default)]` is not a migration strategy.** Confirmed by reproduction. Every added field to `IncomingMessage`/`ChatSummary` risks corruption; the WA chat file has a legacy-decoder shim, the gm cache and message files do not.
6. **Non-atomic, un-fsync'd writes.** Every persistence path is `std::fs::write` (truncate-in-place). A crash mid-write is total loss for that aggregate file.

---

## Priority tiers

### P0 — correctness / data-loss / crash (fix first)

- **serde(default) does not migrate bincode files** — an added field to `IncomingMessage`/`ChatSummary` blanks a chat's history or empties the SMS list; bytes survive but stay invisible until re-sync. Root cause: `bincode` 1.x is positional, `serde(default)` only fires at true EOF, never mid-`Vec` (reproduced). Fix: bump `BIN_HEADER` on any field add + `Legacy*` decoder chain (mirror `LegacyChatSummary` at `runtime.rs:119-164`), re-save on legacy read; add a serde round-trip regression test. Effort: **L**. (`bridge.rs:588-660,561-585`; `runtime.rs:39-42,73-79`)
- **gm_chats.bin headerless, no fallback** — the update that added `auto_mark_read` (and every future `ChatSummary` change) silently resets the SMS cache to empty; `upsert_gm_chat_cache` then writes the empty vec back, making the loss *permanent*, not a one-restart blip. Fix: route `gm_chats.bin` through `write_bin`/`read_bin` + a legacy fallback; critically, guard `upsert_gm_chat_cache` to **abort the write** when a non-empty file deserialized to empty. Effort: **M**. (`gmessages_runtime.rs:546-549,202-205,816-849`)
- **message file invisible on schema mismatch** — a post-`WA02` `IncomingMessage` field addition makes `load_messages` fall through to `return vec![]`; chat opens blank though bytes exist. Fix: per-layout `IncomingMessage` decoder chain like `LegacyChatSummary`; surface a visible "history needs re-sync" state instead of silent blank. Effort: **M**. (`runtime.rs:385-424`)
- **Non-atomic writes corrupt on crash** — a crash/power-loss mid-save leaves a truncated aggregate file: empty sidebar, lost chat history, or read chats reverting to unread. Fix: `atomic_write` helper (temp file in same dir → `sync_all` → `rename`); route `write_bin`/`write_bin_path` and the gm `std::fs::write` sites (`249/385/849`) through it; optional one-gen `.bak`. Effort: **M**. (`runtime.rs:82-89,100-107`)
- **SMS reply/voice-note/GIF/sticker silently dropped** *(critical data-loss)* — in a gm chat these compose actions append an optimistic bubble, clear the input, then hit the silent catch-all: bubble stuck on a Pending clock forever, recipient gets nothing, no error/Resend. Fix: in the `other =>` arm, emit `WaEvent::MessageFailed{msg_id: tmp_id, chat_id}` for tmp_id-carrying variants so the bubble flips to red + Resend appears; downgrade `SendReply` to a plain SMS `SendText` so the text still sends. **Note:** `SendPoll` has no `tmp_id` and produces no bubble — gate the poll picker to exclude/warn on gm chats separately. Effort: **M**. (`gmessages_runtime.rs:1868-1873`; `chat_view.rs:1158,2050,2216-2225,3531`)

### P1 — high user-facing pain

*Read/unread group (the flagship complaint — see deep-dive below):*

- **Live WA unread never persisted** — a read chat shows a live badge on new messages; if the app closes before the next server reseed, the badge silently vanishes on restart. Root: `persist_new_message` carries `unread_count: existing_unread` unchanged (`runtime.rs:3907`); the only live increment is the UI `Cell` (`chat_list.rs:733-736`). Fix: in `persist_new_message`, when `!is_from_me` and the chat isn't open/auto-mark-read, set `unread_count = existing_unread.saturating_add(1)`, guarded by the read watermark; make the row reflect the persisted count (single source of truth). Effort: **M**. *(Two finding IDs — `wa-live-unread-not-persisted` and `live-wa-unread-not-persisted` — are the same defect; merge.)*
- **Phone-read not reflected — upsert restores stale unread over server 0** — you read a chat on the phone; desktop still shows unread after reconnect. Root: `runtime.rs:1278-1280` restores old local unread whenever the incoming summary carries `0`, defeating `conv.unread_count==0`. Fix: gate that restore on the read watermark — only restore when `incoming_ts` is strictly newer than the watermark; the needed values are already in scope at `runtime.rs:1224-1226`. Effort: **M**. (`runtime.rs:1278-1280,1289-1295,2954-2956,3254-3286`)
- **Read receipt uses from_me last id** — opening a chat whose newest message is yours sends a read receipt on *your* message id, so incoming messages are never acknowledged and the phone keeps the chat unread. Root: `last_msg_id` set unconditionally (`runtime.rs:2706,4645`) while `last_msg_sender` only when `!is_from_me`. Fix: track a separate `last_incoming_msg_id`; better, collect all unread incoming ids and pass them to `mark_as_read`. Effort: **M**. (`runtime.rs:2706-2709,4645-4646,4862-4880`; `receipt.rs:164-202`)
- **Read (blue) ticks render gray on every restart/history load** — your sent messages show gray double-checks until a live receipt fires this session; already-read history never shows blue. Root: `MessageBubble::new` always adds `dim-label` and never branches on `ReceiptStatus`; `update_receipt` (the only accent path) isn't called at construction. Fix: call `self.update_receipt(&msg.receipt_status)` at the end of `new()`. Effort: **S**. (`message_bubble.rs:831-844,1223-1246`; `app.rs:48-54`)
- **Merged SMS→WA unread clobbered, over-counts, never clears on reply** — a merged-chat badge can vanish/revert unpredictably, over-count (4 vs one thread), and never clears when you reply from desktop. Root: two uncoordinated writers on `wa_chats.bin` (gmessages direct RMW vs WA full-vector flush) plus no `is_from_me→0` branch in `touch_wa_chat_preview`. Fix: route merged-SMS unread through `RuntimeState` (WaCommand/internal channel) so the authoritative flush includes it; add the `is_from_me` clear; reconcile against the gm watermark. Effort: **M**. **Repro suggested** to confirm the clobber timing. (`runtime.rs:434-455,1205-1300`; `gmessages_runtime.rs:207-217,1321-1343`)

*Other P1:*

- **Live reactions duplicate / never dedup / never remove** — a live reaction adds a second unstyled pill row; changing 👍→❤️ shows both; removals never propagate. Root: render-time reaction row lacks `set_widget_name("reaction-row")` so `show_reaction` never finds it and always appends; only a bare emoji is passed; removals gated out at `runtime.rs:2308`. Fix: add the widget name; factor `build_reaction_row(reactions, is_from_me)` shared by `new()` and `show_reaction`, driven by the full deduped vec; emit `ReactionUpdated` on removals. Effort: **M**. (`chat_view.rs:3348-3391`; `message_bubble.rs:927`; `window.rs:1122`)
- **Mute never suppresses notifications or sound** — muting a chat is cosmetic; banners and sound still fire. Root: `should_notify`/`should_play_sound` take no chat id; no `is_muted(chat_id)` accessor exists. Fix: add `pub fn is_chat_muted(&self, chat_id) -> bool` (mirror `is_auto_mark_read` at `chat_list.rs:997`); gate both notification and sound in `window.rs:911-946`. Effort: **S**. *(Two IDs — `mute-does-not-suppress-notifications`, `per-chat-mute-ignored-by-notifications` — same defect; merge.)*
- **Most gm chat-list actions inert; Delete broken** — Pin/Mute/Archive/Favourite/Mark-unread/Label/Block do nothing on SMS rows (menu advertises them); Delete also does nothing. Root: gm-routed commands hit the silent catch-all and the UI waits for a round-trip event that only the WA runtime emits; `DeleteChat` dies on `chat_id.parse::<Jid>()?` for `gm:N`. *(Corrections: Clear and auto-mark-read do work.)* Fix: (a) either implement in `gmessages_runtime` (server-backed pin/archive) with a local `gm_chat_flags.bin` for client-only flags, or add optimistic UI updates mirroring the auto-mark-read handler at `chat_list.rs:1486-1497`; (b) detect gm chats before the Jid parse and route Delete/Clear appropriately. Effort: **L**. (`runtime.rs:1468-1471,1495-1523,5062-5083`; `gmessages_runtime.rs:1868-1873`)
- **Attachments ignore the WhatsApp/SMS send-mode toggle** — on a merged chat, image/GIF/voice/sticker ignore the header toggle and go over the wrong channel (potentially to the wrong person). Root: `resolve_send_target` is wired only into `SendText`/`SendReply` (`chat_view.rs:2217,2227`); every media path uses the raw open id. Fix: wrap the chat_id in `resolve_send_target` for `SendImage`/`SendGif`/`SendAudio`/`SendSticker` and the multi-file drop loop. Effort: **S**. (`chat_view.rs:1161,2022-2054,3531,1658-1664`)
- **Media download failure is silent + stuck forever** — a failed tap-to-download WA media latches on "⏳ Downloading…" with no retry/error until restart. Root: `execute_media_download` error arms `return;` with no event; no `MediaFailed` variant; the placeholder self-disables after one tap. Fix: add `WaEvent::MediaFailed{msg_id,chat_id,reason}`, emit from all early returns, route to a "⚠ Tap to retry" hint that resets the `clicked` cell. Effort: **M**. (`runtime.rs:7830-7833,7840-7841,7864-7865`; `message_bubble.rs:1836-1843`; `bridge.rs`)
- **iPhone HEIC (gm/MMS) blank after restart** — a transcoded JPEG renders live, then the photo goes blank on reload because `message_to_incoming` re-points at the unrenderable `.heic`. Fix: in `message_to_incoming`, prefer a sibling `.jpg` when it exists (or record the actual render path). Effort: **S**. (`gmessages_runtime.rs:2493-2499,92-114,123-152`)
- **Group rename ignored** — renaming a group never updates the name live and shows no "X changed the group name" system message. Root: the `Subject` action falls into the `_ =>` catch-all that returns early (`runtime.rs:3610-3613`). Fix: add an explicit `Subject` arm before the catch-all (subject is in-hand, no IQ needed): `rename_chat` + `ChatNameUpdated` + a system message. Effort: **S**. (`runtime.rs:3609-3663`; `groups.rs:81-87`)
- **Group participant names never refresh live** — a participant shows a raw `+digits` until you leave and reopen the group. Root: bubbles cache `sender_name` at build with no setter; late-name events skip re-rendering (GL paint workaround). Fix: add `MessageBubble::update_sender_name`; a chat_view loop over matching bubbles; new `SenderNameResolved` event from the usync block (`runtime.rs:~4810`) + PushName/Contact updates; apply the hide/show `messages_box` workaround if the GL bug recurs. Effort: **M**. (`message_bubble.rs:330-349`; `runtime.rs:4732-4790`)
- **Unmapped @lid fuzzy-matches the wrong contact** *(data-integrity)* — a sender known only by an unmapped `@lid` can show an unrelated contact's name, and "Reply privately" can route to the wrong DM. Root: a 15-digit LID id is treated as a phone number and last-10 matched with no uniqueness guard. Fix: in `lookup_full`, return `None` for an `@lid` key not present in `by_lid`; skip `add_indices` for LID-derived keys; skip the fuzzy global lookup for raw `@lid` in `resolve_sender_name` step 6 and `lid_to_canonical_phone_jid`. Effort: **M**. (`contacts.rs:388-426`; `runtime.rs:737-741,852-864`)
- **gm phone-not-responding never forces reconnect** — after the phone stops relaying without cleanly closing the socket, SMS silently stops arriving for up to ~35 min. Root: the pinger emits a toast but never breaks the stuck stream; `next_chunk` has no idle timeout; the `long` client has only a 35-min whole-request cap. Fix: wrap `stream.next()` in a ~90–120s `tokio::time::timeout` so `read_stream` ends and `run_long_poll` reconnects. **Do NOT** use `shutdown.send(())` — it's a shared broadcast that terminates the whole task. Effort: **S**. (`longpoll.rs:412-425,490-501`; `http.rs:52-55`)
- **2FA auto-copy clobbers clipboard on WhatsApp messages** *(data-integrity)* — any WA message with a keyword ("code"/"pin"/…) plus a 4–8 digit run silently overwrites the clipboard and toasts "Verification code copied." Root: the autocopy block doesn't gate on source, even though `is_gm` is already computed two lines above. Fix: add `&& is_gm` to the guard at `window.rs:895`; optionally tighten the detector to require digit-adjacency and snapshot/restore the prior clipboard. Effort: **S**. (`window.rs:880-910`; `bridge.rs:773-826`)

### P2 — polish / UX

- **markChatAsRead sent with `message_range=None`** — a chat read on desktop can re-appear unread on *other linked devices* / after a full app-state re-sync (this is a linked-device gap, **not** the phone blue-tick path). Fix: build a `SyncActionMessageRange` from the newest incoming key + `chat.timestamp` and pass it in; depends on the incoming-id fix. Effort: **M**. **Verify against whatsmeow/Baileys**; the code comment claims `None` is intentional parity. (`runtime.rs:4855,4950,5024`; `chat_actions.rs:447-468`)
- **Read receipt sends only the single newest id** — partially-read chats may not fully clear on the phone while the desktop zeroes the count regardless. Fix together with the from_me fix: collect all unread incoming ids and pass them to `mark_as_read`. Effort: **M**. (`runtime.rs:4879/4885/4901,1186-1190`)
- **Full-sync markChatAsRead dropped** — a phone read that only survives in an app-state snapshot (re-pair/key rotation) is discarded; desktop stays unread. Fix: compare the mutation's timestamp against `read_watermarks` and honor when `action.timestamp >= chat.timestamp` instead of unconditional drop. Effort: **M**. *(Secondary own-JID group claim largely mitigated — `own_lid/own_phone` seeded on Connect.)* (`runtime.rs:2954-2956`; `client.rs:2234`)
- **Mark-as-unread not persisted; no-ops on gm/verification** — the manual unread flag is UI-only and lost on restart; for gm/verification chats the handler aborts on the Jid parse. Fix: set in-memory unread + roll the watermark back + `save_tx` before the parse; move the WA-only protocol call behind `if let Ok(jid)`. Effort: **M**. (`chat_list.rs:1026-1035`; `runtime.rs:5020-5027,2954-2963`)
- **First-run watermark seeding hides real unread** — on a missing watermark file, chats with `unread_count==0` but an advanced timestamp get stamped read, then an equal-timestamp reseed is clamped read (inclusive `>=`). Fix: seed at `timestamp-1` or track a last-read message id; make the reseed clamp strict for genuinely-new activity. Effort: **M**. Depends on the live-unread-persist fix. (`runtime.rs:1008-1018,1289-1295`)
- **gm media has no retry path** — a failed gm media download shows a static "📷 Photo" dead-end (no keys persisted, `RequestMediaDownload` is WA-only). Fix: persist a gm media-download key set + a gm-routed retry command; at minimum emit `MediaFailed`. Effort: **M**. (`gmessages_runtime.rs:1228-1231,2583,1821`)
- **WA missing-file media → permanent blank, no re-download** — a cleared/missing WA media file shows a gray box because the placeholder decision keys on `media_local_path.is_some()`, not file existence. Fix: gate on `Path::exists()` at `message_bubble.rs:580` (mirror gm's `p.exists()`). Effort: **S**. (`runtime.rs:385-424`; `message_bubble.rs:578-588`)
- **Sent media breaks if source file moved/deleted** — sent-image bubbles point at the user's original picked file (gm always; WA for any non-`/tmp` source, incl. file-dialog picks). Fix: always copy outgoing media into `wa_media`/`gm_media` (content-hash named) and store the managed path. Effort: **M**. (`gmessages_runtime.rs:1836`; `runtime.rs:6378,7577-7580`)
- **@mention corruption blanks the bubble** — a `_`/`*`/`~` delimiter opened before a URL whose text contains the same char injects a close tag inside the `href` → invalid Pango → `set_markup` leaves the label empty (no fallback). *(Narrower than "any URL with underscore" — self-contained URLs render fine.)* Fix: apply formatting before linkifying, or protect `<a>` regions; **harden `set_markup` with a plain-text fallback** (the higher-value half). Effort: **M**. (`message_bubble.rs:1967-2003,2007-2058,633-636`)
- **Unresolved @mentions show raw JID** — group mentions the client can't resolve display `@12345…@lid` un-highlighted. Fix: in `resolve_mentions`, map `@lid`→phone via `lid_to_canonical_phone_jid` first, then format; relax `highlight_mentions` for digit-led/lowercase tokens. Effort: **S**. *(Overlaps `unresolved-lid-mentions-render-raw` — same area, fix together.)* (`runtime.rs:918-953`; `message_bubble.rs:1913-1928`)
- **Poll vote wipes other voters** — tapping an option erases everyone else's counts/avatars and shows a full bar until the server echoes. Fix: merge the local vote into a copy of `poll_votes` and call the existing `apply_votes_to_widgets`; drop the hardcoded 250px/"1". Effort: **M**. (`message_bubble.rs:486-525,1098-1146`)
- **Group membership actor wrong / kicks mislabeled "left"** — admin-kicks show "Bob left"; adds/promotes omit the actor; number-change shows generic "Group info was updated." Fix: resolve `update.participant` (with self→"You") and branch Remove on self-leave vs kick; use it for Add/Promote/Demote/Modify. Effort: **S**. (`runtime.rs:3592-3609`; `events.rs:760-775`)
- **Self shown as phone number in group events** — "+164… is now an admin" instead of "You're now an admin." Fix: compare against `own_phone`/`own_lid` in the resolve closure and adjust grammar. Effort: **S**. (`runtime.rs:3581-3590,810-873`)
- **History-sync groups stuck on participant-name placeholder** — a group with no subject in the proto, not in `get_participating`, beyond the first 20 fallbacks stays on "Alice, Bob, …". Fix: spawn `fetch_group_subject` on the JoinedGroup path (as the live-new-group path does); paginate/raise the `.take(20)` cap; fix the stale "limit to 5" comment. Effort: **S**. (`runtime.rs:3024-3286,8176`)
- **push_name overwrites phonebook name** — a saved "Craig Thompson" regresses to a self-chosen "Craigy🔥" (last-writer-wins, ordering-dependent, reaches the row via the authoritative guard-bypassing path). Fix: add source/priority to `contact_names`; refuse to overwrite phonebook with push_name; emit via a non-authoritative path. Effort: **M**. (`runtime.rs:3387-3402,1137-1145`)
- **"Longer name wins" heuristic locks a wrong name (directory fallback)** — in `ContactDirectory`, a longer low-trust name permanently shadows a shorter phonebook name; affects SMS name resolution + WA numbers absent from `contact_names`. Fix: add source-priority tiers to `insert()`; length only as intra-tier tiebreak; authoritative insert path. Effort: **M**. *(Scope narrower than originally claimed — directory-fallback only.)* (`contacts.rs:208-238`)
- **SMS↔WA merge last-10 match, no uniqueness check** *(data-integrity)* — an SMS thread can merge onto the wrong WA contact when two WA chats share last-10 digits. Fix: require exactly one candidate, else no merge; ideally route merges through the global directory. Effort: **S**. (`gmessages_runtime.rs:322-330,778-791`)
- **Typing indicator never sent** — the contact never sees "typing…" on either protocol though the backend is fully wired. Fix: emit `SetTyping(true)` (throttled, routed via `resolve_send_target`) from the input `changed` handler + a ~4s `false` timeout; `false` on send/leave. Effort: **M**. (`bridge.rs:303`; `runtime.rs:4449-4456`; `gmessages_runtime.rs:1702-1707`)
- **Drafts not persisted / no chat-list "Draft:" preview** — unsent text is lost on restart with no reminder. Fix: serialize `drafts` to `wa_drafts.bin`; render a "Draft:" prefix in the chat list. Effort: **M**. (`chat_view.rs:145,2431-2491`)
- **Phantom mentions on delete + unsafe @Name→@Number replace** — deleting a picked mention still pings that person; the send-time replace is substring-unsafe/order-dependent. Fix: reconcile `pending_mentions` against the final text; word-boundary, longest-first substitution. Effort: **M**. (`chat_view.rs:1566-1569,2131-2177`)
- **Duplicate OTP notification** — an OTP SMS produces both the "code copied" OSD and a normal banner + sound. Fix: set `handled_as_2fa` and gate the generic block with `&& !handled_as_2fa`. Effort: **S**. (`window.rs:895-947`)
- **Notification click does nothing** — clicking a banner neither raises the window nor opens the chat. Fix: add a parameterized `app.open-chat` action with `chat_id` target and `set_default_action_and_target_value` in `send_desktop_notification`. Effort: **M**. (`window.rs:1805-1810,428-437`)
- **Global search blocks the UI thread** — after 400ms debounce, search reads + deserializes every `wa_messages/*.bin` on the GTK main thread → window unresponsive for seconds. Fix: wire the existing `WaCommand::SearchAllMessages` → `GlobalSearchResults` async path (currently has no sender); **but first fix the runtime-search chat_id bug below**. Effort: **M**. (`chat_list.rs:304-311,1961-2009`)
- **WA offline send has no auto-retry queue** — a message sent during a brief outage flips to Failed and needs manual per-message Resend. Fix: a pending-send queue keyed by tmp_id that re-dispatches on `Event::Connected`; distinguish transient vs permanent failures. Effort: **M**. (`client.rs:3274-3279`; `runtime.rs:4238-4246`)
- **Edit failure loses text; Edit offered with no window check** — a failed edit clears the input (text unrecoverable) and Edit is offered on any own text message regardless of age. Fix: restore text/banner on `EditFailed`; grey-out Edit past the ~15-min window. Effort: **M**. (`chat_view.rs:2069-2079,4650-4671`)

### P3 — nice-to-have / hardening

- **Unread badge double-count** — first message of a brand-new chat shows 2; older gm batch messages each bump. Fix: skip the `+1` when the row was just created; gate the increment behind `is_newer`. Effort: **S**. (`chat_list.rs:554,733-736`)
- **@lid duplicate ghost rows** — a contact appears twice when the `@lid` and phone rows resolve to different names. Fix: dedup by identity (phone_to_lid) not by exact name string. Effort: **M**. (`chat_list.rs:766-780`)
- **gm long-poll backoff unbounded** — after a sustained outage the linear backoff grows without cap (no early-wake), delaying SMS recovery by minutes. Fix: `.min(60)` the computed secs; optional connectivity probe. Effort: **S**. (`longpoll.rs:196-204,228-243`)
- **Sync spinner boolean race** — the sync bar can vanish mid-sync (multiple independent producers toggle one bool + a 60s unconditional hide). Fix: replace with an in-flight counter. Effort: **S**. (`bridge.rs:76-77`; `window.rs:1022-1044`)
- **Dual suspend detectors double-reconnect** — on resume, the connection can be torn down twice. Fix: single source of truth or debounce. Effort: **S**. (`runtime.rs:1695-1732`; `keepalive.rs:113-127`)
- **Verification-inbox badge flicker** — a non-open auto-mark-read inbox flashes 1→0. Fix: suppress the increment for auto-mark-read chats in `update_last_message`. *(No watermark change needed — the restart-re-appear concern was rejected.)* Effort: **S**. (`window.rs:851-862`; `gmessages_runtime.rs:527-539`)
- **Stale notification not withdrawn on secondary open paths** — opening from New Chat / profile / search leaves the banner. Fix: extract an open-chat helper that also calls `withdraw_chat_notification`. Effort: **S**. (`window.rs:127-138,373-381,1406-1416`)
- **No sound for hidden-window last-open chat** — a message for the last-open chat gives a silent banner when the app is in the tray. Fix: `&& !(is_current_chat && is_active)` on the sound guard. Effort: **S**. (`window.rs:944`)
- **Equal-timestamp sort nondeterminism** — tied chats reshuffle between refreshes. Fix: tiebreak on `widget_name()` (chat_id). *(The "stale time after send" half does not manifest.)* Effort: **S**. (`chat_list.rs:211-215`)
- **Edited badge never applies to media-only messages** — editing a no-caption media message to add text shows nothing. Fix: lazily create the text label in `update_text` when `text_label` is `None`. Effort: **S**. (`message_bubble.rs:606-664,1058-1075`)
- **Quoted-reply thumb full-dir scan on UI thread** — a quoted reply whose media isn't at the deterministic path does a blocking `read_dir` of all of `wa_media` + sync decode per bubble. Fix: one-time in-memory prefix index or async load. Effort: **M**. (`message_bubble.rs:274-322`; `texture_cache.rs:92`)
- **Unbounded auto-download + whole-file RAM buffer** — large incoming WA media auto-downloads with no size gate, buffered whole in RAM; re-delivery re-downloads. Fix: size-gate the auto-download; add a `path.exists()` short-circuit; stream to disk. Effort: **M**. (`runtime.rs:2732-2740,7784-7834`; `download.rs:230-237`)
- **Per-message full-history re-sort + deep clone under the global lock** — each incoming message in a large group holds the global Mutex for an O(n log n) sort + O(n) deep clone → chat-switch lag. Fix: sorted-insert instead of re-sort; move the clone out of the lock (delta or `Arc` snapshot). Effort: **M**. (`runtime.rs:3830-3834,1098-1102`)
- **MarkRead sync watermark write + chat-list clone under lock, per-message for auto-mark chats** — Fix: `spawn_blocking` the watermark write after releasing the lock; send an `Arc`/delta instead of cloning the whole chat vec. Effort: **S**. (`runtime.rs:1182-1203,4834-4837`)
- **`state.lock().unwrap()` poison-intolerant at 121 sites** — one panic under the lock cascades every later handler. *(No known live panic path today — latent SPOF.)* Fix: `.lock().unwrap_or_else(|e| e.into_inner())` via a `lock_state()` helper. Effort: **M**. (`runtime.rs`)
- **Duplicate/drifted `search_local_messages` in runtime.rs** — the runtime copy builds `chat_id` from the file-safe filename (not a JID); latent broken "open result" bug + dead duplicate. Fix: delete the runtime duplicate; route `SearchAllMessages` to the fixed `chat_list` logic. **Must be fixed before wiring async search.** Effort: **S**. (`runtime.rs:7470-7524,7501-7505`)
- **`save_messages_scoped` empty-guard is defensive, not a bug** — hardening only: track per-chat load state so correctness doesn't rely on the empty-input heuristic; log when the guard triggers. Effort: **M**. (`runtime.rs:536-538`)
- **gen_tmp_id collisions on multi-file drop** — dropping several files in the same nanosecond drops a bubble and mis-keys a receipt. Fix: append a monotonic `AtomicU64` counter. Effort: **S**. (`chat_view.rs:3789-3796`)

---

## Read/unread vs mobile — deep-dive (the flagship complaint)

The user's "read vs unread is inconsistent vs the mobile app" is not one bug; it is a **five-link causal chain** across two directions (desktop→phone and phone→desktop) plus a rendering bug. Fixing them in isolation won't fully resolve the complaint — they interact.

**Direction A — desktop shows unread that the phone cleared (phone→desktop):**
1. You read on the phone; the server sends `conv.unread_count==0`.
2. `upsert_chat` sets the summary's unread to 0 — then **`runtime.rs:1278-1280` immediately restores the stale local count** because it can't tell "omitted unread" from "authoritatively read." → chat stays unread on desktop. *(fix: gate the restore on the read watermark)*
3. If the read only survives in an app-state **snapshot** (re-pair/key rotation), the `from_full_sync` markChatAsRead is **unconditionally dropped** at `runtime.rs:2954`. *(fix: honor by timestamp vs watermark)*

**Direction B — phone shows unread that the desktop cleared (desktop→phone):**
4. You open a chat on desktop whose newest message is **from you**; the read receipt is sent on **your own message id** (`last_msg_id` set unconditionally), which acknowledges nothing → the phone never gets blue ticks and keeps the chat unread. This is the near-universal case. *(fix: track `last_incoming_msg_id`; send all unread incoming ids)*
5. Even with a valid anchor, only the **single newest id** is sent, so partial/group reads may not fully clear on the phone — while the desktop zeroes the count regardless, guaranteeing silent divergence. *(fix: multi-id list, already supported in `receipt.rs:186-193`)*

**Rendering:**
6. Your sent messages show **gray ticks on every restart/history load** because `MessageBubble::new` never calls `update_receipt` — so even correctly-read history *looks* unread. *(fix: one line at the end of `new()`)*

**Underlying data-model rot feeding all of the above:**
- Live WA unread is **never persisted** (UI `Cell` only), so the persisted scalar the server reseeds against is chronically wrong, and a genuinely-unread chat can carry `unread_count==0` with an advanced timestamp — which then mis-seeds the **first-run watermark** as "read."
- Merged SMS unread is written by a **second uncoordinated writer** and clobbered by the WA full-vector flush.

**Recommended fix sequence for read/unread (in order):**
1. **Persist live WA unread** in `persist_new_message` (watermark-guarded) and make the row read from the persisted count — collapses the two representations into one source of truth. *(unblocks the seed fix)*
2. **Fix the read-receipt anchor**: track `last_incoming_msg_id`, send the full set of unread incoming ids, skip when there's no incoming id. *(kills the dominant phone-side-unread case)*
3. **Gate the `upsert_chat:1278` restore on the watermark** so a phone-read `0` is trusted. *(kills the dominant desktop-side-unread case)*
4. **Render read ticks at construction** — `self.update_receipt(&msg.receipt_status)` in `new()`. *(makes read history look read)*
5. **Fix first-run watermark seeding** (seed at `ts-1` / last-read id; strict `>` on genuinely-new reseeds) — do this *after* #1 so the store it seeds from is correct.
6. **Honor full-sync markChatAsRead by timestamp** instead of dropping it. *(re-pair/rotation robustness)*
7. **Route merged-SMS unread through `RuntimeState`** and add the `is_from_me→0` clear. *(SMS parity)*

**On `message_range=None`:** it does **not** need to be fixed to resolve the user's phone-side blue-tick complaint — that is driven by the read *receipt* (#2), not this app-state mutation. `message_range` only affects **other linked devices** and full app-state replays. Fix it as a P2 follow-up *after* the incoming-id tracking (#2) exists, since it reuses the same incoming key, and validate against whatsmeow/Baileys (the code comment claims `None` is intentional parity).

---

## Recommended execution order

Each batch is independently shippable. Ordered by impact-over-risk.

**Batch 1 — Persistence hardening (P0, do first; everything else writes through this).**
Atomic writes (`atomic_write` helper) + gm_chats.bin versioned header/legacy fallback + the `upsert_gm_chat_cache` abort-on-empty guard + `IncomingMessage`/`ChatSummary` legacy-decoder chains + a serde round-trip regression test. *Rationale: these prevent silent data loss and are prerequisites — you don't want to change `ChatSummary`/`IncomingMessage` for the unread fixes until migration is safe. Risk: medium (touches all save paths) — land the round-trip test first.*

**Batch 2 — Read/unread core (P1, the flagship).**
Steps 1–4 of the read/unread sequence: persist live WA unread, fix the read-receipt anchor + multi-id, gate the `1278` restore on the watermark, render read ticks at construction. *Ship as one batch — they interlock; shipping only one can shift symptoms without fixing the complaint. Risk: medium.*

**Batch 3 — Read/unread completion (P1/P2).**
First-run watermark seeding fix, full-sync markChatAsRead honor-by-timestamp, merged-SMS unread routed through `RuntimeState` + `is_from_me` clear, mark-as-unread persistence. **Repro suggested before coding the merged-SMS clobber** — capture a log of an incoming SMS on a merged chat followed by a WA-side flush to confirm the clobber timing before restructuring the write path.

**Batch 4 — SMS/gm parity (P0 data-loss + P1).**
Silent compose-drop → `MessageFailed` (reply/voice/GIF/sticker) + gm poll-picker gating; attachment `resolve_send_target`; gm chat-list actions (pin/mute/archive/…/Delete) via optimistic UI or gm implementation. *This is the second-biggest user-facing cluster. The compose-drop half is P0 (silent data loss). Risk: the chat-list-actions half is Large.*

**Batch 5 — Notifications + 2FA + media reliability (P1).**
Mute-suppresses-notifications, 2FA `&& is_gm` guard, `MediaFailed` event + retry (WA and gm), HEIC sibling-jpg, WA missing-file existence gate, sent-media copy-to-managed-dir. *All small/medium, high daily-annoyance payoff, low interdependence.*

**Batch 6 — Rendering + groups + names (P1/P2).**
Live-reaction rebuild, group rename system message, live participant-name refresh (with the GL paint workaround ready), unmapped-@lid None-return, group actor/self labels, mention corruption + `set_markup` fallback, poll-vote merge. *Group by file (`message_bubble.rs`, group handler in `runtime.rs`, `contacts.rs`) to minimize churn.*

**Batch 7 — Connection + search + perf hardening (P2/P3).**
gm phone-not-responding idle timeout, gm backoff cap, delete the duplicate runtime `search_local_messages` **then** wire async search off the main thread, offline send queue, typing indicator, drafts persistence. *The search duplicate-delete must precede wiring the async path.*

**Batch 8 — Low-severity polish (P3).**
Badge double-count, sort tiebreak, spinner counter, dual-suspend debounce, notification click action, poison-tolerant `lock_state()`, per-message sort/clone-under-lock, gen_tmp_id counter, and the remaining P3 items — cherry-pick as capacity allows.

**Items to confirm with a runtime/log repro before coding:**
- **Merged-SMS unread clobber** (Batch 3) — confirm the WA flush actually overwrites the gmessages increment on your real timing.
- **`message_range=None`** (P2) — validate the correct `SyncActionMessageRange` key format against whatsmeow/Baileys before sending; the current `None` is a deliberate parity choice per the in-code comment.
- **Full-sync markChatAsRead honor** (Batch 3) — verify against a real re-pair/key-rotation that snapshots carry the mark-read mutations you intend to honor, so you don't re-introduce the stale-snapshot problem the `2954` drop was added to prevent.

**Confidence/unknowns:** the persistence findings were reproduced with `bincode` 1.3.3 and are high-confidence. The read/unread chain was traced end-to-end with all values shown in-scope. Lower-confidence-by-nature (race-dependent, so hard to guarantee frequency): `@lid` ghost rows, SMS↔WA mis-merge, push_name-overwrite ordering — all confirmed in code but their real-world hit rate depends on timing/data collisions.

Evidence file paths referenced throughout are under `/home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/` (`ui/runtime.rs`, `gmessages_runtime.rs`, `ui/window.rs`, `ui/chat_list.rs`, `ui/message_bubble.rs`, `ui/chat_view.rs`, `bridge.rs`, `ui/settings.rs`, `ui/app.rs`, `ui/texture_cache.rs`) and the crate roots (`src/client.rs`, `src/download.rs`, `src/receipt.rs`, `src/keepalive.rs`, `src/features/chat_actions.rs`, `src/handlers/notification.rs`, `wacore/`, `gmessages-rust/src/longpoll.rs`, `gmessages-rust/src/http.rs`).

---

# Part C — How to sequence the two audits together

Each audit has its own internally-ordered batch plan (above). Executed together, the cross-audit dependencies
are:

1. **FIRST — Persistence hardening** (Part B, Batch 1). Atomic writes + `gm_chats.bin` versioned header/legacy
   fallback + `upsert_gm_chat_cache` abort-on-empty guard + `IncomingMessage`/`ChatSummary` legacy-decoder
   chains + a serde round-trip regression test. **Blocks everything that changes a persisted struct** — and the
   read/unread work does. Do not touch `ChatSummary`/`IncomingMessage` fields until this lands.
2. **Feedback foundation** (Part A, Batch 1) — the single `adw::ToastOverlay` + render `ErrorToast`. Cheap, and
   it is what makes the logic-side error paths (calls, group ops, block, media failures) actually visible, so
   several Part B fixes ("surface the failure") become one-liners once it exists. Do it early.
3. **Read/unread core** (Part B, Batch 2) — persist live WA unread, fix the read-receipt anchor + multi-id,
   gate the `upsert_chat:1278` restore on the watermark, render read ticks at construction. Ship as ONE batch;
   they interlock. This is the flagship complaint.
4. **Destructive-action confirmations** (Part A, Batch 2) — all reuse the existing `show_confirm_dialog`; low
   risk, high safety payoff.
5. **SMS/gm parity** (Part A Batch 3 + Part B Batch 4, combined) — the shared gm catch-all: emit
   `MessageFailed` for dropped sends, gate the poll picker, fix `command_chat_id` routing for `gm:`
   Delete/Clear, and wire (or gate) the gm context-menu actions. Second-biggest user-facing cluster.
6. **Per-chat state correctness + missing states** (Part A Batches 4-5) — favourite/label buttons, compose
   disabled with no chat, empty states, transient-disconnect offline banner (don't eject to QR).
7. **Read/unread completion + media/notifications reliability** (Part B Batches 3 & 5) — watermark seeding,
   full-sync mark-read, merged-SMS unread routing, `MediaFailed` + retry, HEIC sibling-jpg, mute-suppresses-
   notifications, the 2FA `&& is_gm` clipboard guard.
8. **Rendering + groups + names, then polish** (Part B Batches 6-8 + Part A Batches 6-7) — live reactions,
   group rename/actor labels, live name refresh, dialog styling consistency, then the P3 hardening.

**Confirm with a runtime/log repro before coding:** the merged-SMS unread clobber timing, the
`message_range=None` linked-device path (validate the `SyncActionMessageRange` format vs whatsmeow/Baileys),
and that re-pair/rotation snapshots actually carry the mark-read mutations before honoring full-sync mark-read.

*Generated from two adversarially-verified multi-agent audits (58 usability + 68 logic findings). Ready for
execution — recommend running the batches through Opus one at a time, each independently shippable.*
