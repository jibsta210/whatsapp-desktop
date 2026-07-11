# Fix Plan — Proactive contact resolution (phone→LID prewarm sweep)

**Status: PLAN ONLY — execution by Opus. Standing rules identical to FIXPLAN-READSYNC.md
(tag `pre-proactive-lid` first; one packet = one commit; push `github main` only; build with
`cargo build --release -p whatsapp-desktop`; kill+cp install pre-authorized; changelog
entries with revert lines).**

User directive: "ensure we resolve contacts ASAP going forward." Diagnosed + adversarially
verified at HEAD.

## Root gap (verified)

EVERY LID↔phone mapping write is **reactive** — it needs a live incoming message
(`src/message.rs:276/340/389`), a server device-notification
(`src/handlers/notification.rs:326/906`), or a usync response
(`src/usync.rs:41`). There is **no proactive/bulk population job** (the
`MigrationSync*`/`Blocklist*` LearningSources are defined but never emitted).

The three desktop "resolvers" (`resolve_lid_batch` runtime.rs:2016, `resolve_all_lids`
runtime.rs:8727, the LoadChat sweep runtime.rs:5383) all feed **`@lid` JIDs** into
`get_user_devices`, but `wacore/src/iq/usync.rs:565` extracts the `<lid>` child **only when
`user_jid.server == DEFAULT_USER_SERVER`** (a PHONE query). So a LID query resolves nothing —
runtime.rs:2058 even logs "usync device query cannot map lid→pn". A saved contact never
messaged (or messaged before LD-A landed — the user's `88077045383286` case) has its mapping
learned from **nowhere**, so a phantom "+<lid digits>" chat persists.

**The lever:** usync the SAVED CONTACTS' **phone numbers** (`@s.whatsapp.net`) at startup.
That hits the working `usync.rs:565` branch, and `src/usync.rs:41` persists the result
**bidirectionally** via `add_lid_pn_mapping(LearningSource::Usync)`. Pre-warms phone↔LID for
the whole phonebook before any message.

**Honest scope (verified):**
- FUTURE phantoms for never-messaged saved contacts → **prevented** (the unique win; reactive
  learning can't reach them).
- The user's existing `88077045383286` phantom → **heals IFF** that peer is a saved contact
  (repro says yes — "saved Canadian contact") AND `88077045383286` is still their CURRENT
  server LID (likely — it came off a recent live echo, not a stale one). If WhatsApp has
  ROTATED the peer's LID since, usync returns the new LID, the phantom's old LID stays
  unmatched, and only a fresh message heals it. `lid_pn_cache` stores a single current LID,
  no rotation history.

## Packet PL-A — startup phone→LID prewarm sweep
**Files:** desktop/src/contacts.rs, desktop/src/ui/runtime.rs, (optional) src/client/lid_pn.rs

1. **Input** — `desktop/src/contacts.rs`, add near `resolve_lid_to_phone`:
   ```rust
   /// Every saved contact's phone-digits key (real named contacts only, never
   /// @lid-keyed phantom rows). Input for the proactive phone→LID usync prewarm.
   pub fn saved_contact_phones(&self) -> Vec<String> {
       let inner = match self.inner.read() { Ok(g) => g, Err(e) => e.into_inner() };
       inner.by_digits.iter()
           .filter(|(d, e)| !e.is_lid && d.len() >= 7 && e.name.chars().any(|c| c.is_alphabetic()))
           .map(|(d, _)| d.clone())
           .collect()
   }
   ```

2. **Sweep** — `desktop/src/ui/runtime.rs`, hooked into the Connected background task
   IMMEDIATELY BEFORE the existing `merge_lid_chats` call (~runtime.rs:2254), so
   freshly-learned mappings collapse existing phantoms the same launch:
   - Gate on a new `RuntimeState.did_phone_lid_sweep: bool` (add near `connect_count`,
     ~runtime.rs:2168; init false) so reconnects don't re-sweep.
   - Skip already-cached: for each phone digits `d`, `if client.lid_pn_cache.get_current_lid(&d).await.is_none()` push `Jid::pn(d)` (or parse `<d>@s.whatsapp.net`). After the first
     session this shrinks to near-zero.
   - Chunk + throttle: `for chunk in phones.chunks(50) { client.get_user_devices(chunk).await?; sleep(300ms); }`, bail after ~40 chunks (2000 contacts) to bound connect cost.
     `get_user_devices` already de-dups against the device cache (src/usync.rs:18-24) and
     persists mappings (src/usync.rs:41).
   - Mirror the newly-resolved mappings into the desktop sources merge reads: after each
     chunk, for each phone now resolved (`client.lid_pn_cache.get_current_lid(&d)`), build the
     lid JID and `state.lock().insert_lid_phone(lid_jid, phone_jid)` (runtime.rs:1296) AND
     `contacts::global().record_lid_jid(phone_jid, &lid_jid)` (contacts.rs:324 — populates the
     `by_lid` index that `merge_lid_chats` reads). Then `save_lid_phone_map(&map)` +
     `contacts::global().save_if_dirty()`.
   - Log a one-line summary ("phone→LID prewarm: usynced N contacts, learned M mappings").

3. **Heal existing phantoms**: the existing `merge_lid_chats` call right after the sweep now
   finds the freshly-learned mappings (core cache + by_lid) and collapses any phantom whose
   contact is in the phonebook and whose LID matches. No new merge logic needed.

4. **(Optional) parity helper** — `src/client/lid_pn.rs`: add
   `ensure_phone_number_to_lid_mapping(&self, phone: &str)` that checks
   `lid_pn_cache.get_current_lid` and on miss issues the phone usync (mirrors WA Web's
   `WAWebManagePhoneNumberMappingJob`); call it from the sweep and before establishing new
   sessions. Keeps the miss→usync fallback in one place.

### Guardrails (verified sufficient)
- Once per session (flag); skip-cached (shrinks to ~0 after first run); chunk 50 + 300ms
  throttle + hard cap; off the hot path (runs in the post-connect background task, behind the
  existing ~5s delay so it doesn't compete with history/app-state sync). No ban/battery risk
  at this rate — it mirrors WA Web's own phone-number-mapping job.

### Verification
- Unit: `saved_contact_phones` filtering (excludes is_lid rows + digit-only names).
- Live: (a) fresh boot → log shows the prewarm usyncing contacts and learning mappings;
  (b) the `88077045383286` phantom collapses into the named contact chat at that boot IF its
  LID is current (check `app.log` for `Merging 88077045383286@lid → …`); (c) later, message a
  never-before-messaged saved contact from the phone → it lands in the named chat with no
  phantom (previously would have created one).
- Honest: if `88077045383286` does NOT merge after the sweep, its LID has rotated — report
  that plainly; a fresh message remains the only heal.
