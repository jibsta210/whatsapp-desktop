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
