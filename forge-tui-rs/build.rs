use std::process::Command;

/// Stamp the binary with the commit it was built from.
///
/// A version number cannot answer "am I running the build I just made" — every
/// build between two releases says `0.3.0`, which is exactly the situation while
/// a change is being tested. The commit can, so `/version` reports it alongside
/// the binary's own build time.
///
/// There is deliberately no `+dirty` marker. A build script re-runs only when
/// something it watches changes, and it cannot watch "the working tree is
/// clean" — so editing a file after a clean build would leave the stamp
/// claiming clean while the binary contains uncommitted work. A marker that is
/// right most of the time is worse here than none, because the whole purpose is
/// to be trusted. The build time, which is the file's own mtime and never
/// stale, is what distinguishes two builds of the same commit.
///
/// Everything here degrades rather than fails: a source tarball with no `.git`,
/// or a machine with no `git` on PATH, still builds and simply reports an
/// unknown commit.
fn main() {
    // Rebuild when HEAD moves. `.git/HEAD` changes on checkout; the ref file it
    // points at changes on commit, so both are watched — without the second, a
    // new commit on the same branch would keep the stale stamp.
    if let Some(git_dir) = locate_git_dir() {
        println!("cargo:rerun-if-changed={}/HEAD", git_dir.display());
        if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
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
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}
