@AGENTS.md

## Git Workflow

The canonical source is `github` remote (`jibsta210/whatsapp-desktop`, private).

- **Start of session:** `git pull github main` before making any changes.
- **After changes:** Always commit, then `git push github main`.
- The `origin` remote is a separate server for other purposes — do NOT push to `origin`.
- Production binary installs to `~/.local/bin/whatsapp-desktop`. After building release, copy it there (kill the running process first if needed).
