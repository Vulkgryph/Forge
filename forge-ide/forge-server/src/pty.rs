//! PTY session management on the remote machine.

use std::io::Write;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

fn expand_home(path: &str) -> String {
    if path.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() { return path.replacen('~', &home, 1); }
        }
        #[cfg(unix)] {
            let uid = unsafe { libc::getuid() };
            let pw  = unsafe { libc::getpwuid(uid) };
            if !pw.is_null() {
                let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
                if let Ok(home) = dir.to_str() { return path.replacen('~', home, 1); }
            }
        }
    }
    path.to_string()
}

pub struct PtySession {
    master:  Box<dyn portable_pty::MasterPty + Send>,
    writer:  Box<dyn Write + Send>,
    pub cwd:  String,
    pub cols: u16,
    pub rows: u16,
}

impl PtySession {
    pub fn open(
        cols: u16,
        rows: u16,
        cwd:  &str,
        push: impl Fn(Vec<u8>) + Send + 'static,
    ) -> Result<Self, String> {
        // Resolve shell — $SHELL may not be set in SSH exec environment.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| {
            for candidate in ["/bin/bash", "/bin/sh", "/usr/bin/sh"] {
                if std::path::Path::new(candidate).exists() {
                    return candidate.to_string();
                }
            }
            "/bin/sh".to_string()
        });

        let pty  = NativePtySystem::default();
        let pair = pty.openpty(PtySize {
            rows, cols, pixel_width: 0, pixel_height: 0,
        }).map_err(|e| e.to_string())?;

        // Expand ~ in cwd; fall back to HOME or / if unavailable.
        let resolved_cwd = expand_home(cwd);
        let resolved_cwd = if std::path::Path::new(&resolved_cwd).is_dir() {
            resolved_cwd
        } else {
            std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
        };

        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(&resolved_cwd);
        cmd.env("TERM", "xterm-256color");
        pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;

        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n)  => push(buf[..n].to_vec()),
                }
            }
        });

        Ok(Self { master: pair.master, writer, cwd: resolved_cwd, cols, rows })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), String> {
        self.writer.write_all(data).map_err(|e| e.to_string())?;
        self.writer.flush().map_err(|e| e.to_string())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}
