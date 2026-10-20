// Embeds the commit time of the source being built, so the updater can tell a
// local build that is ahead of a published release from one that is behind it.
fn main() {
    let ts = std::process::Command::new("git")
        .args(["log", "-1", "--format=%ct"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".into());
    println!("cargo:rustc-env=SOURCE_COMMIT_UNIX={ts}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");
}
