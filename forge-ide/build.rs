use std::fs;

fn main() {
    emit_forge_server_version();
    emit_forge_agent_version();
    emit_build_commit();

    /// The same reasoning as `emit_forge_server_version`, for the agent. The IDE
/// uploads a Linux build of forge-agent to a remote host so the agent can run
/// where the work is, and needs its version to know whether the copy already
/// there is current. forge-agent is a sibling crate this one deliberately does
/// not link against, so its Cargo.toml is where the number lives.
fn emit_forge_agent_version() {
    let path = "../forge-agent/Cargo.toml";
    println!("cargo:rerun-if-changed={path}");
    let text = fs::read_to_string(path).expect("failed to read forge-agent/Cargo.toml");
    let value: toml::Value = text.parse().expect("failed to parse forge-agent/Cargo.toml");
    let version = value["package"]["version"]
        .as_str()
        .expect("forge-agent/Cargo.toml missing package.version");
    println!("cargo:rustc-env=FORGE_AGENT_VERSION={version}");
}

}

/// `ssh.rs` needs forge-server's version to know whether the copy already on
/// a remote host is current, without depending on the forge-server crate
/// itself (a separate, cross-compiled-to-Linux binary, not something this
/// client links against). Reading its Cargo.toml here — instead of hand-
/// copying the version string into ssh.rs, which then has to be remembered
/// on every forge-server bump — makes forge-server/Cargo.toml the one
/// place that number actually lives.
fn emit_forge_server_version() {
    let path = "forge-server/Cargo.toml";
    println!("cargo:rerun-if-changed={path}");
    let text = fs::read_to_string(path).expect("failed to read forge-server/Cargo.toml");
    let value: toml::Value = text.parse().expect("failed to parse forge-server/Cargo.toml");
    let version = value["package"]["version"]
        .as_str()
        .expect("forge-server/Cargo.toml missing package.version");
    println!("cargo:rustc-env=FORGE_SERVER_VERSION={version}");
}

/// Stamp the binary with the commit it was built from.
///
/// The same reasoning as the terminal client's, and the same implementation:
/// a version number cannot answer "am I running the build I just made", since
/// every build between two releases reports the same number. That is exactly
/// the situation while a change is being tested.
///
/// Degrades rather than fails — a source tarball with no `.git`, or a machine
/// with no `git`, still builds and reports an unknown commit.
fn emit_build_commit() {
    // Rebuild when HEAD moves. `.git/HEAD` changes on checkout; the ref it
    // points at changes on commit, so both are watched — without the second, a
    // new commit on the same branch would keep the stale stamp.
    if let Some(git_dir) = locate_git_dir() {
        println!("cargo:rerun-if-changed={}/HEAD", git_dir.display());
        if let Ok(head) = fs::read_to_string(git_dir.join("HEAD")) {
            if let Some(reference) = head.strip_prefix("ref: ").map(str::trim) {
                println!("cargo:rerun-if-changed={}/{reference}", git_dir.display());
            }
        }
    }
    let commit = git(&["rev-parse", "--short=9", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=FORGE_BUILD_COMMIT={commit}");
}

fn locate_git_dir() -> Option<std::path::PathBuf> {
    git(&["rev-parse", "--absolute-git-dir"]).map(std::path::PathBuf::from)
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
