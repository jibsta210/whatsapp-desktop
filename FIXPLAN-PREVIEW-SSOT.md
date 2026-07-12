# PREVIEW-SSOT — corrected implementation spec (auto-assembled)

## D3 DESIGN (architect)

ALL PATHS RELATIVE TO /home/jakes/Projects/Whatsapp/whatsapp-desktop/desktop/src/. Two implementers, disjoint files. A = producers (bridge.rs, ui/runtime.rs, gmessages_runtime.rs). B = consumers (ui/chat_list.rs, ui/window.rs). bridge.rs is the contract; A owns it and lands it first (it is a pure addition, B compiles against it).

════ CONTRACT (bridge.rs, owned by A) ════
C1. Add to enum WaEvent (bridge.rs:11, after ChatAdded at :24):
    /// Authoritative refresh of one chat's sidebar row. The row renders this
    /// VERBATIM — preview, timestamp, unread, flags — no guards, no UI clocks.
    /// Producer contract: ids starting "gm:" are emitted ONLY by
    /// gmessages_runtime; all other ids ONLY by the WA runtime. Producers
    /// guarantee monotonicity (never emit older-than-last state for a chat).
    ChatRowChanged(ChatSummary),
C2. Add to enum WaCommand (bridge.rs:296):
    /// INTERNAL (gm→WA): an SMS/MMS landed on (or was sent from) a chat merged
    /// into a WhatsApp row. The WA runtime — sole owner of non-gm summaries —
    /// applies it to RuntimeState.chats, persists, and emits ChatRowChanged.
    /// Replaces the old touch_wa_chat_preview direct disk write.
    TouchChatSummary { chat_id: String, preview: String, timestamp: i64, is_from_me: bool },
C3. Delete WaEvent::ChatPreviewUpdated (bridge.rs:181-184) and WaEvent::ChatMarkedUnread (bridge.rs:133-135) once A and B land (final cleanup commit; grep must show zero refs). Keep ChatReadOnOtherDevice (still used for notification withdrawal) and MessageConfirmed (still re-keys bubbles).

════ IMPLEMENTER A — ui/runtime.rs ════
A1. RuntimeState gains the projection tap. Add field `ui_tx: Option<async_channel::Sender<WaEvent>>` (near active_chat, ~:1184); set it once in run_inner right after state construction. Add method:
    fn emit_row(&self, chat_id: &str) {
        if chat_id.starts_with("gm:") { return; } // ownership rule
        let (Some(tx), Some(c)) = (&self.ui_tx, self.chats.iter().find(|c| c.id == chat_id)) else { return };
        let _ = tx.try_send(WaEvent::ChatRowChanged(c.clone())); // try_send: sync-safe, unbounded never Full
    }
A2. upsert_chat (runtime.rs:1441-1549): before mutating an existing entry, snapshot (last_message, timestamp, unread_count, is_pinned, is_muted, is_archived, is_favorite, label, auto_mark_read); at the end (after :1547 sort), call self.emit_row(&id) iff entry is new OR the snapshot changed. Change-detection prevents an event flood during history-sync reseeds (ChatsLoaded still covers bulk). This single hook covers every persist_chat / persist_chat_authoritative / persist_new_message call site — including SendText (4908), where summary.timestamp = sent_msg.timestamp (the runtime's post-send now(), 4869-4881) is now the ONLY clock that ever reaches the row: D1 dead by construction.
A3. mark_chat_read_local (1406-1434): inside the `if changed` block (:1431), after save_tx, call self.emit_row(chat_id). Covers: from_me echo clears (4596), MarkRead (5544), auto-mark-read, and every ChatReadOnOtherDevice producer (3312, 3380, 5548 — implementer must verify each of those three sites calls mark_chat_read_local; add the call where missing so state and event agree).
A4. MarkUnread (5792-5818): after the inline state block (:5813) call state.lock().unwrap().emit_row(&chat_id); DELETE the ChatMarkedUnread send (5814-5818).
A5. New method RuntimeState::set_chat_preview(&mut self, chat_id: &str, preview: &str) — sets chats[chat].last_message (timestamp UNCHANGED), save_tx, emit_row. Convert the direct last_message writes to use it: edit-latest (~2553-2558 and gm-edit ~7795), delete-latest (~5117, ~6127), LoadChat group-sender re-resolution (~5370-5395). At the LoadChat site DELETE the ChatPreviewUpdated emission. ClearChat handler: set last_message = "" via set_chat_preview (timestamp preserved — fixes the epoch-date render of the old clear_chat_messages) and keep emitting ChatCleared for chat_view.
A6. Reaction-latest (~5966): reactions must NOT persist into last_message (restart intentionally shows the underlying message). Emit an ephemeral override: clone the ChatSummary from state, set last_message = format!("Reacted {emoji}"), tx.send(WaEvent::ChatRowChanged(clone)) WITHOUT persisting. window.rs keeps ReactionUpdated only for the bubble row.
A7. Preview formatting moves producer-side. New pub fn row_preview(m: &IncomingMessage, is_group: bool) -> String in runtime.rs: media_preview (4632-4649) + the group "You: "/"{sender}: " prefixes and @<digits> mention resolution currently in chat_list.rs:695-727 (uses crate::contacts::global(), already cross-thread). persist_new_message :4571 switches `let preview = row_preview(m, is_group);`. Side effect (accepted, an improvement): wa_chats.bin previews now carry prefixes, so restart matches live rendering for the first time.
A8. New WaCommand::TouchChatSummary handler in handle_command: lock state; find chats entry; guard `timestamp > existing.timestamp` (move-forward, and skip empty preview clobbering a non-empty one) → set last_message/timestamp; if !is_from_me && active_chat != chat && timestamp > read_watermark → unread+1; if is_from_me → mark_chat_read_local; save_tx; emit_row. DELETE pub fn touch_wa_chat_preview (514-535) after D-side callers are converted.
A9. Dispatcher (1691-1754): (a) move `let (wa_cmd_tx, wa_cmd_rx) = ...` (:1691) ABOVE the gm spawn (:1685) and pass wa_cmd_tx.clone() into gmessages_runtime::spawn (signature change, see G1); (b) add a SetActiveChat fan-out next to the MarkRead one (:1703): clone-send to gm_cmd_tx, then forward to WA as today.

════ IMPLEMENTER A — gmessages_runtime.rs ════
G1. spawn (:57) takes an extra `wa_cmd_tx: tokio::sync::mpsc::UnboundedSender<WaCommand>`; thread it into run() and handle_command().
G2. upsert_gm_chat_cache (241-293): return Option<ChatSummary> (post-upsert clone). Fixes while here (producer-side guards replacing deleted UI guards): (a) for an existing chat, when !is_from_me && timestamp >= existing.timestamp && conv not active && timestamp past the gm read watermark → unread_count += 1 (today it never increments, D2); (b) never overwrite a non-empty last_message with an empty preview (the outgoing-RCS empty display_content case that add_chat's deleted asymmetry used to paper over). Callers pass the active-chat handle + watermarks (both already plumbed through run(); add an `active_chat: Arc<Mutex<Option<String>>>` updated by the new SetActiveChat command arm — dispatcher now forwards it per A9b).
G3. Incoming loop STEP 3 (1441-1463): replace the touch_wa_chat_preview call (:1457) with wa_cmd_tx.send(WaCommand::TouchChatSummary{..}). STEP 4 (1471-1495): use the returned summary → event_tx.send(WaEvent::ChatRowChanged(summary)) right after the upsert (before the final event forward at :1500 is fine; the message's MessageReceived follows on the same FIFO channel).
G4. Send paths — SendText/SendReply (1588-1638), ResendMessage (1665-1711), SendImage (~2105-2166), translate_event tmp-echo (~2666-2673): after building the echo (whose now_s at :1599 is now the ONLY clock for this event), persist + emit: if echo.chat_id starts with "gm:" → upsert_gm_chat_cache(...) and send ChatRowChanged(returned summary); else (merged: WA JID) → wa_cmd_tx.send(TouchChatSummary{ preview: text, timestamp: echo.timestamp, is_from_me: true }). Emission order per send: MessageConfirmed → MessageReceived(echo) → ChatRowChanged (keeps bubble-before-sidebar).
G5. gm MarkRead leg (unread clear in gm_chats.bin, ~1895-1910): after zeroing, emit ChatRowChanged for each affected gm summary. gm MarkUnread leg (~1940): also set unread=1 in gm_chats.bin and emit.
G6. Reseed ChatAdded (~987-991) monotonic overlay: before sending, if gm_chats.bin holds a newer timestamp/preview for the conv (updated live by G2/G4), overlay it onto the outgoing summary — the UI-side strict-> guard that used to absorb stale reseeds is gone, so staleness must die here.

════ IMPLEMENTER B — ui/chat_list.rs ════
B1. New sole writer:
    pub fn apply_summary(&self, chat: &ChatSummary) {
        if row absent → self.add_chat_row(chat.clone()) and return;   // creation path, keeps phone-dedup 1163-1181
        row.update_preview(&chat.last_message, chat.timestamp);       // verbatim; time_label from summary ts
        row.set_unread(chat.unread_count);
        flags verbatim (is_pinned/is_muted/is_archived/is_favorite/auto_mark_read + indicators + label_badge, as in 518-525/1143-1156);
        inner.timestamps.insert(id, chat.timestamp);                  // sort key = summary ts, nothing else, ever
        invalidate_sort + invalidate_filter;
        // Does NOT touch chat_name/name_label (name flow keeps its hardened heuristics via update_chat_name*).
        // Typing overlay untouched: preview_label text may change while typing_box is visible (label is hidden).
    }
B2. add_chat (510-572) collapses to: `if exists { self.apply_summary(&chat) } else { self.add_chat_row(chat) }` — the strict-> guard, the update_preview_timestamp branch, and the "trust the server" unread comment block are DELETED (guards now live in upsert_chat / G2 / G6). ChatsLoaded/ChatAdded callers unchanged.
B3. DELETE: update_last_message (574-812) whole fn incl. both diag log lines; bump_chat_to_top (859-869); update_preview_text (1129-1134); clear_chat_messages (1136-1141); reset_unread (871-876); mark_chat_unread (1099-1108); load_chats (465-480, dead); typing_previews field + refs (41, 196, and the :792 removal — active_typers stays); the optimistic set_unread(0) in connect_row_activated (446-449). After this commit `grep -n "SystemTime::now" ui/chat_list.rs` MUST return nothing.
B4. ADD note_recent_message(&self, chat_id: &str, msg: &IncomingMessage): the stealth-peek cache block verbatim from 754-766 (dedup by id, cap 10) plus the typing-overlay reset from 791-801 (a real message supersedes typing). Touches recent_messages + active_typers + visibility only — never preview/time/unread/timestamps.
B5. KEEP unchanged: sort/filter funcs (204-299), set_typing (888-970), stealth hover (1203-1404), context menu, remove_chat/remove_stale/remove_lid_duplicate, select_chat, chat_name, set_avatar, update_chat_name*, flag setters (set_chat_archived/muted/pinned/favorite/label/auto_mark_read — their events are flag-only, clock-free; folding them into ChatRowChanged is a later cleanup).

════ IMPLEMENTER B — ui/window.rs ════
W1. Add handler: WaEvent::ChatRowChanged(chat) => inner.chat_list.apply_summary(&chat);   // deliberately NO remove_lid_duplicate / rail refresh — that's why ChatAdded is not reused (its handler at 934-957 has per-chat side effects that must not fire per-message).
W2. MessageReceived (958-1100): replace the update_last_message call (988-990) with inner.chat_list.note_recent_message(&msg.chat_id, &msg). Everything else (typing clear, append_message-first, MarkRead gating, 2FA, notification/sound) stays byte-identical.
W3. MessageConfirmed (1101-1108): delete the bump_chat_to_top line (:1107); keep confirm_bubble.
W4. Delete the row-write halves of: ChatReadOnOtherDevice (:1171 reset_unread — keep withdraw_chat_notification), ChatPreviewUpdated (whole arm :1226-1228), ChatMarkedUnread (:1259-1261), ChatCleared (:1276 chat_list call — keep chat_view.clear_chat), ReactionUpdated preview branch (1294-1298), MessageDeletedLocal new_preview branch (1319-1321), MessageEdited is_latest branch (1335-1337). Runtime events now carry these as ChatRowChanged.

════ DESIGN DECISIONS (task Q1-Q6, concretely) ════
1. Event: NEW ChatRowChanged(ChatSummary), not ChatAdded (side-effect-laden handler, window.rs:934-957). Emission choke point: RuntimeState mutators (upsert_chat + mark_chat_read_local + set_chat_preview + MarkUnread inline), because persist_new_message covers only the 10 message paths — edits/deletes/reactions/read-state mutate state directly. Outbound sends need no special handling: SendText Ok already flows persist_new_message with the runtime's single post-send timestamp (4869/4881→4576); with bump_chat_to_top gone there is no second clock anywhere.
2. gm parity: option (b) — gmessages_runtime emits from its own persisted store (upsert_gm_chat_cache already exists and is incrementally maintained; making it return the summary is 5 lines), NOT window.rs-side derivation (option (a) would recreate a UI-thread computer of row state — the exact class being deleted — and window.rs has no access to gm watermarks/active-chat needed for unread). Merged chats are the one cross-owner surface: routed to the WA runtime via TouchChatSummary, which simultaneously retires the gm-thread's behind-the-back wa_chats.bin disk write (runtime.rs:514-535) that could silently lose updates to the WA save_tx flusher — without this re-route, a WA-side mark-read emission would regress a merged row's SMS preview from stale in-memory state.
3. Demolition: see B2/B3/W3/W4. Kept: typing overlay, stealth peek (new feeder), sort/filter, creation/removal, name heuristics, flag setters.
4. Unread: runtime-owned end-to-end. WA: persist_new_message computes (4481-4523, active-chat + auto_mark_read + receipt-watermark aware) → emitted by A2; clears via mark_chat_read_local → A3; MarkUnread → A4. gm: G2 adds the missing live increment with active-chat + watermark suppression (dispatcher fans SetActiveChat to gm, A9b/G2). Row badge writes exist ONLY inside apply_summary; the optimistic click-clear is deleted (round-trip is one channel hop, ~ms; if perceptible, re-add as a documented converging exception — not expected).
5. Ordering: one async_channel (app.rs:468) shared by both producers (runtime.rs:1685), drained FIFO on the GTK main context (app.rs:499-524) — no second channel, no cross-channel races. Per-chat, single-owner emission makes summary events for one chat totally ordered by their producer's own store mutations. ChatRowChanged lands adjacent to (typically same 8-event batch as) its MessageReceived, so the bubble/preview paint in the same frame.
6. Migration safety / regression watchlist: (a) notification gating reads row flags — safe, ChatAdded/ChatRowChanged precede MessageReceived (4908-4920, 3118→); (b) restart previews gain You:/sender prefixes (A7 — intended); (c) reaction preview no longer survives restart-preview — unchanged semantics, now explicit (A6); (d) reseed staleness absorbed producer-side (G6, upsert_chat 1465) instead of row guards; (e) stealth peek + typing must keep working via note_recent_message; (f) chat_view/bubbles are untouched (chat_view.rs:2403 optimistic bubble ts only labels the bubble, never the row).

════ ACCEPTANCE TESTS ════
T1 (the bug): send 20 messages from desktop across a minute (crossing second boundaries); sidebar preview equals the last sent text every time; row at top. Structural: grep chat_list.rs for SystemTime::now → empty; grep -rn "bump_chat_to_top" → empty.
T2: incoming WA message to a background chat → preview + time + badge+1 + top, in one paint. To the ACTIVE focused chat → no badge. Group message → "Sender: text" with @mention resolved (now from runtime).
T3: edit latest → preview = new text, row does NOT reorder (timestamp untouched via set_chat_preview); edit older → row untouched. Delete-for-everyone latest → "🚫 Message deleted". React to latest → "Reacted 👍"; restart → underlying message text (not "Reacted"); react to older → row untouched.
T4 (gm): send SMS from desktop (gm: chat and a MERGED WA-JID chat) → preview updates immediately and survives restart (gm_chats.bin / wa_chats.bin respectively); incoming SMS to background gm chat → badge+1 (new behavior, was UI-only before); incoming SMS to open gm chat → no badge; 2FA SMS still routes to Verification Codes.
T5: read on phone → badge clears (ChatReadOnOtherDevice site emits); right-click Mark unread → badge 1, survives restart; click a chat → badge clears within perception threshold (no optimistic write).
T6: Clear chat → empty preview, row KEEPS its position and shows a sane date (not epoch).
T7: full restart + reconnect reseed → no unread resurrection, no preview regression (existing hardening untouched; G6 covers gm reseed).
T8: typing indicator shows/hides; message arrival clears it; stealth hover peek shows recent messages; muted chat suppresses notification+sound.
T9: cargo clippy -p whatsapp-desktop && cargo build --release -p whatsapp-desktop (NOTE: default cargo build does NOT build the GUI); after install (kill + cp to ~/.local/bin, pre-authorized) run with RUST_LOG=info and confirm the update_last_message[self] log lines are gone from the codebase entirely.


## ADVERSARIAL CORRECTIONS (holds=False on 4 invariants; incorporate ALL)

Core architecture (owner-per-id, choke-point emission, TouchChatSummary reroute, deleting the UI clock) is sound and the cited anchors are mostly accurate — but four stated invariants are refuted at HEAD and must be corrected before implementation. All paths relative to desktop/src/.

(1) COVERAGE: the choke point does NOT cover every send. SendAudio (ui/runtime.rs:8056-8135) never persists and never echoes MessageReceived — its only row-mover today is bump_chat_to_top on MessageConfirmed (window.rs:1107), which the design deletes. MultiSend (runtime.rs:8189+) emits MessageReceived(self_msg) with no persist_new_message. Group system messages (~4271-4279) append history + MessageReceived only. Add to A-scope: SendAudio and MultiSend build a sent IncomingMessage and call persist_new_message (mirror SendImage at 7251); system messages route through persist_new_message too (it already suppresses unread via is_system_message, 4496).

(2) A5 site list corrected: runtime.rs:2553-2558 is the incoming REVOKE writer, not edit; incoming edit-latest is 2653-2658. Full set_chat_preview conversion set: 2558 (revoke), 2655 (edit), 5132 (DeleteForEveryone), 6124 (DeleteForMe), 7788 (gm edit), 5382 (LoadChat, delete ChatPreviewUpdated at 5388).

(3) A6 must cover all three ReactionUpdated producers, not just SendReaction (5966): incoming reactions (runtime.rs:2710) and gm reactions (gmessages_runtime.rs:2057) also drive the sidebar "Reacted 👍" via window.rs:1293-1298, which B strips. The gm site emits its ephemeral ChatRowChanged itself for gm: ids and routes merged ids via TouchChatSummary-with-ephemeral flag or leaves the preview handler keyed on is_latest for gm only — pick one and write it into the contract.

(4) Producer monotonicity has a hole the deleted UI guard was masking: upsert_chat's keep_old_preview (1465-1466) only fires when the EXISTING ts is newer. CreateGroup (6472-6491) and StartNewChat (7050-7069) upsert last_message:"" with timestamp=now(), clobbering the real preview in state, then emit ChatAdded with the PRE-upsert stale payload (6490, 7068); 9075 emits merged_summary similarly. Harden upsert_chat: never overwrite a non-empty last_message with an empty one, never move timestamp backward. Additionally define ChatAdded semantics for B: create-if-missing, ignore-if-exists (row data flows only via ChatRowChanged/ChatsLoaded) — otherwise every pre-upsert ChatAdded payload and the gm boot sequence (pin row ts=now/empty preview at gmessages_runtime.rs:604-620, then older hydration rows 621-635, then reseed 991) violates verbatim rendering.

(5) gm read/unread ownership: gm rows are NOT in RuntimeState.chats (seeded from wa_chats.bin only, runtime.rs:1196), so A3/A4 emit_row is a double no-op for gm: ids. gm MarkRead already persists unread=0 (gmessages_runtime.rs:1895-1913) — add a ChatRowChanged emission there; ALSO keep window.rs reset_unread on ChatReadOnOtherDevice (it is currently the only gm badge-clear). gm MarkUnread (1947-1980) must additionally set unread=1 in gm_chats.bin and emit ChatRowChanged, or SMS mark-unread loses its badge when ChatMarkedUnread is deleted.

(6) gm send echoes (SendText 1588-1638, ResendMessage 1665-1711, SendImage ~2068-2160) must persist + emit; NOTE merged chats in SMS mode send with chat_id = the gm: pair id (chat_view.rs resolve_send_target, 2715-2735) and the echo is NOT merge-redirected — the summary update must consult merge_map and go via TouchChatSummary with the WA id or the visible merged row never moves on sent SMS.

(7) Ordering corrections: (a) the "ChatRowChanged always precedes MessageReceived" invariant is FALSE for merged SMS (gm emits MessageReceived on event_tx directly; the row update detours through wa_cmd_tx and a tokio::spawn'd handler, runtime.rs:2014) — document that B must never assume row freshness at MessageReceived time; (b) A9b's SetActiveChat fan-out to gm must be applied INLINE in gm's select loop, not via its spawned handle_command (gmessages_runtime.rs:1529), mirroring the WA race fix at runtime.rs:2002-2010, and must translate merged WA ids back to conv ids via merge_map for active-conv unread suppression; (c) TouchChatSummary handlers run concurrently (spawned) — the ts-monotonic guard makes that safe but same-second pairs drop the second preview and undercount unread (pre-existing parity with touch_wa_chat_preview:522; acceptable, state it).

(8) Perf verdict: acceptable. emit_row is a small clone + try_send on the existing unbounded channel; per-message event volume equals today's MessageReceived-driven update_last_message volume, and the GTK batch drain (app.rs:499-524) is unchanged. A2's change-detection plus ChatsLoaded-for-bulk prevents reseed floods (history sync upserts per conversation at 3755/3761, not per message). B should skip GTK setters when values are unchanged and only invalidate_sort when the timestamp actually moved.

Everything else verified true at HEAD: single shared event channel (app.rs:468, runtime.rs:1685); bump_chat_to_top sole caller window.rs:1107; chat_list load_chats (465-480) dead; typing_previews never inserted (41/196/792); upsert_gm_chat_cache never bumps unread for existing chats (251-260); all three ChatReadOnOtherDevice producers call mark_chat_read_local first (3303/3378/5544); persist-before-event ordering holds on all WA paths (e.g. 4908 vs 4920, 3118 vs 3163).


## MUST-FIX LIST

- SendAudio (ui/runtime.rs:8056-8135): add persist_new_message + MessageReceived echo for the sent voice note; deleting bump_chat_to_top without this freezes the row on voice-note sends (also fixes the pre-existing bug that voice notes are never persisted).
- MultiSend (ui/runtime.rs:8189+): route self_msg through persist_new_message before the MessageReceived send, or broadcast sends stop updating rows.
- Group system messages (ui/runtime.rs:~4271-4279): route sys_msg through persist_new_message (unread already suppressed via is_system_message) or rows freeze on 'You added X' events.
- upsert_chat (ui/runtime.rs:1441-1549): add producer-side guards — never overwrite non-empty last_message with empty, never move timestamp backward — before deleting chat_list.rs:550-556; CreateGroup(6472-6491)/StartNewChat(7050-7069) currently clobber previews in state with ts=now + empty preview.
- Define ChatAdded as create-only on the consumer (ignore for existing rows): 6490/7068/9075 emit pre-upsert stale payloads and the gm boot pin-row/hydration/reseed sequence (gmessages_runtime.rs:604-635, 991) is non-monotonic; verbatim-applying ChatAdded regresses rows once add_chat guards are deleted.
- A5 conversion set corrected and completed: runtime.rs:2558 (incoming revoke), 2655 (incoming edit — NOT 2553-2558 as cited), 5132 (DeleteForEveryone), 6124 (DeleteForMe), 7788 (gm edit), 5382 (LoadChat).
- A6 ephemeral reaction override must cover incoming reactions (runtime.rs:2710) and gm reactions (gmessages_runtime.rs:2057), not just SendReaction (5966), before B strips the preview-set from the ReactionUpdated handler (window.rs:1293-1298).
- gm MarkRead (gmessages_runtime.rs:1808-1939): emit ChatRowChanged after the gm_chats.bin unread-clear (1895-1913), and B must KEEP reset_unread on ChatReadOnOtherDevice — emit_row is a no-op for gm: ids (gm rows are not in RuntimeState.chats, seeded from wa_chats.bin only at runtime.rs:1196).
- gm MarkUnread (gmessages_runtime.rs:1947-1980): persist unread=1 into gm_chats.bin and emit ChatRowChanged; with ChatMarkedUnread deleted and the WA inline block a no-op for gm ids, SMS mark-unread otherwise loses its badge entirely.
- gm send echoes (gmessages_runtime.rs:1588-1638, 1665-1711, ~2068-2160): persist + emit; merged-chat SMS sends carry the gm: pair id un-redirected (chat_view.rs:2715-2735) — apply merge_map and route merged ids via TouchChatSummary with the WA id or sent SMS never moves the visible merged row.
- SetActiveChat fan-out to gm must be applied inline in gm's select loop (its handle_command is tokio::spawn'd at gmessages_runtime.rs:1529 — reintroduces the rapid-switch race fixed WA-side at runtime.rs:2002-2010) and must reverse-map merged WA ids to conv ids for unread suppression.
- Document the relaxed invariant: for merged SMS, ChatRowChanged arrives AFTER MessageReceived (command-channel detour through spawned TouchChatSummary tasks); B must not assume row freshness at MessageReceived time.


## ACCEPTANCE TESTS

- Send a voice note in a WA chat: the row moves to top with a voice-note preview and correct time; after app restart the preview/timestamp survive (previously neither persisted).
- MultiSend a text to 3 chats: all 3 rows move to top with the sent text as preview; restart preserves them.
- Add a participant to a group: the group row shows the system-message preview and bumps (or, if explicitly descoped, the row provably does not regress to an older preview).
- StartNewChat with an existing contact that has a recent preview: the row keeps its preview and does not blank or spuriously jump; wa_chats.bin still holds the old preview after the upsert.
- React 👍 from ANOTHER device/peer to a chat's latest message: sidebar shows 'Reacted 👍' without persisting it; restart shows the underlying message text again. Repeat for a gm/SMS reaction.
- Send an SMS from a merged chat (WA row, SMS mode): the merged WA row bumps immediately with the SMS text, unread stays 0, and the state survives restart; wa_chats.bin was updated via the WA runtime (no direct disk write from the gm thread).
- Receive an SMS on a merged chat while viewing it (SetActiveChat fan-out): no unread bump on either the gm or WA side; while NOT viewing it: exactly +1 unread, once, including for two messages arriving in the same second (document expected count).
- Mark an SMS-only (gm:) chat unread from the context menu: badge appears, survives restart and a list_conversations reseed; then open it: badge clears, survives restart.
- Edit / delete-for-everyone / delete-for-me / clear on the LATEST message of a chat: row preview updates (edit text, '🚫 Message deleted', or empty) with NO timestamp/sort change; performing the same on an OLDER message leaves the preview untouched.
- Reconnect reseed storm (suspend/resume with 1000+ chats): read chats stay read, no event flood (verify ChatRowChanged count ≈ genuinely-changed chats via debug log), UI stays responsive during batch drain.
- Rapid A→B→A chat switching while SMS arrive for A: unread suppression tracks the FINAL active chat on both runtimes (no stale-active race from spawned handlers).
- Kill-switch regression: grep shows zero refs to ChatPreviewUpdated, ChatMarkedUnread, bump_chat_to_top, update_last_message, touch_wa_chat_preview after the cleanup commit; stealth peek (recent_messages) and typing indicators still work, fed by note_recent_message.