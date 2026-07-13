# Desktop update releases

The Linux desktop app uses signed, whole-binary updates from private GitHub
Releases. A downloaded binary is never executed until its Ed25519 signature,
declared size, and SHA-256 hash have all been verified.

## Channels

- `desktop-canary` is replaced automatically after every relevant push to
  `main` passes the protocol and desktop tests.
- `desktop-stable` is replaced only by a manual `Desktop auto-update release`
  workflow run with the `stable` input.
- Both releases retain fixed asset names so clients can follow a channel
  without discovering version-specific URLs.
- Each fixed channel tag is moved to the exact commit used for its current
  assets, preserving useful release and rollback metadata.

The app defaults to Stable. The Updates settings page can switch to Canary or
Manual, start a check immediately, and restart into a staged update.

## Signing

The GitHub Actions secret `WHATSAPP_UPDATE_SIGNING_KEY_PEM` contains the
Ed25519 private key. Its public half is embedded in `desktop/src/updater.rs`.
The private key must remain outside the repository and should be backed up in
the same secure location as the other deployment credentials.

To rotate the key, first ship an application build containing the new public
key through the old signing path. Only then replace the Actions secret.

## Private repository authentication

The updater checks credentials in this order:

1. `WHATSAPP_UPDATE_TOKEN`
2. `~/.config/whatsapp-desktop/update-token`
3. `gh auth token` for developer machines

A deployment token only needs read access to this repository. The token is not
stored in the application binary or update manifest. The token file should be
mode `0600`.

## Install and recovery

The updater downloads beside the installed executable, verifies it, and writes
`update-pending.json` in the application data directory. On restart it:

1. retains the current executable as `whatsapp-desktop.previous`;
2. installs and re-executes the staged build;
3. waits for a real WhatsApp `Connected` event before marking the build healthy.

If an unconfirmed build is launched again, the startup guard restores the
previous executable before GTK or the protocol runtime starts. Failed builds
are retained beside the executable for diagnosis.

## Publishing stable

After Canary has been exercised, run:

```bash
gh workflow run desktop-release.yml \
  --repo jibsta210/whatsapp-desktop \
  --ref main \
  -f channel=stable
```

The workflow runs the full relevant test suite, builds with a monotonically
increasing Actions run number, signs the manifest, and uploads both fixed-name
assets only after all earlier steps succeed.
