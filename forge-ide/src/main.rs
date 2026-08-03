mod agent_panel;
mod app;
mod buffer;
mod dap;
#[cfg(target_os = "macos")]
mod dock_menu;
#[cfg(feature = "vulkan-renderer")]
mod egui_pass;
mod filetree;
mod filewatch;
#[cfg(feature = "vulkan-renderer")]
mod gfx;
#[cfg(not(feature = "vulkan-renderer"))]
mod gfx_wgpu;
mod git;
mod icons;
mod lsp;
mod onboarding;
mod plugin;
mod fmt;
mod ptyhost;
mod session;
mod settings;
mod ssh;
mod tasks;
mod theme;
mod update_check;
mod wake;
pub use app::OutputLevel;
mod markdown;
mod terminal;

use std::sync::Arc;

use app::{IdeApp, NewWindowSpec};
#[cfg(feature = "vulkan-renderer")]
use egui_pass::{EguiPass, SharedEguiPass};
#[cfg(feature = "vulkan-renderer")]
use gfx::{GfxContext, SharedGfx};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};
#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

/// Decide which windows to open at startup.
///
/// Paths on the command line always win: that is `forge-ide <path>`, and it is
/// also how "Reload Window" rebuilds the set, one path per window.
///
/// With no paths this is a genuine launch, and the windows that were last open
/// are reopened. Before that, startup fell through to a single window with no
/// folder and every other window the user had was silently lost.
///
/// `remembered` is a closure so the file is not read when the setting is off, and
/// so tests can supply a set without touching the user's configuration.
fn plan_initial_windows(
    cwds:            Vec<std::path::PathBuf>,
    remembered:      impl FnOnce() -> Vec<session::WindowRecord>,
    restore_windows: bool,
    is_reload:       bool,
) -> Vec<NewWindowSpec> {
    if !cwds.is_empty() {
        return cwds
            .into_iter()
            .map(|cwd| NewWindowSpec { cwd: Some(cwd), ssh_host: None, is_reload })
            .collect();
    }

    let usable: Vec<NewWindowSpec> = if restore_windows {
        remembered()
            .into_iter()
            // A folder that has since been deleted or moved is dropped rather
            // than opening a window onto a path that is not there.
            .filter(|r| r.cwd.as_ref().is_none_or(|p| p.is_dir()))
            .map(|r| NewWindowSpec { cwd: r.cwd, ssh_host: None, is_reload })
            .collect()
    } else {
        Vec::new()
    };

    if usable.is_empty() {
        vec![NewWindowSpec { cwd: None, ssh_host: None, is_reload }]
    } else {
        usable
    }
}

/// Whatever the active backend needs to share process-wide across windows.
#[cfg(feature = "vulkan-renderer")]
type SharedRenderer = (SharedGfx, SharedEguiPass);
#[cfg(not(feature = "vulkan-renderer"))]
type SharedRenderer = Arc<gfx_wgpu::SharedWgpu>;

// ── Per-window state ──────────────────────────────────────────────────────────

struct IdeWindow {
    id:     WindowId,
    window: Arc<Window>,
    #[cfg(feature = "vulkan-renderer")]
    gfx:    GfxContext,
    #[cfg(feature = "vulkan-renderer")]
    egui:   EguiPass,
    #[cfg(not(feature = "vulkan-renderer"))]
    egui:   gfx_wgpu::WgpuPass,
    app:    IdeApp,
    /// Closed by the user but not yet removed from the vec.
    closing: bool,
    /// Earliest time anything asked to be repainted again, captured via
    /// `egui::Context::set_request_repaint_callback` — every
    /// `request_repaint`/`request_repaint_after` call (app code or egui's
    /// own internals, e.g. a blinking text cursor) funnels through here.
    /// Reset before each frame and read back right after, so it reflects
    /// only *this* frame's requests. Drives `about_to_wait`'s
    /// `ControlFlow::WaitUntil` — see its doc comment for why this exists.
    next_repaint: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
}

impl IdeWindow {
    /// `shared` holds the process-wide Vulkan instance/device and egui
    /// pipeline once the first window has created them — every window after
    /// the first reuses them instead of paying MoltenVK's instance/device
    /// creation and shader-compile cost again (see `gfx::SharedGfx` and
    /// `egui_pass::SharedEguiPass`).
    fn create(
        event_loop: &ActiveEventLoop,
        spec: NewWindowSpec,
        shared: &mut Option<SharedRenderer>,
    ) -> Option<Self> {
        let window = event_loop.create_window(
            Window::default_attributes()
                .with_title("Forge IDE")
                .with_active(true)
                .with_inner_size(winit::dpi::LogicalSize::new(1400u32, 900u32)),
        ).ok()?;
        window.focus_window();
        let window = Arc::new(window);

        #[cfg(feature = "vulkan-renderer")]
        let (gfx, egui) = match shared {
            Some((shared_gfx, shared_egui)) => {
                let gfx = match GfxContext::new_shared(shared_gfx, &window) {
                    Ok(g) => g,
                    Err(e) => { eprintln!("Vulkan init: {e}"); return None; }
                };
                let egui = match EguiPass::new(
                    shared_egui, gfx.instance(), gfx.physical(), gfx.device(), &window,
                ) {
                    Ok(e) => e,
                    Err(e) => { eprintln!("EguiPass init: {e}"); return None; }
                };
                (gfx, egui)
            }
            None => {
                let (shared_gfx, gfx) = match GfxContext::new(&window) {
                    Ok(x) => x,
                    Err(e) => { eprintln!("Vulkan init: {e}"); return None; }
                };
                let shared_egui = match SharedEguiPass::new(&shared_gfx, gfx.render_pass) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("EguiPass pipeline init: {e}"); return None; }
                };
                let egui = match EguiPass::new(
                    &shared_egui, gfx.instance(), gfx.physical(), gfx.device(), &window,
                ) {
                    Ok(e) => e,
                    Err(e) => { eprintln!("EguiPass init: {e}"); return None; }
                };
                *shared = Some((shared_gfx, shared_egui));
                (gfx, egui)
            }
        };

        #[cfg(not(feature = "vulkan-renderer"))]
        let egui = match gfx_wgpu::WgpuPass::new(shared, Arc::clone(&window)) {
            Ok(p)  => p,
            Err(e) => { eprintln!("wgpu init: {e}"); return None; }
        };

        let id  = window.id();
        let app = IdeApp::new_with_spec(spec);

        let next_repaint: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        {
            let next_repaint = next_repaint.clone();
            egui.ctx.set_request_repaint_callback(move |info: egui::RequestRepaintInfo| {
                let at = std::time::Instant::now() + info.delay;
                let mut slot = next_repaint.lock().unwrap();
                *slot = Some(match *slot {
                    Some(existing) => existing.min(at),
                    None => at,
                });
            });
        }

        #[cfg(feature = "vulkan-renderer")]
        return Some(IdeWindow { id, window, gfx, egui, app, closing: false, next_repaint });
        #[cfg(not(feature = "vulkan-renderer"))]
        return Some(IdeWindow { id, window, egui, app, closing: false, next_repaint });
    }

    /// Renders one frame and returns the earliest time (if any) this frame's
    /// `draw()` asked to be repainted again.
    fn render(&mut self) -> Option<std::time::Instant> {
        *self.next_repaint.lock().unwrap() = None;

        #[cfg(feature = "vulkan-renderer")]
        {
            if self.gfx.is_dirty() { self.gfx.rebuild(&self.window); }

            let raw_input   = self.egui.winit.take_egui_input(&self.window);
            let full_output = self.egui.ctx.run(raw_input, |ctx| self.app.draw(ctx));
            self.egui.winit.handle_platform_output(
                &self.window, full_output.platform_output.clone());

            if !full_output.textures_delta.is_empty() {
                let _ = self.egui.update_textures(
                    self.gfx.instance(), self.gfx.physical(), self.gfx.device(),
                    self.gfx.queue(), self.gfx.command_pool,
                    full_output.textures_delta,
                );
            }

            let primitives = self.egui.ctx.tessellate(
                full_output.shapes, full_output.pixels_per_point);

            if let Some((cmd, img_idx, fi)) = self.gfx.begin_frame() {
                self.egui.record(
                    self.gfx.device(), cmd, fi, self.gfx.extent,
                    &primitives, full_output.pixels_per_point);
                self.gfx.end_frame(cmd, img_idx);
            }
        }

        #[cfg(not(feature = "vulkan-renderer"))]
        {
            let raw_input   = self.egui.winit.take_egui_input(&self.window);
            let full_output = self.egui.ctx.run(raw_input, |ctx| self.app.draw(ctx));
            self.egui.winit.handle_platform_output(
                &self.window, full_output.platform_output.clone());
            let ppp = full_output.pixels_per_point;
            let window = Arc::clone(&self.window);
            self.egui.present(&window, full_output, ppp);
        }

        self.next_repaint.lock().unwrap().take()
    }
}

// ── Application ───────────────────────────────────────────────────────────────

struct Ide {
    windows:      Vec<IdeWindow>,
    /// Specs queued by app code; created in about_to_wait.
    pending:      Vec<NewWindowSpec>,
    /// Every window to create at startup — one entry per CLI path argument
    /// (`forge-ide <path>`, VS Code's `code .` equivalent), or from a
    /// restart-in-place re-invoking itself with every window that had been
    /// open (see `reload_window`'s doc comment on why that's the full list,
    /// not just the one that triggered the reload). Always has at least one
    /// entry (a folderless window) so a plain `forge-ide` with no arguments
    /// still opens something.
    initial_specs: Vec<NewWindowSpec>,
    /// The process-wide Vulkan instance/device + egui pipeline, populated by
    /// the first window created and reused by every window after it.
    shared: Option<SharedRenderer>,
    /// Set whenever a real window event arrives (`window_event`) or a new
    /// window is queued — cleared once that's actually been rendered.
    /// `about_to_wait` only does the expensive egui-layout-plus-Vulkan-submit
    /// work when this is true or `next_deadline` has passed; see its doc
    /// comment for why that distinction turned out to matter.
    dirty: bool,
    /// The `WaitUntil` deadline `about_to_wait` most recently asked the event
    /// loop for. Needed to tell a *genuine* due wakeup apart from one of the
    /// many spurious extra times the OS/runloop calls `about_to_wait` well
    /// before that deadline for unrelated reasons.
    next_deadline: Option<std::time::Instant>,
}

impl Ide {
    fn new(initial_specs: Vec<NewWindowSpec>) -> Self {
        Self {
            windows: Vec::new(), pending: Vec::new(), initial_specs, shared: None,
            dirty: false, next_deadline: None,
        }
    }

    fn find_mut(&mut self, id: WindowId) -> Option<&mut IdeWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }
}

impl ApplicationHandler for Ide {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Right-click-the-Dock-icon menu. Installed here rather than in `main`
        // because it needs a live event loop (and, underneath, a main-thread
        // marker) — see `dock_menu`.
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::ActiveEventLoopExtMacOS;
            use objc2_foundation::MainThreadMarker;
            if let Some(mtm) = MainThreadMarker::new() {
                event_loop.set_dock_menu(dock_menu::build(mtm));
            }
        }

        if !self.windows.is_empty() { return; }
        // `about_to_wait` recomputes this every iteration based on what the
        // just-rendered frame actually asked for (see its doc comment) —
        // `Wait` here is just a safe idle default for the sliver of time
        // before the first render happens.
        event_loop.set_control_flow(ControlFlow::Wait);

        let mut specs = std::mem::take(&mut self.initial_specs).into_iter();
        if let Some(spec) = specs.next() {
            if let Some(win) = IdeWindow::create(event_loop, spec, &mut self.shared) {
                self.windows.push(win);
                self.remember_windows();
            }
        }
        // Every window after the first goes through the normal `pending`
        // queue — `about_to_wait` picks them up on the very next iteration
        // (see the `!self.pending.is_empty()` check at the bottom).
        self.pending.extend(specs);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Set before the early-return below (egui consuming an event, e.g.
        // most real keyboard/mouse input, returns early) — a consumed event
        // still means something needs re-rendering just as much as one that
        // wasn't.
        self.dirty = true;
        let Some(win) = self.find_mut(id) else { return };

        // Forward to egui first
        let resp = win.egui.winit.on_window_event(&*win.window, &event);
        if resp.consumed { return; }

        match event {
            WindowEvent::CloseRequested => {
                win.app.save_session();
                // `EguiPass` (descriptor pool, per-frame vertex/index
                // buffers, every uploaded texture) has no `Drop` impl —
                // only the process-wide `SharedGfx`/`SharedEguiPass` do.
                // Without this, closing a window while others (and the
                // process) stay open would leak its Vulkan resources for
                // as long as the process keeps running, since the device
                // itself stays alive via the other windows' `Arc` refs.
                #[cfg(feature = "vulkan-renderer")]
                win.egui.destroy(win.gfx.device());
                win.closing = true;
            }
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    #[cfg(feature = "vulkan-renderer")]
                    win.gfx.mark_dirty();
                    #[cfg(not(feature = "vulkan-renderer"))]
                    win.egui.mark_dirty();
                }
            }
            // A window can relocate to a different screen (different backing/color space,
            // e.g. moving onto a CGVirtualDisplay-backed screen) without any Resized event
            // firing at all, since the pixel dimensions don't have to change. Nothing else
            // forces a swapchain rebuild in that case, and MoltenVK doesn't reliably report
            // ERROR_OUT_OF_DATE/SUBOPTIMAL for it either — so the surface can keep presenting
            // stale content indefinitely. Treat both as swapchain-dirty too.
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. } => {
                #[cfg(feature = "vulkan-renderer")]
                win.gfx.mark_dirty();
                #[cfg(not(feature = "vulkan-renderer"))]
                win.egui.mark_dirty();
            }
            _ => {}
        }

        // Remove closed windows; exit if none left
        let before = self.windows.len();
        self.windows.retain(|w| !w.closing);
        if self.windows.len() != before {
            self.remember_windows();
        }
        if self.windows.is_empty() { event_loop.exit(); }
    }

    /// Renders every window once, then puts the event loop back to sleep
    /// until the *earliest* moment any window's frame actually asked to be
    /// woken up again (`ControlFlow::WaitUntil`), or fully idle
    /// (`ControlFlow::Wait`) if nothing did.
    ///
    /// This used to be `ControlFlow::Poll` — the event loop looping and
    /// re-rendering as fast as the OS would schedule it, unconditionally,
    /// forever, even sitting completely idle with the window unfocused.
    /// That's the dominant reason the IDE burned a full CPU core (and
    /// correspondingly, battery/energy) at all times: every render() call
    /// re-runs egui's full layout pass and re-tessellates and re-submits a
    /// GPU frame, and Poll was doing that thousands of times a second for
    /// no reason. Real input (typing, clicking, resizing) always wakes the
    /// loop immediately regardless of Wait/WaitUntil — winit delivers those
    /// as actual events, not through this polling mechanism — so
    /// interactive responsiveness is unaffected. What *does* need explicit
    /// handling is anything that updates without direct user input this
    /// frame (streaming agent tokens, terminal output, a blinking cursor,
    /// a background git/SSH task finishing) — those now schedule their own
    /// next-wake time via `ctx.request_repaint_after(..)` (see the
    /// consolidated check in `IdeApp::draw` and the few per-feature ones
    /// already alongside their own animations), captured here through
    /// `IdeWindow::next_repaint`.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Dock-menu clicks land on a queue rather than calling in directly: the
        // click arrives on the main thread inside AppKit, which has no handle on
        // the state that owns the window list. Drained here, just before the
        // pending-window pass below picks it up.
        #[cfg(target_os = "macos")]
        for req in dock_menu::take_requests() {
            match req {
                // Same as "New Window" inside the app — `cwd: None` inherits the
                // process working directory, matching Cmd+N / the menu item.
                dock_menu::DockRequest::NewWindow => {
                    self.pending.push(NewWindowSpec::default());
                    self.dirty = true;
                }
            }
        }

        // Create any windows queued by app code
        let specs: Vec<NewWindowSpec> = std::mem::take(&mut self.pending);
        if !specs.is_empty() { self.dirty = true; }
        for spec in specs {
            if let Some(win) = IdeWindow::create(event_loop, spec, &mut self.shared) {
                self.windows.push(win);
                self.remember_windows();
            }
        }

        // Most `about_to_wait` calls are *not* because our own requested
        // `WaitUntil` deadline actually elapsed — the OS/runloop invokes it
        // for all sorts of unrelated reasons far more often than that
        // (measured ~100+ calls/sec at complete idle, vs. the ~3/sec our
        // own 300ms baseline alone would ask for). Rendering unconditionally
        // on every one of those, as this used to, meant a full egui layout
        // + Vulkan submit per spurious wakeup regardless of whether anything
        // could possibly have changed. Only do that work when a real window
        // event happened since the last render (`dirty`) or our own
        // previously-requested deadline has actually passed.
        //
        // `None` means nothing asked to be repainted, so nothing is due — this
        // must not be `map_or(true, ..)`. That inverted default was harmless
        // only while a baseline repaint guaranteed a deadline every frame; once
        // a genuinely idle window schedules nothing, it turned every spurious
        // wakeup into a rendered frame (measured: 10% of a core, worse than the
        // polling it replaced).
        let deadline_due = self.next_deadline
            .is_some_and(|t| std::time::Instant::now() >= t);
        if !self.dirty && !deadline_due {
            event_loop.set_control_flow(match self.next_deadline {
                Some(t) => ControlFlow::WaitUntil(t),
                None    => ControlFlow::Wait,
            });
            return;
        }
        self.dirty = false;

        // Render each window, harvest any new-window/reload requests, and
        // track the earliest repaint any of them asked for.
        let mut new_specs: Vec<NewWindowSpec> = Vec::new();
        let mut reload_requested = false;
        let mut earliest: Option<std::time::Instant> = None;
        for win in &mut self.windows {
            if let Some(t) = win.render() {
                earliest = Some(earliest.map_or(t, |e: std::time::Instant| e.min(t)));
            }
            if let Some(spec) = win.app.pending_new_window.take() {
                new_specs.push(spec);
            }
            if std::mem::take(&mut win.app.pending_reload) {
                reload_requested = true;
            }
        }
        self.pending.extend(new_specs);

        // "Reload Window" spawns a genuinely new process (new PID) and
        // exits this one — every window's Vulkan/Metal/window-server
        // state, and the shared instance/device/pipeline, is torn down
        // explicitly *here* first so it's a clean handoff instead of
        // leaving the old state dangling for the OS to notice and reclaim
        // on its own schedule.
        //
        // This used to use `exec` on unix to stay truly in-place (same
        // PID), but that broke multi-window restore: a re-exec'd process's
        // `resumed()` never got far enough to drain the second window's
        // spec from `pending` at all (no panic, no error — the run loop
        // just never called back again after the very first window).
        // Best-supported explanation is that macOS AppKit ties a lot of
        // state to a PID's *first* `NSApplication` registration, and a
        // second one spun up in-place under a re-used PID leaves the run
        // loop unable to properly pump events for anything beyond that
        // first window. Spawning a genuinely new process sidesteps that
        // entirely — confirmed this reliably restores every window, not
        // just the first, where `exec` did not.
        //
        // Every currently open window's folder (not just the one that
        // triggered the reload) has to be passed along, since the new
        // process starts from nothing and has no other way to know what
        // else was open.
        if reload_requested {
            let exe = std::env::current_exe().unwrap_or_default();
            if exe.as_os_str().is_empty() || !exe.exists() {
                eprintln!("reload_window: couldn't resolve current_exe, staying open");
            } else {
                let cwds: Vec<std::path::PathBuf> =
                    self.windows.iter().map(|w| w.app.cwd().to_path_buf()).collect();
                // `reload_window()` already unconditionally saved *its own*
                // window's session — but a reload tears down every open
                // window, not just the one that triggered it, so every
                // other one needs the same unconditional save here or its
                // conversation/open-files state never reaches disk at all.
                for win in &self.windows {
                    win.app.save_session_for_reload();
                }
                self.windows.clear();
                self.shared = None;
                if std::process::Command::new(&exe).arg("--reload").args(&cwds).spawn().is_ok() {
                    std::process::exit(0);
                }
                eprintln!("reload_window: spawn failed after teardown");
                if self.windows.is_empty() { event_loop.exit(); }
            }
        }

        // If there are pending windows to create, we need another event loop
        // iteration right away to actually create them.
        if !self.pending.is_empty() {
            earliest = Some(std::time::Instant::now());
        }

        self.next_deadline = earliest;
        event_loop.set_control_flow(match earliest {
            Some(t) => ControlFlow::WaitUntil(t),
            None    => ControlFlow::Wait,
        });
    }

    /// A background thread called `wake::wake()`. Nothing to inspect — the
    /// point is simply that the loop is awake now; marking dirty makes
    /// `about_to_wait` render one frame, which polls every channel as usual.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        self.dirty = true;
    }

    /// Last chance to persist state before the process goes away.
    ///
    /// `WindowEvent::CloseRequested` only fires from AppKit's
    /// `windowShouldClose:` — i.e. the window's own close button. Quitting the
    /// application (⌘Q, the Dock menu, "Quit Forge IDE") goes through
    /// `applicationWillTerminate:` instead, which never touches that path, so
    /// every session was silently dropped on the most common way to quit.
    /// winit surfaces that as `LoopExiting`, which is this hook.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        for win in &mut self.windows {
            win.app.save_session();
        }
    }
}

fn main() {
    // Only the optional Vulkan renderer needs MoltenVK, and it is no longer
    // bundled — the host must provide it (Homebrew's molten-vk, or the Vulkan
    // SDK). The default wgpu build talks to Metal directly and needs none of
    // this.
    #[cfg(all(target_os = "macos", feature = "vulkan-renderer"))]
    {
        use std::ffi::OsStr;
        if std::env::var_os("DYLD_LIBRARY_PATH").is_none() {
            for c in ["/usr/local/lib", "/opt/homebrew/lib", "/usr/lib"] {
                if std::path::Path::new(c).join("libMoltenVK.dylib").exists() {
                    unsafe { std::env::set_var("DYLD_LIBRARY_PATH", OsStr::new(c)); }
                    break;
                }
            }
        }
    }

    // `forge-ide <path>` opens that path as the workspace (`code .`
    // equivalent) — also how "Reload Window" re-opens every workspace that
    // was open across every window before it restarted (one path argument
    // per window; see `reload_window`'s doc comment), since a fresh process
    // otherwise has no way to know which folders the old one had open.
    // `--reload` additionally marks that this process was just launched by
    // Reload Window (not a genuinely fresh start), so it should always
    // restore the session it just saved regardless of the `restore_session`
    // setting.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let is_reload = args.iter().any(|a| a == "--reload");
    let cwds: Vec<std::path::PathBuf> = args.iter()
        .filter(|a| *a != "--reload")
        .map(std::path::PathBuf::from)
        .collect();
    let initial_specs = plan_initial_windows(
        cwds,
        || session::load_windows(),
        settings::load().restore_windows,
        is_reload,
    );

    let mut builder = EventLoop::builder();
    #[cfg(target_os = "macos")]
    builder
        .with_activation_policy(ActivationPolicy::Regular)
        .with_activate_ignoring_other_apps(true);
    let event_loop = builder.build().expect("event loop");

    // Let background threads wake the loop. Without this, the loop sleeping in
    // `ControlFlow::Wait` would never notice an `mpsc` send, which is why the
    // app used to poll unconditionally at 300ms — see `wake` and
    // `IdeApp::draw`'s repaint tiers.
    let proxy = event_loop.create_proxy();
    wake::set_waker(move || { let _ = proxy.send_event(()); });

    event_loop.run_app(&mut Ide::new(initial_specs)).expect("run");
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::path::PathBuf;

    fn record(cwd: Option<&str>) -> session::WindowRecord {
        session::WindowRecord { cwd: cwd.map(PathBuf::from) }
    }

    /// The reported bug: a restart opened one window and lost the rest. Every
    /// remembered window must come back, not just the first.
    #[test]
    fn every_remembered_window_is_reopened() {
        // Real directories, since a missing folder is deliberately dropped.
        let a = std::env::temp_dir();
        let b = std::env::temp_dir().join("..");
        let specs = plan_initial_windows(
            Vec::new(),
            || vec![record(a.to_str()), record(b.to_str()), record(None)],
            true,
            false,
        );
        assert_eq!(specs.len(), 3, "all three, not one: {specs:?}");
        assert!(specs.iter().any(|s| s.cwd.is_none()), "including the folderless one");
    }

    /// With the toggle off, startup goes back to a single empty window.
    #[test]
    fn the_toggle_off_opens_one_empty_window() {
        let specs = plan_initial_windows(
            Vec::new(),
            || vec![record(std::env::temp_dir().to_str()), record(None)],
            false,
            false,
        );
        assert_eq!(specs.len(), 1);
        assert!(specs[0].cwd.is_none(), "and it has no folder");
    }

    /// The file must not even be read when the toggle is off.
    #[test]
    fn nothing_is_loaded_when_the_toggle_is_off() {
        let mut read = false;
        let _ = plan_initial_windows(
            Vec::new(),
            || {
                read = true;
                Vec::new()
            },
            false,
            false,
        );
        assert!(!read, "the window list was read despite the toggle being off");
    }

    /// Paths on the command line always win — that is `forge-ide <path>`, and it
    /// is how Reload Window rebuilds the set.
    #[test]
    fn command_line_paths_take_precedence() {
        let specs = plan_initial_windows(
            vec![PathBuf::from("/one"), PathBuf::from("/two")],
            || panic!("the remembered set must not be consulted"),
            true,
            true,
        );
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].cwd, Some(PathBuf::from("/one")));
        assert!(specs.iter().all(|s| s.is_reload), "the reload flag is carried");
    }

    /// A folder deleted since last time must not open a window onto a path that
    /// is not there.
    #[test]
    fn folders_that_no_longer_exist_are_dropped() {
        let real = std::env::temp_dir();
        let specs = plan_initial_windows(
            Vec::new(),
            || vec![record(Some("/definitely/not/a/real/path")), record(real.to_str())],
            true,
            false,
        );
        assert_eq!(specs.len(), 1, "only the one that still exists: {specs:?}");
        assert_eq!(specs[0].cwd, Some(real));
    }

    /// And if nothing usable remains, it falls back rather than opening nothing —
    /// an empty spec list would exit the event loop immediately.
    #[test]
    fn an_unusable_set_falls_back_to_one_window() {
        for remembered in [
            Vec::new(),
            vec![record(Some("/gone/one")), record(Some("/gone/two"))],
        ] {
            let specs = plan_initial_windows(Vec::new(), || remembered.clone(), true, false);
            assert_eq!(specs.len(), 1, "never zero windows");
            assert!(specs[0].cwd.is_none());
        }
    }

    /// A folderless window stays folderless rather than inheriting a directory.
    #[test]
    fn a_folderless_record_reopens_without_a_folder() {
        let specs = plan_initial_windows(Vec::new(), || vec![record(None)], true, false);
        assert_eq!(specs.len(), 1);
        assert!(specs[0].cwd.is_none());
    }
}
