//! Plugin system — dynamically loaded `.so`/`.dylib`/`.dll` plugins from
//! `~/.config/forge-ide/plugins/`.
//!
//! Plugins expose a minimal C ABI:
//!
//! ```c
//! // Plugin display name (static string, not freed).
//! const char* forge_plugin_name(void);
//! // JSON array of commands: [{"id":"upper","title":"Uppercase Buffer"}]
//! const char* forge_plugin_commands(void);
//! // Run a command against the current buffer text. Returns a newly
//! // allocated replacement text, or NULL to leave the buffer unchanged.
//! char* forge_plugin_run(const char* id, const char* text);
//! // Free a pointer previously returned by forge_plugin_run.
//! void forge_plugin_free(char* ptr);
//! ```
//!
//! Commands appear in the command palette as "Plugin: <title>".

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;

use libloading::{Library, Symbol};

pub struct PluginCommand {
    pub id:     String,
    pub title:  String,
    pub plugin: usize, // index into PluginHost::plugins
}

pub struct Plugin {
    // Not yet surfaced in any UI (no "installed plugins" list exists);
    // collected at load time since it's the natural key for one.
    #[allow(dead_code)]
    pub name: String,
    lib:      Library,
}

#[derive(Default)]
pub struct PluginHost {
    pub plugins:  Vec<Plugin>,
    pub commands: Vec<PluginCommand>,
}

fn plugins_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("forge-ide")
        .join("plugins")
}

impl PluginHost {
    /// Scan the plugins directory and load every dynamic library found.
    pub fn load() -> Self {
        let mut host = Self::default();
        let Ok(iter) = std::fs::read_dir(plugins_dir()) else { return host };
        for entry in iter.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "so" | "dylib" | "dll") { continue; }
            // SAFETY: plugins are user-installed native code; loading them is
            // inherently trusted, same as VS Code native extensions.
            let Ok(lib) = (unsafe { Library::new(&path) }) else { continue };
            let name = unsafe {
                match lib.get::<Symbol<unsafe extern "C" fn() -> *const c_char>>(b"forge_plugin_name") {
                    Ok(f) => {
                        let p = f();
                        if p.is_null() { continue; }
                        CStr::from_ptr(p).to_string_lossy().into_owned()
                    }
                    Err(_) => continue,
                }
            };
            let commands_json = unsafe {
                match lib.get::<Symbol<unsafe extern "C" fn() -> *const c_char>>(b"forge_plugin_commands") {
                    Ok(f) => {
                        let p = f();
                        if p.is_null() { String::new() }
                        else { CStr::from_ptr(p).to_string_lossy().into_owned() }
                    }
                    Err(_) => String::new(),
                }
            };
            let idx = host.plugins.len();
            if let Ok(cmds) = serde_json::from_str::<Vec<serde_json::Value>>(&commands_json) {
                for c in cmds {
                    let (Some(id), Some(title)) = (
                        c.get("id").and_then(|v| v.as_str()),
                        c.get("title").and_then(|v| v.as_str()),
                    ) else { continue };
                    host.commands.push(PluginCommand {
                        id:     id.to_string(),
                        title:  title.to_string(),
                        plugin: idx,
                    });
                }
            }
            host.plugins.push(Plugin { name, lib });
        }
        host
    }

    /// Run a command against `text`. Returns the replacement text, if any.
    pub fn run(&self, cmd: &PluginCommand, text: &str) -> Option<String> {
        let plugin = self.plugins.get(cmd.plugin)?;
        let id_c   = CString::new(cmd.id.as_str()).ok()?;
        let text_c = CString::new(text).ok()?;
        unsafe {
            let run: Symbol<unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char> =
                plugin.lib.get(b"forge_plugin_run").ok()?;
            let out = run(id_c.as_ptr(), text_c.as_ptr());
            if out.is_null() { return None; }
            let result = CStr::from_ptr(out).to_string_lossy().into_owned();
            if let Ok(free) = plugin.lib
                .get::<Symbol<unsafe extern "C" fn(*mut c_char)>>(b"forge_plugin_free") {
                free(out);
            }
            Some(result)
        }
    }
}
