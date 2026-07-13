//! Signed whole-application updater for the desktop binary.
//!
//! Updates are downloaded in the background, verified with an embedded
//! Ed25519 public key, and staged beside the installed executable. They are
//! applied before GTK starts on the next launch. The previous executable is
//! retained until the replacement reaches a real WhatsApp `Connected` event;
//! relaunching an unconfirmed build automatically rolls back first.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bridge::WaEvent;

const MANIFEST_SCHEMA: u32 = 1;
const PLATFORM: &str = "linux-x86_64";
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_BINARY_BYTES: u64 = 300 * 1024 * 1024;
const PENDING_FILE: &str = "update-pending.json";
const HEALTH_FILE: &str = "update-health.json";
const REEXEC_ENV: &str = "WHATSAPP_UPDATE_REEXEC";

// Public half of ~/.config/whatsapp-desktop/update-signing-key.pem. The
// private key never belongs in the repository or application binary.
const UPDATE_PUBLIC_KEY_B64: &str = "u2/HnJ5x8zz+8wa+Bf1pSFUukZGgyr+jJHztyc+liGI=";

const GITHUB_REPOSITORY: &str = "jibsta210/whatsapp-desktop";
const MANIFEST_ASSET: &str = "whatsapp-desktop-linux-x86_64.json";
const BINARY_ASSET: &str = "whatsapp-desktop-linux-x86_64";

static CHECK_RUNNING: AtomicBool = AtomicBool::new(false);
static STATUS: LazyLock<Mutex<UpdateStatus>> =
    LazyLock::new(|| Mutex::new(UpdateStatus::default()));
static GITHUB_TOKEN: LazyLock<Option<String>> = LazyLock::new(resolve_github_token);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub schema: u32,
    pub channel: String,
    pub platform: String,
    pub version: String,
    pub build: u64,
    pub published_at: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingUpdate {
    manifest: UpdateManifest,
    staged_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HealthPhase {
    AwaitingHealth,
    Healthy,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HealthState {
    phase: HealthPhase,
    build: u64,
    version: String,
    target_path: PathBuf,
    previous_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct UpdateStatus {
    pub message: String,
    pub checking: bool,
    pub ready: bool,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            message: "Updates are checked automatically".into(),
            checking: false,
            ready: false,
        }
    }
}

pub fn status() -> UpdateStatus {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_status(message: impl Into<String>, checking: bool, ready: bool) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = UpdateStatus {
        message: message.into(),
        checking,
        ready,
    };
}

pub fn current_version_label() -> String {
    let build = current_build();
    if build == 0 {
        format!("{} (local build)", env!("CARGO_PKG_VERSION"))
    } else {
        format!("{} (build {build})", env!("CARGO_PKG_VERSION"))
    }
}

fn current_build() -> u64 {
    option_env!("WHATSAPP_BUILD_ID")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn release_tag(channel: &str) -> &'static str {
    match channel {
        "canary" => "desktop-canary",
        _ => "desktop-stable",
    }
}

fn canonical_payload(manifest: &UpdateManifest) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        manifest.schema,
        manifest.channel,
        manifest.platform,
        manifest.version,
        manifest.build,
        manifest.published_at,
        manifest.url,
        manifest.sha256,
        manifest.size
    )
}

fn verify_manifest_with_key(
    manifest: &UpdateManifest,
    public_key: &[u8; 32],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.schema == MANIFEST_SCHEMA,
        "unsupported update manifest schema"
    );
    anyhow::ensure!(
        manifest.platform == PLATFORM,
        "update is for a different platform"
    );
    anyhow::ensure!(
        manifest.url.starts_with("https://"),
        "update URL must use HTTPS"
    );
    anyhow::ensure!(
        manifest.size > 0 && manifest.size <= MAX_BINARY_BYTES,
        "invalid update size"
    );
    anyhow::ensure!(
        manifest.sha256.len() == 64 && manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid update SHA-256"
    );
    Version::parse(&manifest.version)?;

    let signature = base64::engine::general_purpose::STANDARD
        .decode(&manifest.signature)
        .map_err(|e| anyhow::anyhow!("invalid update signature encoding: {e}"))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|e| anyhow::anyhow!("invalid update signature: {e}"))?;
    let key = VerifyingKey::from_bytes(public_key)
        .map_err(|e| anyhow::anyhow!("invalid embedded update key: {e}"))?;
    key.verify_strict(canonical_payload(manifest).as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("update signature verification failed"))
}

fn verify_manifest(manifest: &UpdateManifest) -> anyhow::Result<()> {
    let key = base64::engine::general_purpose::STANDARD
        .decode(UPDATE_PUBLIC_KEY_B64)
        .map_err(|e| anyhow::anyhow!("embedded update key is invalid: {e}"))?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| anyhow::anyhow!("embedded update key has the wrong length"))?;
    verify_manifest_with_key(manifest, &key)
}

fn is_newer(manifest: &UpdateManifest) -> anyhow::Result<bool> {
    let available = Version::parse(&manifest.version)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    Ok(available > current || (available == current && manifest.build > current_build()))
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .user_agent(&format!(
            "whatsapp-desktop/{} updater",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
}

/// Resolve a private GitHub release credential without ever compiling one into
/// the binary. A deployment can provide a read-only token via environment or a
/// 0600 config file. Developer machines may reuse the GitHub CLI keyring.
fn resolve_github_token() -> Option<String> {
    if let Ok(token) = std::env::var("WHATSAPP_UPDATE_TOKEN")
        && !token.trim().is_empty()
    {
        return Some(token.trim().to_string());
    }
    let token_file = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/whatsapp-desktop/update-token"));
    if let Some(path) = token_file
        && let Ok(token) = fs::read_to_string(path)
        && !token.trim().is_empty()
    {
        return Some(token.trim().to_string());
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    (!token.trim().is_empty()).then(|| token.trim().to_string())
}

fn github_call(url: &str, accept: &str) -> anyhow::Result<ureq::Response> {
    let agent = http_agent();
    let mut request = agent
        .get(url)
        .set("Accept", accept)
        .set("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = GITHUB_TOKEN.as_deref() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    request.call().map_err(|error| {
        anyhow::anyhow!(
            "private update feed request failed: {error}. Configure WHATSAPP_UPDATE_TOKEN or ~/.config/whatsapp-desktop/update-token with read-only repository access"
        )
    })
}

fn github_asset_url(channel: &str, asset_name: &str) -> anyhow::Result<String> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_REPOSITORY}/releases/tags/{}",
        release_tag(channel)
    );
    let response = github_call(&url, "application/vnd.github+json")?;
    let release: serde_json::Value = response.into_json()?;
    release
        .get("assets")
        .and_then(|assets| assets.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                (asset.get("name")?.as_str()? == asset_name)
                    .then(|| asset.get("url")?.as_str().map(str::to_string))
                    .flatten()
            })
        })
        .ok_or_else(|| {
            anyhow::anyhow!("release {} has no {asset_name} asset", release_tag(channel))
        })
}

fn github_asset_response(channel: &str, asset_name: &str) -> anyhow::Result<ureq::Response> {
    let url = github_asset_url(channel, asset_name)?;
    github_call(&url, "application/octet-stream")
}

fn fetch_manifest(channel: &str) -> anyhow::Result<UpdateManifest> {
    let response = if let Ok(url) = std::env::var("WHATSAPP_UPDATE_MANIFEST_URL") {
        http_agent().get(&url).call()?
    } else {
        github_asset_response(channel, MANIFEST_ASSET)?
    };
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "update manifest is too large"
    );
    let manifest: UpdateManifest = serde_json::from_slice(&bytes)?;
    verify_manifest(&manifest)?;
    anyhow::ensure!(manifest.channel == channel, "update channel mismatch");
    Ok(manifest)
}

fn sha256_file(path: &Path) -> anyhow::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        anyhow::ensure!(
            size <= MAX_BINARY_BYTES,
            "downloaded update exceeds size limit"
        );
        hasher.update(&buffer[..read]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn atomic_json_write(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn stage_update(data_dir: &Path, manifest: &UpdateManifest) -> anyhow::Result<PathBuf> {
    let target = std::env::current_exe()?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("installed executable has no parent directory"))?;
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("installed executable name is invalid"))?;
    let partial = parent.join(format!(".{name}.update-{}.partial", manifest.build));
    let staged = parent.join(format!(".{name}.update-{}", manifest.build));
    let _ = fs::remove_file(&partial);
    let _ = fs::remove_file(&staged);

    let response = if manifest
        .url
        .contains("github.com/jibsta210/whatsapp-desktop/releases/")
    {
        github_asset_response(&manifest.channel, BINARY_ASSET)?
    } else {
        http_agent().get(&manifest.url).call()?
    };
    let mut reader = response.into_reader().take(MAX_BINARY_BYTES + 1);
    let mut output = File::create(&partial)?;
    std::io::copy(&mut reader, &mut output)?;
    output.sync_all()?;

    let (actual_hash, actual_size) = sha256_file(&partial)?;
    anyhow::ensure!(
        actual_size == manifest.size,
        "downloaded update size mismatch"
    );
    anyhow::ensure!(
        actual_hash.eq_ignore_ascii_case(&manifest.sha256),
        "downloaded update hash mismatch"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&partial, &staged)?;

    atomic_json_write(
        &data_dir.join(PENDING_FILE),
        &PendingUpdate {
            manifest: manifest.clone(),
            staged_path: staged.clone(),
        },
    )?;
    Ok(staged)
}

/// Check and stage an update on a worker thread. `force` controls whether a
/// no-update/failure result is surfaced to the user as a toast.
pub fn check_for_updates(
    channel: String,
    force: bool,
    notifications: Option<async_channel::Sender<WaEvent>>,
) {
    if CHECK_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    set_status("Checking for updates…", true, false);
    std::thread::Builder::new()
        .name("app-updater".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<Option<UpdateManifest>> {
                let data_dir = std::env::current_dir()?;
                let manifest = fetch_manifest(&channel)?;
                if !is_newer(&manifest)? {
                    return Ok(None);
                }
                stage_update(&data_dir, &manifest)?;
                Ok(Some(manifest))
            })();

            match result {
                Ok(Some(manifest)) => {
                    let text = format!(
                        "Version {} is ready and will install on restart",
                        manifest.version
                    );
                    set_status(&text, false, true);
                    if let Some(tx) = &notifications {
                        let _ = tx.try_send(WaEvent::InfoToast(text));
                    }
                }
                Ok(None) => {
                    let text = "You’re running the latest version";
                    set_status(text, false, false);
                    if force && let Some(tx) = &notifications {
                        let _ = tx.try_send(WaEvent::InfoToast(text.into()));
                    }
                }
                Err(error) => {
                    log::warn!("Update check failed: {error:#}");
                    let text = format!("Update check failed: {error}");
                    set_status(&text, false, false);
                    if force && let Some(tx) = &notifications {
                        let _ = tx.try_send(WaEvent::ErrorToast(text));
                    }
                }
            }
            CHECK_RUNNING.store(false, Ordering::Release);
        })
        .ok();
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn previous_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("whatsapp-desktop");
    target.with_file_name(format!("{name}.previous"))
}

fn install_staged_binary(target: &Path, staged: &Path, previous: &Path) -> anyhow::Result<()> {
    let _ = fs::remove_file(previous);
    fs::rename(target, previous)
        .map_err(|e| anyhow::anyhow!("cannot retain previous executable: {e}"))?;
    if let Err(error) = fs::rename(staged, target) {
        let restore_error = fs::rename(previous, target).err();
        return Err(match restore_error {
            Some(restore_error) => anyhow::anyhow!(
                "cannot install staged update: {error}; restoring the current executable also failed: {restore_error}"
            ),
            None => anyhow::anyhow!("cannot install staged update: {error}"),
        });
    }
    Ok(())
}

fn restore_previous_binary(target: &Path, previous: &Path, failed: &Path) -> anyhow::Result<()> {
    let _ = fs::remove_file(failed);
    fs::rename(target, failed)
        .map_err(|e| anyhow::anyhow!("cannot retain failed executable: {e}"))?;
    if let Err(error) = fs::rename(previous, target) {
        let restore_error = fs::rename(failed, target).err();
        return Err(match restore_error {
            Some(restore_error) => anyhow::anyhow!(
                "cannot restore previous executable: {error}; restoring the failed executable also failed: {restore_error}"
            ),
            None => anyhow::anyhow!("cannot restore previous executable: {error}"),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn exec_replacement(path: &Path, reexec: bool) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(path);
    if reexec {
        command.env(REEXEC_ENV, "1");
    } else {
        command.env_remove(REEXEC_ENV);
    }
    command.exec()
}

/// Apply a pending update or recover an unconfirmed update. This function may
/// replace the current process image and therefore never return on success.
pub fn handle_startup_update(data_dir: &Path) {
    #[cfg(not(unix))]
    let _ = data_dir;

    #[cfg(unix)]
    {
        let health_path = data_dir.join(HEALTH_FILE);
        let pending_path = data_dir.join(PENDING_FILE);
        let is_reexec = std::env::var_os(REEXEC_ENV).is_some();

        if !is_reexec
            && let Some(mut health) = read_json::<HealthState>(&health_path)
            && health.phase == HealthPhase::AwaitingHealth
            && health.previous_path.exists()
        {
            log::warn!(
                "Update {} never reached healthy state; restoring {}",
                health.version,
                health.previous_path.display()
            );
            let failed = health
                .target_path
                .with_file_name(format!("whatsapp-desktop.failed-{}", health.build));
            if restore_previous_binary(&health.target_path, &health.previous_path, &failed).is_ok()
            {
                health.phase = HealthPhase::RolledBack;
                let _ = atomic_json_write(&health_path, &health);
                let error = exec_replacement(&health.target_path, false);
                eprintln!("failed to exec rolled-back application: {error}");
            }
        }

        if is_reexec {
            return;
        }

        let Some(pending) = read_json::<PendingUpdate>(&pending_path) else {
            return;
        };
        if let Err(error) = verify_manifest(&pending.manifest) {
            log::warn!("Discarding invalid pending update: {error:#}");
            let _ = fs::remove_file(&pending_path);
            let _ = fs::remove_file(&pending.staged_path);
            return;
        }
        let Ok((hash, size)) = sha256_file(&pending.staged_path) else {
            let _ = fs::remove_file(&pending_path);
            return;
        };
        if size != pending.manifest.size || !hash.eq_ignore_ascii_case(&pending.manifest.sha256) {
            log::warn!("Discarding pending update whose binary no longer matches its manifest");
            let _ = fs::remove_file(&pending_path);
            let _ = fs::remove_file(&pending.staged_path);
            return;
        }

        let Ok(target) = std::env::current_exe() else {
            return;
        };
        let previous = previous_path(&target);
        if let Err(error) = install_staged_binary(&target, &pending.staged_path, &previous) {
            log::warn!("Cannot install staged update: {error:#}");
            return;
        }

        let health = HealthState {
            phase: HealthPhase::AwaitingHealth,
            build: pending.manifest.build,
            version: pending.manifest.version,
            target_path: target.clone(),
            previous_path: previous.clone(),
        };
        if let Err(error) = atomic_json_write(&health_path, &health) {
            log::warn!("Cannot persist update health state: {error}");
            let failed = target.with_extension("update-failed");
            let _ = restore_previous_binary(&target, &previous, &failed);
            return;
        }
        let _ = fs::remove_file(&pending_path);
        let error = exec_replacement(&target, true);
        eprintln!("failed to exec installed update: {error}");
        let failed = target.with_extension("update-failed");
        let _ = restore_previous_binary(&target, &previous, &failed);
    }
}

/// Confirm an installed update only after the protocol reached Connected.
pub fn mark_healthy() {
    let Ok(data_dir) = std::env::current_dir() else {
        return;
    };
    let path = data_dir.join(HEALTH_FILE);
    let Some(mut health) = read_json::<HealthState>(&path) else {
        return;
    };
    if health.phase != HealthPhase::AwaitingHealth {
        return;
    }
    health.phase = HealthPhase::Healthy;
    if let Err(error) = atomic_json_write(&path, &health) {
        log::warn!("Failed to mark update healthy: {error}");
    } else {
        log::info!(
            "Update {} build {} marked healthy after WhatsApp connection",
            health.version,
            health.build
        );
    }
}

/// Restart through the user service manager so a pending update can be applied
/// before the next GTK process starts.
pub fn restart_to_apply() -> anyhow::Result<()> {
    let data_dir = std::env::current_dir()?;
    anyhow::ensure!(data_dir.join(PENDING_FILE).exists(), "no update is ready");
    let exe = std::env::current_exe()?;
    let unit = format!("whatsapp-update-relaunch-{}", std::process::id());
    let mut command = std::process::Command::new("systemd-run");
    command.args([
        "--user",
        "--collect",
        &format!("--unit={unit}"),
        "--on-active=1s",
        "/usr/bin/env",
    ]);
    if std::env::var("GMESSAGES_ENABLE").as_deref() == Ok("1") {
        command.arg("GMESSAGES_ENABLE=1");
    }
    let status = command.arg(exe).status()?;
    anyhow::ensure!(status.success(), "failed to schedule application restart");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let id = TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("whatsapp-updater-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn manifest() -> UpdateManifest {
        UpdateManifest {
            schema: 1,
            channel: "stable".into(),
            platform: PLATFORM.into(),
            version: "9.8.7".into(),
            build: 42,
            published_at: "2026-07-13T00:00:00Z".into(),
            url: "https://example.invalid/whatsapp-desktop".into(),
            sha256: "ab".repeat(32),
            size: 1234,
            signature: String::new(),
        }
    }

    #[test]
    fn signed_manifest_round_trip_and_tamper_rejection() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let mut manifest = manifest();
        manifest.signature = base64::engine::general_purpose::STANDARD.encode(
            signing
                .sign(canonical_payload(&manifest).as_bytes())
                .to_bytes(),
        );
        verify_manifest_with_key(&manifest, signing.verifying_key().as_bytes()).unwrap();

        manifest.build += 1;
        assert!(verify_manifest_with_key(&manifest, signing.verifying_key().as_bytes()).is_err());
    }

    #[test]
    fn canonical_payload_is_stable_and_excludes_signature() {
        let a = canonical_payload(&manifest());
        let mut changed_signature = manifest();
        changed_signature.signature = "anything".into();
        assert_eq!(a, canonical_payload(&changed_signature));
        assert_eq!(a.lines().count(), 9);
    }

    #[test]
    fn previous_binary_sits_beside_target() {
        assert_eq!(
            previous_path(Path::new("/home/test/.local/bin/whatsapp-desktop")),
            Path::new("/home/test/.local/bin/whatsapp-desktop.previous")
        );
    }

    #[test]
    fn install_and_rollback_preserve_known_good_binary() {
        let dir = test_dir();
        let target = dir.join("whatsapp-desktop");
        let staged = dir.join(".whatsapp-desktop.update-2");
        let previous = previous_path(&target);
        let failed = dir.join("whatsapp-desktop.failed-2");
        fs::write(&target, b"known-good").unwrap();
        fs::write(&staged, b"new-build").unwrap();

        install_staged_binary(&target, &staged, &previous).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new-build");
        assert_eq!(fs::read(&previous).unwrap(), b"known-good");

        restore_previous_binary(&target, &previous, &failed).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"known-good");
        assert_eq!(fs::read(&failed).unwrap(), b"new-build");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_install_restores_current_binary() {
        let dir = test_dir();
        let target = dir.join("whatsapp-desktop");
        let staged = dir.join("missing-update");
        let previous = previous_path(&target);
        fs::write(&target, b"known-good").unwrap();

        assert!(install_staged_binary(&target, &staged, &previous).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"known-good");
        assert!(!previous.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
