use ash::{khr, vk};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::sync::Arc;
use winit::window::Window;

const FRAMES: usize = 2;

// ── shared (process-wide) Vulkan state ─────────────────────────────────────────

/// The Vulkan instance/device, created exactly once per process and shared by
/// every window's `GfxContext`. Building a fresh `ash::Instance` +
/// `ash::Device` per window (the old behavior) was a real, avoidable cost on
/// top of MoltenVK's own Metal shader-compile cache — this way every window
/// after the first reuses the same instance/device instead of paying for a
/// new one.
struct SharedGfxInner {
    entry:        ash::Entry,
    instance:     ash::Instance,
    surface_fn:   khr::surface::Instance,
    physical:     vk::PhysicalDevice,
    device:       ash::Device,
    queue:        vk::Queue,
    queue_family: u32,
    sc_fn:        khr::swapchain::Device,
}

#[derive(Clone)]
pub struct SharedGfx(Arc<SharedGfxInner>);

impl SharedGfx {
    pub fn instance(&self) -> &ash::Instance { &self.0.instance }
    pub fn physical(&self) -> vk::PhysicalDevice { self.0.physical }
    pub fn device(&self)   -> &ash::Device { &self.0.device }
    pub fn queue(&self)    -> vk::Queue { self.0.queue }

    /// Creates the shared instance/device. `window` is only used to
    /// enumerate required extensions and to pick a present-capable physical
    /// device; the surface created along the way is returned so the caller's
    /// first `GfxContext` can reuse it instead of creating (and destroying)
    /// a second, throwaway one.
    fn new(window: &Window) -> Result<(Self, vk::SurfaceKHR), String> {
        let entry = load_entry()?;

        let mut exts = ash_window::enumerate_required_extensions(
            window.display_handle().unwrap().as_raw(),
        ).map_err(|e| format!("enumerate extensions: {e:?}"))?
        .to_vec();

        // Only request macOS portability extensions if MoltenVK actually supports them.
        // Older bundled MoltenVK builds omit VK_KHR_portability_enumeration.
        let available_exts = unsafe {
            entry.enumerate_instance_extension_properties(None)
                .unwrap_or_default()
        };
        let has_ext = |name: &std::ffi::CStr| {
            available_exts.iter().any(|e| {
                let n = unsafe { std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) };
                n == name
            })
        };

        let mut flags = vk::InstanceCreateFlags::empty();
        #[cfg(target_os = "macos")]
        {
            if has_ext(khr::portability_enumeration::NAME) {
                exts.push(khr::portability_enumeration::NAME.as_ptr());
                flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
            }
            if has_ext(khr::get_physical_device_properties2::NAME) {
                exts.push(khr::get_physical_device_properties2::NAME.as_ptr());
            }
        }

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"Forge IDE")
            .api_version(vk::API_VERSION_1_3);

        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&exts)
                    .flags(flags),
                None,
            )
        }.map_err(|e| format!("create instance: {e:?}"))?;

        let surface = unsafe {
            ash_window::create_surface(
                &entry, &instance,
                window.display_handle().unwrap().as_raw(),
                window.window_handle().unwrap().as_raw(),
                None,
            )
        }.map_err(|e| format!("create surface: {e:?}"))?;

        let surface_fn = khr::surface::Instance::new(&entry, &instance);

        let (physical, queue_family) = pick_device(&instance, &surface_fn, surface)?;

        let mut dev_exts = vec![khr::swapchain::NAME.as_ptr()];
        #[cfg(target_os = "macos")]
        dev_exts.push(ash::khr::portability_subset::NAME.as_ptr());

        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&[1.0])];

        let device = unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&dev_exts),
                None,
            )
        }.map_err(|e| format!("create device: {e:?}"))?;

        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let sc_fn = khr::swapchain::Device::new(&instance, &device);

        Ok((
            Self(Arc::new(SharedGfxInner {
                entry, instance, surface_fn, physical, device, queue, queue_family, sc_fn,
            })),
            surface,
        ))
    }

    /// Creates a fresh surface for `window` against the already-shared
    /// instance/device — used for every window after the first.
    fn create_surface(&self, window: &Window) -> Result<vk::SurfaceKHR, String> {
        let surface = unsafe {
            ash_window::create_surface(
                &self.0.entry, &self.0.instance,
                window.display_handle().unwrap().as_raw(),
                window.window_handle().unwrap().as_raw(),
                None,
            )
        }.map_err(|e| format!("create surface: {e:?}"))?;

        let present = unsafe {
            self.0.surface_fn.get_physical_device_surface_support(
                self.0.physical, self.0.queue_family, surface,
            )
        }.unwrap_or(false);
        if !present {
            unsafe { self.0.surface_fn.destroy_surface(surface, None); }
            return Err("new window's surface doesn't support presentation on the shared GPU/queue".into());
        }
        Ok(surface)
    }
}

impl Drop for SharedGfxInner {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

// ── per-window Vulkan state ─────────────────────────────────────────────────────

pub struct GfxContext {
    shared:       SharedGfx,
    surface:      vk::SurfaceKHR,
    swapchain:    vk::SwapchainKHR,
    views:        Vec<vk::ImageView>,
    pub format:   vk::Format,
    pub extent:   vk::Extent2D,
    pub render_pass:  vk::RenderPass,
    framebuffers:     Vec<vk::Framebuffer>,
    pub command_pool: vk::CommandPool,
    cmd_bufs:     [vk::CommandBuffer; FRAMES],
    img_avail:    [vk::Semaphore; FRAMES],
    render_done:  [vk::Semaphore; FRAMES],
    fences:       [vk::Fence; FRAMES],
    frame:        usize,
    dirty:        bool,
}

impl GfxContext {
    /// First window in the process — also builds the process-wide
    /// `SharedGfx`, returned alongside so `Ide` can hand it to every later
    /// window via `new_shared`.
    pub fn new(window: &Window) -> Result<(SharedGfx, Self), String> {
        let (shared, surface) = SharedGfx::new(window)?;
        let ctx = Self::build(shared.clone(), surface, window)?;
        Ok((shared, ctx))
    }

    /// Every window after the first — reuses the process-wide `SharedGfx`
    /// instance/device instead of creating a new one.
    pub fn new_shared(shared: &SharedGfx, window: &Window) -> Result<Self, String> {
        let surface = shared.create_surface(window)?;
        Self::build(shared.clone(), surface, window)
    }

    fn build(shared: SharedGfx, surface: vk::SurfaceKHR, window: &Window) -> Result<Self, String> {
        let (swapchain, views, format, extent) = build_swapchain(
            shared.physical(), shared.device(),
            &shared.0.surface_fn, &shared.0.sc_fn,
            surface, window, vk::SwapchainKHR::null(),
        )?;

        let render_pass   = make_render_pass(shared.device(), format)?;
        let framebuffers  = make_framebuffers(shared.device(), render_pass, &views, extent)?;
        let command_pool  = make_command_pool(shared.device(), shared.0.queue_family)?;
        let cmd_bufs      = alloc_cmd_bufs(shared.device(), command_pool)?;
        let (img_avail, render_done, fences) = make_sync(shared.device())?;

        Ok(Self {
            shared, surface, swapchain, views, format, extent,
            render_pass, framebuffers, command_pool, cmd_bufs,
            img_avail, render_done, fences,
            frame: 0, dirty: false,
        })
    }

    pub fn instance(&self) -> &ash::Instance { self.shared.instance() }
    pub fn physical(&self) -> vk::PhysicalDevice { self.shared.physical() }
    pub fn device(&self)   -> &ash::Device { self.shared.device() }
    pub fn queue(&self)    -> vk::Queue { self.shared.queue() }
    pub fn mark_dirty(&mut self) { self.dirty = true; }
    pub fn is_dirty(&self) -> bool { self.dirty }

    pub fn rebuild(&mut self, window: &Window) {
        let device = self.shared.device();
        unsafe { let _ = device.device_wait_idle(); }

        for &fb in &self.framebuffers { unsafe { device.destroy_framebuffer(fb, None); } }
        for &v  in &self.views        { unsafe { device.destroy_image_view(v,  None); } }

        let old = self.swapchain;
        match build_swapchain(self.shared.physical(), device,
                              &self.shared.0.surface_fn, &self.shared.0.sc_fn,
                              self.surface, window, old)
        {
            Ok((sc, views, format, extent)) => {
                unsafe { self.shared.0.sc_fn.destroy_swapchain(old, None); }
                self.swapchain    = sc;
                self.views        = views;
                self.format       = format;
                self.extent       = extent;
                self.framebuffers = make_framebuffers(
                    device, self.render_pass, &self.views, self.extent
                ).unwrap_or_default();
                self.dirty = false;
            }
            Err(e) => eprintln!("rebuild swapchain: {e}"),
        }
    }

    /// Returns (cmd_buf, image_index, frame_index) or None if the swapchain needs rebuilding.
    pub fn begin_frame(&mut self) -> Option<(vk::CommandBuffer, u32, usize)> {
        let device = self.shared.device();
        let fi = self.frame % FRAMES;

        unsafe {
            let _ = device.wait_for_fences(&[self.fences[fi]], true, u64::MAX);
            let _ = device.reset_fences(&[self.fences[fi]]);
        }

        let result = unsafe {
            self.shared.0.sc_fn.acquire_next_image(
                self.swapchain, u64::MAX, self.img_avail[fi], vk::Fence::null(),
            )
        };

        let img_idx = match result {
            Ok((idx, false)) => idx,
            Ok((_, true)) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.dirty = true;
                return None;
            }
            Err(e) => { eprintln!("acquire: {e:?}"); return None; }
        };

        let cmd = self.cmd_bufs[fi];
        unsafe {
            let _ = device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty());
            let _ = device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            );
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.12, 0.12, 0.14, 1.0] },
            }];
            device.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(self.framebuffers[img_idx as usize])
                    .render_area(vk::Rect2D { offset: Default::default(), extent: self.extent })
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
        }

        Some((cmd, img_idx, fi))
    }

    pub fn end_frame(&mut self, cmd: vk::CommandBuffer, img_idx: u32) {
        let device = self.shared.device();
        let fi = self.frame % FRAMES;
        unsafe {
            device.cmd_end_render_pass(cmd);
            let _ = device.end_command_buffer(cmd);

            let wait_mask = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let _ = device.queue_submit(self.shared.queue(), &[
                vk::SubmitInfo::default()
                    .wait_semaphores(&[self.img_avail[fi]])
                    .wait_dst_stage_mask(&wait_mask)
                    .command_buffers(&[cmd])
                    .signal_semaphores(&[self.render_done[fi]]),
            ], self.fences[fi]);

            match self.shared.0.sc_fn.queue_present(self.shared.queue(), &vk::PresentInfoKHR::default()
                .wait_semaphores(&[self.render_done[fi]])
                .swapchains(&[self.swapchain])
                .image_indices(&[img_idx]))
            {
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty = true,
                _ => {}
            }
        }
        self.frame += 1;
    }
}

impl Drop for GfxContext {
    fn drop(&mut self) {
        let device = self.shared.device();
        unsafe {
            let _ = device.device_wait_idle();
            for i in 0..FRAMES {
                device.destroy_semaphore(self.img_avail[i],   None);
                device.destroy_semaphore(self.render_done[i], None);
                device.destroy_fence(self.fences[i],          None);
            }
            device.destroy_command_pool(self.command_pool, None);
            for &fb in &self.framebuffers { device.destroy_framebuffer(fb, None); }
            for &v  in &self.views        { device.destroy_image_view(v,  None); }
            self.shared.0.sc_fn.destroy_swapchain(self.swapchain, None);
            device.destroy_render_pass(self.render_pass, None);
            self.shared.0.surface_fn.destroy_surface(self.surface, None);
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn pick_device(
    instance:   &ash::Instance,
    surface_fn: &khr::surface::Instance,
    surface:    vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32), String> {
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("enumerate devices: {e:?}"))?;

    for pd in &devices {
        let props = unsafe { instance.get_physical_device_properties(*pd) };
        let families = unsafe { instance.get_physical_device_queue_family_properties(*pd) };
        for (i, fam) in families.iter().enumerate() {
            if !fam.queue_flags.contains(vk::QueueFlags::GRAPHICS) { continue; }
            let present = unsafe {
                surface_fn.get_physical_device_surface_support(*pd, i as u32, surface)
                    .unwrap_or(false)
            };
            if present {
                // Prefer discrete GPU
                if props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
                    return Ok((*pd, i as u32));
                }
            }
        }
    }
    // Fallback: any device with graphics + present
    for pd in &devices {
        let families = unsafe { instance.get_physical_device_queue_family_properties(*pd) };
        for (i, fam) in families.iter().enumerate() {
            if !fam.queue_flags.contains(vk::QueueFlags::GRAPHICS) { continue; }
            let present = unsafe {
                surface_fn.get_physical_device_surface_support(*pd, i as u32, surface)
                    .unwrap_or(false)
            };
            if present { return Ok((*pd, i as u32)); }
        }
    }
    Err("no suitable GPU found".into())
}

fn build_swapchain(
    physical:   vk::PhysicalDevice,
    device:     &ash::Device,
    surface_fn: &khr::surface::Instance,
    sc_fn:      &khr::swapchain::Device,
    surface:    vk::SurfaceKHR,
    window:     &Window,
    old:        vk::SwapchainKHR,
) -> Result<(vk::SwapchainKHR, Vec<vk::ImageView>, vk::Format, vk::Extent2D), String> {
    let caps = unsafe {
        surface_fn.get_physical_device_surface_capabilities(physical, surface)
    }.map_err(|e| format!("surface caps: {e:?}"))?;

    let formats = unsafe {
        surface_fn.get_physical_device_surface_formats(physical, surface)
    }.map_err(|e| format!("surface formats: {e:?}"))?;

    let format = formats.iter()
        .find(|f| {
            (f.format == vk::Format::B8G8R8A8_SRGB || f.format == vk::Format::R8G8B8A8_SRGB)
            && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| formats.first())
        .ok_or("no surface format")?;

    let present_modes = unsafe {
        surface_fn.get_physical_device_surface_present_modes(physical, surface)
    }.map_err(|e| format!("present modes: {e:?}"))?;
    let present_mode = present_modes.iter()
        .copied()
        .find(|&m| m == vk::PresentModeKHR::MAILBOX)
        .unwrap_or(vk::PresentModeKHR::FIFO);

    let size  = window.inner_size();
    let extent = if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D {
            width:  size.width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: size.height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        }
    };

    let img_count = (caps.min_image_count + 1).min(
        if caps.max_image_count == 0 { u32::MAX } else { caps.max_image_count }
    );

    let swapchain = unsafe {
        sc_fn.create_swapchain(
            &vk::SwapchainCreateInfoKHR::default()
                .surface(surface)
                .min_image_count(img_count)
                .image_format(format.format)
                .image_color_space(format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                .present_mode(present_mode)
                .clipped(true)
                .old_swapchain(old),
            None,
        )
    }.map_err(|e| format!("create swapchain: {e:?}"))?;

    let images = unsafe { sc_fn.get_swapchain_images(swapchain) }
        .map_err(|e| format!("get images: {e:?}"))?;

    let views = images.iter().map(|&img| {
        unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(img)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format.format)
                    .subresource_range(vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1).layer_count(1)),
                None,
            )
        }.map_err(|e| format!("image view: {e:?}"))
    }).collect::<Result<Vec<_>, _>>()?;

    Ok((swapchain, views, format.format, extent))
}

fn make_render_pass(device: &ash::Device, format: vk::Format) -> Result<vk::RenderPass, String> {
    let attachments = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];

    let color_ref = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];

    let subpasses = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)];

    let deps = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];

    unsafe {
        device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attachments)
                .subpasses(&subpasses)
                .dependencies(&deps),
            None,
        )
    }.map_err(|e| format!("render pass: {e:?}"))
}

fn make_framebuffers(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    views: &[vk::ImageView],
    extent: vk::Extent2D,
) -> Result<Vec<vk::Framebuffer>, String> {
    views.iter().map(|&v| {
        let attachments = [v];
        unsafe {
            device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(&attachments)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1),
                None,
            )
        }.map_err(|e| format!("framebuffer: {e:?}"))
    }).collect()
}

fn make_command_pool(device: &ash::Device, family: u32) -> Result<vk::CommandPool, String> {
    unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
    }.map_err(|e| format!("command pool: {e:?}"))
}

fn alloc_cmd_bufs(
    device: &ash::Device,
    pool:   vk::CommandPool,
) -> Result<[vk::CommandBuffer; FRAMES], String> {
    let v = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(FRAMES as u32),
        )
    }.map_err(|e| format!("cmd bufs: {e:?}"))?;
    Ok([v[0], v[1]])
}

fn make_sync(device: &ash::Device)
    -> Result<([vk::Semaphore; FRAMES], [vk::Semaphore; FRAMES], [vk::Fence; FRAMES]), String>
{
    let sem   = vk::SemaphoreCreateInfo::default();
    let fence = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
    let mut ia = [vk::Semaphore::null(); FRAMES];
    let mut rd = [vk::Semaphore::null(); FRAMES];
    let mut fn_ = [vk::Fence::null(); FRAMES];
    for i in 0..FRAMES {
        ia[i]  = unsafe { device.create_semaphore(&sem,   None) }.map_err(|e| format!("{e:?}"))?;
        rd[i]  = unsafe { device.create_semaphore(&sem,   None) }.map_err(|e| format!("{e:?}"))?;
        fn_[i] = unsafe { device.create_fence(&fence,     None) }.map_err(|e| format!("{e:?}"))?;
    }
    Ok((ia, rd, fn_))
}

fn load_entry() -> Result<ash::Entry, String> {
    // On macOS there is no system libvulkan.dylib — we load MoltenVK directly.
    #[cfg(target_os = "macos")]
    {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();

        // A real shipped `.app`: Contents/MacOS/<bin>, with MoltenVK bundled
        // alongside it or in the standard Contents/Frameworks/ location.
        // This has to be resolved from the *running* executable's actual
        // path at runtime, not baked in at compile time — `current_exe()`
        // reflects wherever this process actually launched from (an
        // end user's `/Applications/Forge IDE.app`, not this dev machine).
        if let Ok(exe) = std::env::current_exe() {
            if let Some(macos_dir) = exe.parent() {
                candidates.push(macos_dir.join("libMoltenVK.dylib"));
                if let Some(contents_dir) = macos_dir.parent() {
                    candidates.push(contents_dir.join("Frameworks").join("libMoltenVK.dylib"));
                }
            }
        }

        // Dev convenience: running straight from the repo via `cargo run` /
        // `./target/debug/forge-ide` rather than a built `.app`. This is a
        // compile-time constant baked into the binary at build time — fine
        // for that case (it's always this same checkout), meaningless for
        // an actually-shipped build, which is why it's a fallback and not
        // the primary lookup.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        candidates.push(manifest.join("runtime/macos/libMoltenVK.dylib"));
        candidates.push(manifest.join("../Game Engine/launchers/desktop/runtime/macos/libMoltenVK.dylib"));

        // Last resort: whatever happens to already be installed system-wide.
        candidates.push(std::path::PathBuf::from("/opt/homebrew/lib/libMoltenVK.dylib"));
        candidates.push(std::path::PathBuf::from("/usr/local/lib/libMoltenVK.dylib"));
        candidates.push(std::path::PathBuf::from("/usr/local/lib/libvulkan.dylib"));
        candidates.push(std::path::PathBuf::from("/opt/homebrew/lib/libvulkan.dylib"));

        for path in &candidates {
            if path.exists() {
                if let Ok(e) = unsafe { ash::Entry::load_from(path) } {
                    return Ok(e);
                }
            }
        }
        Err(
            "MoltenVK not found. Fix with one of:\n  \
             brew install molten-vk\n  \
             Copy libMoltenVK.dylib to runtime/macos/libMoltenVK.dylib"
            .into(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { ash::Entry::load() }.map_err(|e| format!("Vulkan not found: {e}"))
    }
}
