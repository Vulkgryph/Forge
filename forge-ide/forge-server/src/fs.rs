use std::fs;
use forge_proto::FsEntry;

pub fn list(path: &str) -> Result<Vec<FsEntry>, String> {
    let expanded = expand_home(path);
    let rd = fs::read_dir(&expanded).map_err(|e| format!("{expanded}: {e}"))?;
    let mut entries: Vec<FsEntry> = rd.filter_map(|e| {
        let e    = e.ok()?;
        let meta = e.metadata().ok()?;
        let name = e.file_name().to_string_lossy().to_string();
        // Include all files — hidden files are common on server home dirs.
        // The client can filter if needed.
        Some(FsEntry {
            path:   e.path().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size:   meta.len(),
            name,
        })
    }).collect();
    entries.sort_by(|a, b|
        b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

pub fn read(path: &str) -> Result<String, String> {
    let expanded = expand_home(path);
    fs::read_to_string(&expanded).map_err(|e| format!("{expanded}: {e}"))
}

pub fn write(path: &str, text: &str) -> Result<(), String> {
    let expanded = expand_home(path);
    if let Some(parent) = std::path::Path::new(&expanded).parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&expanded, text).map_err(|e| format!("{expanded}: {e}"))
}

/// Create a directory, and any parent it needs.
///
/// `write` already creates the parents of a file, but a folder someone wants to
/// put files in later has no file to hang off — and asking for one to be made is
/// the ordinary thing to want from a file tree.
///
/// Refuses when something is already there, rather than reporting success for a
/// directory it did not create. `create_dir_all` is happy to do nothing, which
/// would make a typo look like it worked.
pub fn mkdir(path: &str) -> Result<(), String> {
    let expanded = expand_home(path);
    if std::path::Path::new(&expanded).exists() {
        return Err(format!("{expanded}: already exists"));
    }
    fs::create_dir_all(&expanded).map_err(|e| format!("{expanded}: {e}"))
}

fn expand_home(path: &str) -> String {
    if path.starts_with('~') {
        // Try $HOME first, then fall back to /etc/passwd via getpwuid.
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return path.replacen('~', &home, 1);
            }
        }
        // SSH exec channels sometimes don't set $HOME — derive it from uid.
        #[cfg(unix)]
        {
            let uid = unsafe { libc::getuid() };
            let pw  = unsafe { libc::getpwuid(uid) };
            if !pw.is_null() {
                let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
                if let Ok(home) = dir.to_str() {
                    return path.replacen('~', home, 1);
                }
            }
        }
    }
    path.to_string()
}
