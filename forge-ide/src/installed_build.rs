//! Resolve updates installed alongside a running Windows executable.

use std::path::PathBuf;
#[cfg(any(windows, test))]
use std::path::Path;

pub fn launch_executable() -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    #[cfg(windows)]
    return Ok(resolve(&current));
    #[cfg(not(windows))]
    Ok(current)
}

#[cfg(any(windows, test))]
fn resolve(current: &Path) -> PathBuf {
    resolve_active(current).unwrap_or_else(|| current.to_owned())
}

#[cfg(any(windows, test))]
fn resolve_active(current: &Path) -> Option<PathBuf> {
    if current.file_name()? != "forge-ide.exe" { return None; }
    let releases = current.parent()?.parent()?;
    if releases.file_name()? != "releases" { return None; }
    let root = releases.parent()?;
    let bytes = std::fs::read(root.join("install.json")).ok()?;
    // Windows PowerShell 5's Set-Content -Encoding UTF8 writes a BOM.
    let record: serde_json::Value = serde_json::from_slice(
        bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes),
    ).ok()?;
    let generation = PathBuf::from(record.get("binaries")?.as_str()?);
    let generation = generation.canonicalize().ok()?;
    let releases = releases.canonicalize().ok()?;
    // Only follow this installation's direct child generation. A dev checkout
    // or another install root must never be redirected by a stale record.
    if generation.parent()? != releases { return None; }
    let candidate = generation.join("forge-ide.exe");
    if !candidate.is_file() { return None; }
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_follows_active_generation_and_rejects_invalid_records() {
        let root = std::env::temp_dir().join(format!("forge-update-resolver-{}", std::process::id()));
        let old = root.join("releases/old/forge-ide.exe");
        let new = root.join("releases/new/forge-ide.exe");
        for path in [&old, &new] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        let record = root.join("install.json");
        let write_record = |directory: &Path| {
            std::fs::write(&record, serde_json::to_vec(&serde_json::json!({
                "binaries": directory,
            })).unwrap()).unwrap();
        };
        write_record(new.parent().unwrap());
        assert_eq!(resolve(&old), new.canonicalize().unwrap());
        assert_eq!(resolve(&new), new.canonicalize().unwrap());

        let mut powershell_record = b"\xef\xbb\xbf".to_vec();
        powershell_record.extend(std::fs::read(&record).unwrap());
        std::fs::write(&record, powershell_record).unwrap();
        assert_eq!(resolve(&old), new.canonicalize().unwrap());

        let dev = root.join("target/debug/forge-ide.exe");
        assert_eq!(resolve(&dev), dev);
        write_record(&root);
        assert_eq!(resolve(&old), old);
        write_record(&root.join("releases/missing"));
        assert_eq!(resolve(&old), old);
        std::fs::write(&record, b"unfinished json").unwrap();
        assert_eq!(resolve(&old), old);
        std::fs::remove_file(&record).unwrap();
        assert_eq!(resolve(&old), old);
        // All paths above are fixed children of this uniquely named fixture.
        assert_eq!(root.canonicalize().unwrap().parent().unwrap(), std::env::temp_dir().canonicalize().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}
