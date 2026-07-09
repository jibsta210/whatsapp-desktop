# Decisions for review (not auto-applied)

Everything else — all the objective bugs from the audit (read ticks rendering gray, mute not suppressing notifications, silent SMS data-loss, persistence corruption, wrong name resolution, missing empty states, the disabled-compose / filter-chip / escape-to-close affordances, MediaFailed events, and so on, 103 findings in total) — is being fixed automatically in batches. **This file is only the judgment calls**: changes that alter product behaviour or policy, add or remove a feature, touch privacy, or hide/disable UI you might actually want. Nothing below has been touched. Pick an option per item and the fixes will follow.

---

## Confirmations

Every one of these fires a destructive action *immediately* today with no dialog. The app already ships a styled `adw::AlertDialog` helper (`show_confirm_dialog`, `chat_list.rs:1697`), so "add a confirmation" is trivially cheap. These are DECISIONs — not bugs — because adding a confirmation is a UX-policy choice (friction vs. safety), and WhatsApp itself confirms some of these but not all. Recommend saying **yes to all** except where noted; they are individually toggleable.

**Delete-for-everyone / delete-for-me (message menu)** — would add a confirm modal before an irreversible unsend. · *Judgment call:* a single misclick unsends permanently, but power users hate an extra click on every delete. · **A:** confirm only delete-for-everyone (irreversible for the recipient); delete-for-me stays instant. · **B:** confirm both. · **Recommend A** — matches WhatsApp mobile. · `desktop/src/ui/chat_view.rs:4744` / `:4763` (mb-02, F4)

**Leave group** — confirm + feedback before sending `LeaveGroup`. · *Judgment call:* consequential but reversible (you can be re-added). · **A:** confirm. · **B:** leave instant, rely on a toast. · **Recommend A.** · `desktop/src/ui/profile_panel.rs:428` (F2)

**Remove participant** — confirm "Remove <name>?" before `RemoveGroupParticipant`. · *Judgment call:* visible to others, mildly embarrassing if mis-fired. · **A:** confirm. · **B:** instant. · **Recommend A.** · `desktop/src/ui/profile_panel.rs:1125` (F3)

**Clear SMS cache (settings)** — confirm before wiping `gm_*` + `gm_chats.bin`. · *Judgment call:* destroys cached history but re-syncable from the phone. · **A:** confirm + "Cleared N conversations" toast. · **B:** instant. · **Recommend A.** · `desktop/src/ui/settings.rs:759` (F7)

**Delete saved send-group (broadcast list)** — confirm before `remove`+`save`. · *Judgment call:* config loss, low stakes. · **A:** confirm + live row refresh. · **B:** instant + refresh only. · **Recommend A (lightweight).** · `desktop/src/ui/window.rs:2354` (F11)

**Multi-file drag-drop** — currently loops `SendImage` and fires every dropped file *instantly*, no preview. · *Judgment call:* this is both a missing-confirm and a small feature (a staging/review queue). Because a mis-drop is unrecoverable and invisible, but the "right" fix (a full staging UI) is real work. · **A:** minimal — a styled confirm modal listing the N files before sending. · **B:** full staging queue with removable thumbnails + optimistic bubbles (larger; the staging-UI rework is otherwise deferred). · **Recommend A now, B later.** · `desktop/src/ui/chat_view.rs:1647` (cv-04)

---

## Calls / WebRTC

The call buttons and the incoming-call dialog are wired to command stubs that only log — real WebRTC is not implemented. These are DECISIONs because the choice is *hide the affordance vs. keep it and explain* vs. *build the feature*, and building it is a large separate effort.

**Video / voice call buttons (chat header)** — always enabled, click sends `InitiateCall` which the runtime only logs; the resulting error toast is swallowed. A false affordance. · *Judgment call:* do you want the buttons visible as a "coming soon" signal, or gone until they work? · **A:** hide/disable both buttons until WebRTC lands (cleanest, no false promise). · **B:** keep them but show a styled "Calls aren't supported yet" notice on click (requires the toast/error layer, which *is* being added in batch 2). · **Recommend B** — keeps discoverability, honest feedback, cheap once the toast layer exists. · `desktop/src/ui/chat_view.rs:208` (F1)

**Accept incoming-call dialog** — a nicely-styled dialog appears, but Accept/Reject/End are log-only no-ops; Accept closes the dialog and nothing happens. · *Judgment call:* surfacing a dialog you can't honour is worse than not surfacing it. · **A:** don't show the incoming-call dialog at all until WebRTC lands. · **B:** show it but replace Accept with a "Answer on your phone" message. · **Recommend B** — the user still learns someone is calling. (Real answer = the deferred WebRTC work.) · `desktop/src/ui/window.rs:2022` (F6)

---

## Feature gaps

Adding or removing a feature — reasonable people differ on whether these belong in the product at all.

**"Event" attach row** — the attach popover lists "Event" but the handler has no branch, so it silently closes. · *Judgment call:* build an event creator, or drop the row? · **A:** remove the "Event" entry (honest, zero risk). · **B:** keep it and show a styled "coming soon" notice. · **C:** build a real event creator (feature work). · **Recommend A** unless events are on the roadmap. · `desktop/src/ui/chat_view.rs:1707` (cv-01)

**Log out / unlink device** — `WaCommand::Logout` exists and is handled, but there is **zero** UI to trigger it; the capability is unreachable. · *Judgment call:* adding a destructive account control is a product decision, not a bug fix — some users want it, and a mis-click un-pairs the device (full re-scan to recover). · **A:** add a confirmed, destructive "Log out / Unlink" row under a new Account group in settings. · **B:** leave it out (keep the app "always linked"). · **Recommend A** — a linked-device app without a logout is a real gap; guard it behind a confirm. · `desktop/src/bridge.rs:317` (F9, GW-09 — one underlying gap, counted twice)

---

## Privacy

Sending a signal to the other party that we don't send today. Turning it on is a privacy choice the user should opt into.

**Broadcast outgoing "typing…" indicator** — `SetTyping` is fully implemented in both the WA and gm runtimes but is *never* triggered from the UI, so peers never see you typing. · *Judgment call:* this leaks presence/activity (peers learn you opened the chat and are composing) — a deliberate privacy trade-off, not an accidental omission. · **A:** wire it on (debounced typing-true on input, idle typing-false) — matches WhatsApp default. · **B:** wire it, but gate behind a settings toggle defaulted off. · **C:** leave off. · **Recommend B** — honour the feature but let the user decide. (Both finding IDs `cv-11` and `typing-indicator-never-sent` are the same defect.) · `desktop/src/ui/chat_view.rs:1416`, `desktop/src/bridge.rs:303`

---

*14 decisions above. The remaining 112 findings are either objective bugs being auto-fixed (103) or large/risky items deferred for a runtime repro (9 — WebRTC call initiation, full server-side gm chat-list actions, the async global-search rework, an offline send-retry queue, the mic-gesture and inline-audio rework, the 121-site poison-tolerant lock refactor, and three read-sync items that need a re-pair / log repro).*
