mod agent_panel;
mod app;
mod buffer;
mod dap;
#[cfg(target_os = "macos")]
mod dock_install;
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
mod model_proxy;
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

/// A rectangle in physical pixels — a monitor, or a window's frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect { x: i32, y: i32, w: u32, h: u32 }

/// How much of a restored window has to be visible for it to be usable: enough
/// of the top edge to grab and drag it somewhere better.
const MIN_VISIBLE: (i32, i32) = (160, 48);

/// The frame to reopen a window at, or `None` to let the platform choose.
///
/// A recorded frame can point somewhere that no longer exists — an external
/// display was unplugged, or the arrangement changed, or the same window is being
/// restored on a different machine. Honouring it blindly opens the window
/// offscreen, where there is nothing to click and no way to drag it back, so it
/// is only honoured while a usable piece of it still falls on some display.
///
/// With no monitors to check against (the platform does not report them) the
/// frame is used as-is rather than second-guessed.
fn onscreen_frame(frame: session::WindowFrame, monitors: &[Rect]) -> Option<session::WindowFrame> {
    if monitors.is_empty() {
        return Some(frame);
    }
    let win = Rect { x: frame.x, y: frame.y, w: frame.w, h: frame.h };
    monitors.iter().any(|m| overlap(&win, m)).then_some(frame)
}

fn overlap(a: &Rect, b: &Rect) -> bool {
    let ix = (a.x + a.w as i32).min(b.x + b.w as i32) - a.x.max(b.x);
    let iy = (a.y + a.h as i32).min(b.y + b.h as i32) - a.y.max(b.y);
    ix >= MIN_VISIBLE.0 && iy >= MIN_VISIBLE.1
}

/// How long a window has to stop moving before its new frame is written.
const GEOMETRY_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// Stands in for a window with no folder in the argument list a reload builds.
/// A bare flag rather than a path, since every path is a real workspace.
const NO_FOLDER: &str = "--no-folder";

/// Decide which windows to open at startup.
///
/// Three cases, in order:
///
/// * A **reload** re-opens exactly what was open, so it prefers the recorded set
///   over its own argument list. Both describe the same windows — the reload
///   writes the record immediately before spawning — but only the record carries
///   each window's position and size, which cannot travel through argv without
///   inventing a syntax for it. The arguments stay as the fallback for when the
///   record could not be written at all (no writable config directory), and are
///   used if the two disagree, since the live set is authoritative over a file
///   that may be stale.
/// * **Paths on the command line** — `forge-ide <path>` — open those folders.
/// * **A genuine launch** reopens the windows that were last open, subject to the
///   setting. Before that, startup fell through to a single folderless window and
///   every other window the user had was silently lost.
///
/// Each entry of `cwds` is one window, and `None` is a window with no folder —
/// which has to stay distinguishable from a real path all the way through. A
/// folderless window still *has* a working directory (`$HOME`, so its terminal
/// starts somewhere sensible), and passing that along instead would silently
/// promote it to a `$HOME` workspace on every reload.
///
/// `remembered` is a closure so tests can supply a set without touching the
/// user's configuration, and so it is not consulted at all on the paths that must
/// not be influenced by it.
fn parse_window_args(args: &[String]) -> (bool, Vec<Option<std::path::PathBuf>>) {
    let is_reload = args.iter().any(|a| a == "--reload");
    let cwds = args
        .iter()
        .filter(|a| *a != "--reload")
        .map(|a| (a != NO_FOLDER).then(|| std::path::PathBuf::from(a)))
        .collect();
    (is_reload, cwds)
}

fn plan_initial_windows(
    cwds:            Vec<Option<std::path::PathBuf>>,
    remembered:      impl FnOnce() -> Vec<session::WindowRecord>,
    restore_windows: bool,
    is_reload:       bool,
) -> Vec<NewWindowSpec> {
    // A folder that has since been deleted or moved is dropped rather than
    // opening a window onto a path that is not there.
    fn still_there(r: &session::WindowRecord) -> bool {
        r.cwd.as_ref().is_none_or(|p| p.is_dir())
    }
    fn from_records(rs: Vec<session::WindowRecord>, is_reload: bool) -> Vec<NewWindowSpec> {
        rs.into_iter()
            .filter(still_there)
            .map(|r| NewWindowSpec {
                cwd: r.cwd, frame: r.frame, maximized: r.maximized,
                window_id: r.id, ssh_host: None, is_reload, reload_count: 0,
            })
            .collect()
    }
    fn from_paths(cwds: Vec<Option<std::path::PathBuf>>, is_reload: bool) -> Vec<NewWindowSpec> {
        cwds.into_iter()
            .map(|cwd| NewWindowSpec { cwd, is_reload, ..Default::default() })
            .collect()
    }

    let usable: Vec<NewWindowSpec> = if is_reload {
        let records = remembered();
        // With no window arguments at all the record is the only information
        // there is, so it is used rather than compared against nothing. Treating
        // that as a disagreement — which it is not — dropped every window and
        // opened a single empty one instead.
        let agree_on_the_windows = cwds.is_empty()
            || (records.len() == cwds.len() && records.iter().zip(&cwds).all(|(r, c)| &r.cwd == c));
        if !records.is_empty() && agree_on_the_windows {
            from_records(records, is_reload)
        } else {
            from_paths(cwds, is_reload)
        }
    } else if !cwds.is_empty() {
        from_paths(cwds, is_reload)
    } else if restore_windows {
        from_records(remembered(), is_reload)
    } else {
        Vec::new()
    };

    if usable.is_empty() {
        vec![NewWindowSpec { is_reload, ..Default::default() }]
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
        let mut attrs = Window::default_attributes()
            .with_title("Forge IDE")
            .with_active(true)
            .with_inner_size(winit::dpi::LogicalSize::new(1400u32, 900u32));

        // Put a reopened window back where it was. `onscreen_frame` drops a frame
        // that no longer lands on any display, in which case the platform places
        // the window as it would a new one.
        let monitors: Vec<Rect> = event_loop
            .available_monitors()
            .map(|m| {
                let (p, s) = (m.position(), m.size());
                Rect { x: p.x, y: p.y, w: s.width, h: s.height }
            })
            .collect();
        let restore_to = spec.frame.and_then(|f| onscreen_frame(f, &monitors));
        if let Some(f) = restore_to {
            attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(f.w, f.h));
        }
        // After the frame, so the frame is what it un-zooms back to.
        if spec.maximized {
            attrs = attrs.with_maximized(true);
        }

        let window = event_loop.create_window(attrs).ok()?;
        // Positioned after creation rather than through `with_position`, which on
        // macOS places the *content* origin while `outer_position` reads back the
        // frame origin — restoring one from the other walked every window up the
        // screen by a title bar on each reload. `set_outer_position` is the same
        // coordinate that was recorded, on every platform.
        if let Some(f) = restore_to {
            window.set_outer_position(winit::dpi::PhysicalPosition::new(f.x, f.y));
        }
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
    /// The window set as last written to disk, so it is only rewritten when it
    /// actually changes.
    saved_windows: Vec<session::WindowRecord>,
    /// When the on-screen geometry first differed from what is on disk.
    ///
    /// Dragging or resizing a window produces a continuous stream of new frames,
    /// and writing each one would mean an fsync-and-rename per mouse-move. The
    /// write waits for the motion to settle; see `remember_windows`.
    geometry_dirty_since: Option<std::time::Instant>,
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
    /// Set once AppKit has been asked to terminate. `terminate:` is
    /// asynchronous, so `about_to_wait` keeps being called for a few turns
    /// afterwards; without this the reload block would spawn a second process.
    terminating: bool,
    /// The `WaitUntil` deadline `about_to_wait` most recently asked the event
    /// loop for. Needed to tell a *genuine* due wakeup apart from one of the
    /// many spurious extra times the OS/runloop calls `about_to_wait` well
    /// before that deadline for unrelated reasons.
    next_deadline: Option<std::time::Instant>,
}

impl Ide {
    fn new(initial_specs: Vec<NewWindowSpec>) -> Self {
        Self {
            windows: Vec::new(), saved_windows: Vec::new(), geometry_dirty_since: None,
            pending: Vec::new(), initial_specs, shared: None,
            dirty: false, next_deadline: None, terminating: false,
        }
    }

    fn find_mut(&mut self, id: WindowId) -> Option<&mut IdeWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// Write down which windows are open, so a restart can reopen them.
    ///
    /// Compares against what was last written and does nothing when unchanged,
    /// so this can be called every frame. That matters because the set changes
    /// for reasons other than a window opening or closing — "Open Folder" turns
    /// a folderless window into a workspace, and hooking only the add and remove
    /// paths meant the recorded set went stale the moment someone opened a
    /// folder in a window that already existed. Checking the value rather than
    /// the event cannot miss a path.
    fn remember_windows(&mut self) {
        self.write_windows(false);
    }

    /// Write the set immediately, debounce or no debounce. Used where there is no
    /// later chance to write: the process is exiting, or a reload is about to
    /// tear every window down.
    fn remember_windows_now(&mut self) {
        self.write_windows(true);
    }

    fn window_records(&self) -> Vec<session::WindowRecord> {
        self.windows
            .iter()
            .map(|w| session::WindowRecord {
                id:        w.app.window_id,
                cwd:       w.app.workspace_root(),
                // `outer_position` is unsupported on some platforms and fails
                // while a window is being created; no frame simply means the
                // platform picks one.
                frame:     w.window.outer_position().ok().map(|p| {
                    let size = w.window.inner_size();
                    session::WindowFrame { x: p.x, y: p.y, w: size.width, h: size.height }
                }),
                maximized: w.window.is_maximized(),
            })
            .collect()
    }

    fn write_windows(&mut self, force: bool) {
        let records = self.window_records();
        if records == self.saved_windows {
            self.geometry_dirty_since = None;
            return;
        }

        // Which windows are open, and what folder each has, is written the moment
        // it changes: those are deliberate acts, and each one could be the last
        // thing that happens before the process goes away. Geometry alone waits,
        // because it changes continuously while a window is dragged.
        let set_changed = records.len() != self.saved_windows.len()
            || records.iter().zip(&self.saved_windows).any(|(a, b)| a.cwd != b.cwd);
        if !force && !set_changed {
            let since = *self.geometry_dirty_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < GEOMETRY_SETTLE {
                return;
            }
        }

        session::save_windows(&records);
        self.saved_windows = records;
        self.geometry_dirty_since = None;
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
        self.windows.retain(|w| !w.closing);
        // Records the set whenever it differs from what is on disk, which covers
        // opening and closing a window *and* a folder being opened in one.
        self.remember_windows();
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
        // AppKit is tearing us down; do no further work (and above all, do not
        // spawn another reload process).
        if self.terminating { return; }



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
            // Rebuild just this window's app, keeping its OS window, swapchain
            // and egui context — everything the process restart below has to
            // tear down and rebuild for every window, and the reason that one
            // takes the other windows' sessions with it.
            //
            // Safe to do here, mid-loop, because nothing outside `win.app`
            // refers to it: the old value's `Drop` kills its agent child and
            // language servers, and the shells it was showing are the pty
            // daemon's, so the new app reattaches to the same ones by id.
            if std::mem::take(&mut win.app.pending_window_reload) {
                win.app = IdeApp::new_with_spec(win.app.reload_spec());
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
                // `workspace_root()`, not `cwd()`: the former is `None` for a
                // window with no folder open, while the latter is that window's
                // `$HOME` fallback. Passing `cwd()` here reopened folderless
                // windows as `$HOME` workspaces.
                let cwds: Vec<String> = self
                    .windows
                    .iter()
                    .map(|w| match w.app.workspace_root() {
                        Some(p) => p.to_string_lossy().into_owned(),
                        None => NO_FOLDER.to_string(),
                    })
                    .collect();
                // `reload_window()` already unconditionally saved *its own*
                // window's session — but a reload tears down every open
                // window, not just the one that triggered it, so every
                // other one needs the same unconditional save here or its
                // conversation/open-files state never reaches disk at all.
                for win in &self.windows {
                    win.app.save_session_for_reload();
                }
                // The record is what carries each window's geometry to the new
                // process (the argument list cannot), so it has to be written
                // while the windows are still here to measure.
                self.remember_windows_now();
                self.windows.clear();
                self.shared = None;
                if std::process::Command::new(&exe).arg("--reload").args(&cwds).spawn().is_ok() {
                    // Leave through winit rather than AppKit's `terminate:`.
                    //
                    // `terminate:` looked right — it runs the documented
                    // shutdown, including `applicationWillTerminate:` — but it
                    // cannot be called from in here. This is a winit callback,
                    // and winit dispatches callbacks through a handler it holds
                    // borrowed for the duration; `terminate:` spins a *nested*
                    // run loop (`_waitForPendingChangesToFinish`, saving the
                    // persistent-UI state) which pumps more events straight back
                    // into that handler, and winit panics on the re-entrancy by
                    // design. A panic crossing back into Objective-C aborts, so
                    // every reload ended as SIGABRT — which is exactly what
                    // macOS then reported as having quit unexpectedly. The crash
                    // report says so plainly: `terminate:` →
                    // `discardAllPersistentStateAndClose` →
                    // `_waitForPendingChangesToFinish` → our handler → abort.
                    //
                    // `exit()` asks the loop to stop between callbacks, so the
                    // teardown happens with nothing borrowed and the process
                    // leaves with status 0. It also calls `exiting` on the way
                    // out, which is what `terminate:` was reached for in the
                    // first place — and everything that has to reach disk was
                    // already written a few lines above, before the windows
                    // were torn down.
                    //
                    // Nothing is lost by not going through AppKit. `terminate:`
                    // was also once hoped to make macOS restore window
                    // placement, and with it each window's Space, by writing
                    // `~/Library/Saved Application State/`. It cannot: the
                    // windows are torn down a few lines above so there is
                    // nothing left to record, the new process starts before the
                    // old one would write anything, and Space assignment has no
                    // public API at all. Position and size are restored
                    // explicitly through the window record instead.
                    self.terminating = true;
                    event_loop.exit();
                    return;
                }
                eprintln!("reload_window: spawn failed after teardown");
                if self.windows.is_empty() {
                    // Same reason as the success path: the windows are already
                    // gone and already saved, so `exiting` must not write the
                    // empty list over the record.
                    self.terminating = true;
                    event_loop.exit();
                }
            }
        }

        // If there are pending windows to create, we need another event loop
        // iteration right away to actually create them.
        if !self.pending.is_empty() {
            earliest = Some(std::time::Instant::now());
        }

        // A window that was moved or resized has its frame written only once the
        // motion settles, and the last mouse-move is the last event there will
        // be — so the loop has to be woken again for that write to ever happen.
        self.remember_windows();
        if let Some(since) = self.geometry_dirty_since {
            let due = since + GEOMETRY_SETTLE;
            earliest = Some(earliest.map_or(due, |e: std::time::Instant| e.min(due)));
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
        // A reload has already saved everything and then emptied `windows` on
        // purpose. Running the save again from here would ask an empty list
        // where its windows are and write *that* — erasing the record the
        // process being started right now is about to read.
        if self.terminating { return; }

        // Before the sessions, while the windows still exist and can still be
        // asked where they are.
        self.remember_windows_now();
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
    let (is_reload, cwds) = parse_window_args(&args);
    // Session files for windows that are no longer in the record are dead weight.
    // Done here, once, because the recorded set is authoritative only before any
    // window exists — doing it while running would race the windows that have
    // been planned but not yet created.
    session::prune_window_sessions(&session::load_windows());

    // Read once rather than inside `plan_initial_windows`: the same set is what
    // says which stored window sessions are still wanted. Without this pass every
    // window ever opened would leave its session file behind for good.
    let remembered = session::load_windows();
    session::prune_window_sessions(&remembered);
    let initial_specs = plan_initial_windows(
        cwds,
        move || remembered,
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
        session::WindowRecord { cwd: cwd.map(PathBuf::from), ..Default::default() }
    }

    fn frame(x: i32, y: i32, w: u32, h: u32) -> session::WindowFrame {
        session::WindowFrame { x, y, w, h }
    }

    /// A 1440p main display with a second one to its right.
    fn two_monitors() -> Vec<Rect> {
        vec![
            Rect { x: 0,    y: 0, w: 2560, h: 1440 },
            Rect { x: 2560, y: 0, w: 1920, h: 1080 },
        ]
    }

    /// The ordinary case: a window comes back exactly where it was.
    #[test]
    fn a_frame_on_a_connected_display_is_honoured() {
        let f = frame(100, 80, 1400, 900);
        assert_eq!(onscreen_frame(f, &two_monitors()), Some(f));
        // Including on the second display.
        let f2 = frame(2600, 40, 1200, 800);
        assert_eq!(onscreen_frame(f2, &two_monitors()), Some(f2));
    }

    /// The failure this guards: a window saved on a display that is no longer
    /// there must not reopen offscreen, where it cannot be clicked or dragged.
    #[test]
    fn a_frame_on_a_vanished_display_is_dropped() {
        // Saved on the second monitor; only the first is connected now.
        let only_main = vec![Rect { x: 0, y: 0, w: 2560, h: 1440 }];
        assert_eq!(onscreen_frame(frame(3000, 200, 1200, 800), &only_main), None);
        // Or the arrangement moved: a window off the top-left corner.
        assert_eq!(onscreen_frame(frame(-1500, -1000, 1400, 900), &only_main), None);
    }

    /// A window mostly offscreen is still fine as long as enough of it can be
    /// grabbed — dropping those would relocate windows people deliberately parked
    /// half off the edge.
    #[test]
    fn a_partly_offscreen_frame_survives_if_it_can_be_grabbed() {
        let m = vec![Rect { x: 0, y: 0, w: 2560, h: 1440 }];
        // 200px of width and the full height still on screen.
        let ok = frame(2360, 100, 1400, 900);
        assert_eq!(onscreen_frame(ok, &m), Some(ok));
        // Only 40px: not enough to hit with a mouse.
        assert_eq!(onscreen_frame(frame(2520, 100, 1400, 900), &m), None);
    }

    /// If the platform reports no displays, trust the frame rather than moving
    /// every window on the strength of no information.
    #[test]
    fn with_no_monitors_reported_the_frame_is_kept() {
        let f = frame(10, 20, 800, 600);
        assert_eq!(onscreen_frame(f, &[]), Some(f));
    }

    /// Geometry cannot travel through the argument list, so a reload has to take
    /// it from the record — the whole reason the record is preferred there.
    #[test]
    fn a_reload_restores_geometry_from_the_record() {
        let dir = std::env::temp_dir();
        let saved = frame(300, 150, 1000, 700);
        let (is_reload, cwds) =
            parse_window_args(&["--reload".to_string(), dir.to_string_lossy().into_owned()]);
        let specs = plan_initial_windows(
            cwds,
            || vec![session::WindowRecord {
                cwd: Some(dir.clone()), frame: Some(saved), maximized: true,
                ..Default::default()
            }],
            // Off on purpose: a reload restores what was open regardless of the
            // setting, which only governs a real quit-and-relaunch.
            false,
            is_reload,
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].frame, Some(saved), "the frame came back");
        assert!(specs[0].maximized, "and so did zoomed");
    }

    /// A reload with no window arguments has only the record to go on. This fell
    /// through to a single empty window, losing every window and all their state.
    #[test]
    fn a_reload_with_no_arguments_restores_the_record() {
        let dir = std::env::temp_dir();
        let (is_reload, cwds) = parse_window_args(&["--reload".to_string()]);
        assert!(is_reload && cwds.is_empty());
        let specs = plan_initial_windows(
            cwds,
            || vec![record(dir.to_str()), record(None)],
            false,
            is_reload,
        );
        assert_eq!(specs.len(), 2, "both windows, not one empty one: {specs:?}");
        assert_eq!(specs[0].cwd, Some(dir));
        assert_eq!(specs[1].cwd, None);
    }

    /// If the record disagrees with what the reload was told is open, the live set
    /// wins — a stale file must not resurrect a window that was closed.
    #[test]
    fn a_stale_record_does_not_override_the_reload_arguments() {
        let dir = std::env::temp_dir();
        let (is_reload, cwds) =
            parse_window_args(&["--reload".to_string(), dir.to_string_lossy().into_owned()]);
        let specs = plan_initial_windows(
            cwds,
            || vec![
                record(dir.to_str()),
                record(Some("/some/window/that/was/closed")),
            ],
            false,
            is_reload,
        );
        assert_eq!(specs.len(), 1, "one window, as the arguments said: {specs:?}");
        assert_eq!(specs[0].cwd, Some(dir));
        assert_eq!(specs[0].frame, None, "and no geometry, since it was not trusted");
    }

    /// A cold start gets geometry too, via the setting.
    #[test]
    fn a_restored_launch_carries_geometry() {
        let dir = std::env::temp_dir();
        let saved = frame(64, 32, 900, 600);
        let specs = plan_initial_windows(
            Vec::new(),
            || vec![session::WindowRecord {
                cwd: Some(dir), frame: Some(saved), ..Default::default()
            }],
            true,
            false,
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].frame, Some(saved));
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

    /// `forge-ide <path>` opens those folders and nothing else — the remembered
    /// set is not even read, let alone merged in.
    #[test]
    fn command_line_paths_take_precedence() {
        let specs = plan_initial_windows(
            vec![Some(PathBuf::from("/one")), Some(PathBuf::from("/two"))],
            || panic!("the remembered set must not be consulted"),
            true,
            false,
        );
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].cwd, Some(PathBuf::from("/one")));
    }

    /// The reload flag reaches every window, so each restores the session the
    /// reload just saved.
    #[test]
    fn the_reload_flag_is_carried() {
        let dir = std::env::temp_dir();
        let specs = plan_initial_windows(
            vec![Some(dir.clone()), Some(dir)],
            Vec::new,
            true,
            true,
        );
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|s| s.is_reload));
    }

    /// The reported bug: a reload dropped the user at a workspace they never
    /// opened. A folderless window was passed along as its `$HOME` working
    /// directory, so it came back as a `$HOME` workspace — and every reload after
    /// that kept it. The marker has to survive the argv round-trip.
    #[test]
    fn a_folderless_window_survives_a_reload() {
        // Exactly what the reload path builds, for a folder window and a
        // folderless one.
        let argv = vec!["--reload".to_string(), "/work".to_string(), NO_FOLDER.to_string()];
        let (is_reload, cwds) = parse_window_args(&argv);
        assert!(is_reload);

        // Nothing recorded — the config directory was not writable — so the
        // argument list is all there is, and the marker has to carry the folderless
        // window through it.
        let specs = plan_initial_windows(cwds, Vec::new, true, is_reload);
        assert_eq!(specs.len(), 2, "both windows: {specs:?}");
        assert_eq!(specs[0].cwd, Some(PathBuf::from("/work")), "the real folder is kept");
        assert_eq!(specs[1].cwd, None, "and the folderless one is still folderless");
    }

    /// The marker must never be mistaken for a workspace path.
    #[test]
    fn the_marker_is_not_treated_as_a_path() {
        let (_, cwds) = parse_window_args(&[NO_FOLDER.to_string()]);
        let specs = plan_initial_windows(cwds, Vec::new, true, true);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].cwd, None, "not a folder named {NO_FOLDER}");
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
