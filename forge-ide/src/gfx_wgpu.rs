//! Alternative renderer backend: `egui-wgpu` on top of `wgpu`.
//!
//! Behind the `wgpu-renderer` feature; `gfx.rs` + `egui_pass.rs` (raw Vulkan
//! via MoltenVK) remain the default.
//!
//! ## What this did and did not fix
//!
//! This was built to recover ~300 MB. It did not. Measured on the same machine,
//! same workspace, fresh process: **Vulkan 493 MB, wgpu/Metal 488 MB** — and the
//! large `owned unmapped (graphics)` block stayed at 38 allocations of exactly
//! 8 MB (~316 MB) under both.
//!
//! Instrumenting this backend localised it precisely. Footprint by stage:
//!
//! ```text
//! after request_device .................. 14 MB   gfx = 0
//! after Renderer::new (pipeline+shader) . 14 MB   gfx = 0
//! present: after update_texture ......... 55 MB   gfx = 0
//! present: after submit + present ....... 385 MB  gfx = 302 MB
//! ```
//!
//! It appears on the **first `queue.submit()`**, not at device creation, not at
//! pipeline/shader creation, and not at texture upload — identically under
//! MoltenVK and under native Metal. Apple's GPU compiler/driver stack is mapped
//! into the process (`AGXCompilerCore`, `libGPUCompiler*`, `AGXMetalG`), and the
//! uniform 8 MB block size is consistent with an arena there. So it is not the
//! Vulkan→Metal translation layer, which is what this module was written to
//! test. Unexplained: Zed, native Metal, same machine, occupies 4.8 MB in that
//! same region class. The next useful experiment is a minimal wgpu app drawing
//! one triangle — if that also costs ~300 MB, the cost is the Apple driver
//! baseline for any GPU app compiling shaders at runtime.
//!
//! ## Why it is still worth keeping
//!
//! Independent of memory, this backend:
//!   - uses the *native* API per platform (Metal on macOS, D3D12 on Windows,
//!     Vulkan on Linux) from one codebase, and
//!   - removes the bundled 10 MB `libMoltenVK.dylib` plus the
//!     `DYLD_LIBRARY_PATH` probing in `main()` from distribution entirely,
//!     along with the Windows dependency on a vendor Vulkan driver.
//!
//! egui itself is untouched: `egui-wgpu` is pinned to 0.29 to match `egui`
//! 0.29.1, so this is a renderer swap with no UI changes mixed in.

use std::sync::Arc;
use winit::window::Window;

/// Process-wide GPU objects, created by the first window and shared by every
/// window after it — same arrangement as `SharedGfx` in the Vulkan backend, so
/// a second window costs a surface and a `Renderer`, not a second device.
pub struct SharedWgpu {
    instance: wgpu::Instance,
    device:   wgpu::Device,
    queue:    wgpu::Queue,
}

/// Per-window renderer state.
pub struct WgpuPass {
    shared:   Arc<SharedWgpu>,
    surface:  wgpu::Surface<'static>,
    config:   wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    pub ctx:   egui::Context,
    pub winit: egui_winit::State,
    /// Set when the surface needs reconfiguring (resize, moved to another
    /// display, scale-factor change). Mirrors `GfxContext::dirty`.
    dirty: bool,
}

impl WgpuPass {
    /// Build a renderer for `window`, creating the shared device on first call.
    pub fn new(
        shared: &mut Option<Arc<SharedWgpu>>,
        window: Arc<Window>,
    ) -> Result<Self, String> {
        // Ask for exactly the native backend for this platform, not
        // `Backends::PRIMARY`. PRIMARY includes Vulkan, which on macOS means
        // wgpu will try to load MoltenVK purely to enumerate an adapter we have
        // no intention of using — pulling the translation layer back into a
        // process whose whole point is not needing it. The GL fallback is
        // likewise excluded: silently landing on it would mask a real failure.
        let (shared_arc, surface) = match shared {
            Some(existing) => {
                let surface = existing.instance.create_surface(window.clone())
                    .map_err(|e| format!("create surface: {e}"))?;
                (Arc::clone(existing), surface)
            }
            None => {
                #[cfg(target_os = "macos")]
                let backends = wgpu::Backends::METAL;
                #[cfg(target_os = "windows")]
                let backends = wgpu::Backends::DX12;
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                let backends = wgpu::Backends::VULKAN;

                let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                    backends,
                    ..Default::default()
                });
                let surface = instance.create_surface(window.clone())
                    .map_err(|e| format!("create surface: {e}"))?;
                let adapter = pollster::block_on(instance.request_adapter(
                    &wgpu::RequestAdapterOptions {
                        // An editor is not a game: prefer the integrated GPU so
                        // we don't wake a discrete one and burn battery.
                        power_preference: wgpu::PowerPreference::LowPower,
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: false,
                    },
                )).ok_or("no suitable GPU adapter")?;

                let info = adapter.get_info();
                eprintln!("wgpu: {:?} backend on {} ({:?})",
                          info.backend, info.name, info.device_type);

                let (device, queue) = pollster::block_on(adapter.request_device(
                    &wgpu::DeviceDescriptor {
                        label: Some("forge-ide"),
                        required_features: wgpu::Features::empty(),
                        // Ask for no more than the adapter offers; egui needs
                        // nothing exotic.
                        required_limits: adapter.limits(),
                        // The whole point of this backend. `Performance` lets
                        // the allocator keep large slabs around; we would
                        // rather give memory back.
                        memory_hints: wgpu::MemoryHints::MemoryUsage,
                    },
                    None,
                )).map_err(|e| format!("request device: {e}"))?;

                let arc = Arc::new(SharedWgpu { instance, device, queue });
                *shared = Some(Arc::clone(&arc));
                (arc, surface)
            }
        };

        let caps = surface.get_capabilities(&{
            // `get_capabilities` needs the adapter the surface was made from.
            // Re-request it rather than storing one: this runs once per window.
            let inst = &shared_arc.instance;
            pollster::block_on(inst.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })).ok_or("no adapter for surface capabilities")?
        });

        // egui's shader writes gamma-space colour, so pick a non-sRGB format —
        // an `*_SRGB` surface would apply the transfer function twice and wash
        // the whole UI out.
        let format = caps.formats.iter().copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width:  size.width.max(1),
            height: size.height.max(1),
            // Fifo is vsync — the correct choice for an editor, and it is the
            // one present mode guaranteed to exist everywhere.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&shared_arc.device, &config);

        let renderer = egui_wgpu::Renderer::new(
            &shared_arc.device, format, None, 1, false,
        );

        let ctx = egui::Context::default();
        let winit = egui_winit::State::new(
            ctx.clone(), egui::ViewportId::ROOT, &*window,
            Some(window.scale_factor() as f32), None, None,
        );

        Ok(Self { shared: shared_arc, surface, config, renderer, ctx, winit, dirty: false })
    }

    pub fn mark_dirty(&mut self) { self.dirty = true; }

    fn reconfigure(&mut self, window: &Window) {
        let size = window.inner_size();
        self.config.width  = size.width.max(1);
        self.config.height = size.height.max(1);
        self.surface.configure(&self.shared.device, &self.config);
        self.dirty = false;
    }

    /// Upload texture deltas, encode the frame, and present it.
    pub fn present(&mut self, window: &Window, output: egui::FullOutput, ppp: f32) {
        if self.dirty { self.reconfigure(window); }

        for (id, delta) in &output.textures_delta.set {
            self.renderer.update_texture(
                &self.shared.device, &self.shared.queue, *id, delta);
        }

        let primitives = self.ctx.tessellate(output.shapes, ppp);

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            // Lost/outdated surface: reconfigure and skip this frame, the same
            // way the Vulkan path treats ERROR_OUT_OF_DATE_KHR.
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.reconfigure(window);
                return;
            }
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(e) => { eprintln!("wgpu surface: {e}"); return; }
        };

        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.shared.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("egui") });

        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: ppp,
        };
        let user_cmds = self.renderer.update_buffers(
            &self.shared.device, &self.shared.queue,
            &mut encoder, &primitives, &screen);

        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.12, g: 0.12, b: 0.14, a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // `render` wants a 'static pass so a paint callback can outlive the
            // borrow; we have no callbacks, but the signature still requires it.
            self.renderer.render(&mut pass.forget_lifetime(), &primitives, &screen);
        }

        // Textures freed *after* rendering — a delta can free an atlas that
        // this frame's draws still reference.
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        self.shared.queue.submit(user_cmds.into_iter().chain([encoder.finish()]));
        frame.present();
    }
}

/// Isolates the ~300 MB that appears on the first GPU submission: is it the cost
/// of *any* Metal submission, or something about how egui/the surface is used?
///
/// Offscreen only — no window, no surface, no egui, one triangle.
///   cargo test -p forge-ide --features wgpu-renderer minimal_submit -- --ignored --nocapture
#[cfg(test)]
mod minimal_submit_bench {
    fn fp(label: &str) {
        let pid = std::process::id().to_string();
        let Ok(o) = std::process::Command::new("/usr/bin/footprint")
            .args(["-p", &pid]).output() else { return };
        let t = String::from_utf8_lossy(&o.stdout);
        let total = t.lines().find(|l| l.contains("Footprint:"))
            .and_then(|l| l.split("Footprint:").nth(1)).unwrap_or("?").trim();
        let gfx = t.lines().find(|l| l.contains("(unmapped) (graphics)"))
            .map(|l| l.trim().split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| "0 B".into());
        eprintln!("[min] {label:<38} total={total:<26} gfx={gfx}");
    }

    #[test]
    #[ignore]
    fn minimal_submit() { run(wgpu::Limits::downlevel_defaults(), "downlevel_defaults"); }

    #[test]
    #[ignore]
    fn minimal_submit_full_limits() { run_full(); }

    fn run_full() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL, ..Default::default() });
        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions::default())).expect("adapter");
        let l = adapter.limits();
        drop(adapter); drop(instance);
        run(l, "adapter.limits() (full)");
    }

    fn run(limits: wgpu::Limits, label: &str) {
        eprintln!("=== requested limits: {label} ===");
        fp("baseline (no GPU)");

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        fp("after Instance::new");

        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions::default())).expect("adapter");
        fp("after request_adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("minimal"),
                required_features: wgpu::Features::empty(),
                required_limits: limits,
                memory_hints: wgpu::MemoryHints::MemoryUsage,
            }, None)).expect("device");
        fp("after request_device");

        // Trivial pipeline: one hardcoded triangle, solid colour, no buffers.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tri"),
            source: wgpu::ShaderSource::Wgsl(r#"
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(0.0, 0.5), vec2(-0.5, -0.5), vec2(0.5, -0.5));
    return vec4<f32>(p[i], 0.0, 1.0);
}
@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(1.0, 0.5, 0.2, 1.0); }
"#.into()),
        });
        fp("after create_shader_module");

        let fmt = wgpu::TextureFormat::Bgra8Unorm;
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tri"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader, entry_point: "vs",
                compilation_options: Default::default(), buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader, entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(fmt.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        fp("after create_render_pipeline");

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width: 1400, height: 900, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: fmt,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        fp("after create_texture (1400x900)");

        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tri"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None, occlusion_query_set: None,
            });
            pass.set_pipeline(&pipeline);
            pass.draw(0..3, 0..1);
        }
        fp("after encoding draw");

        queue.submit([enc.finish()]);
        device.poll(wgpu::Maintain::Wait);
        fp("after submit + wait   <<< THE ANSWER");
    }
}
