use ash::vk;
use std::collections::HashMap;
use std::sync::Arc;
use crate::gfx::SharedGfx;

const VERTEX_BUFFER_SIZE: vk::DeviceSize = 8 * 1024 * 1024;
const INDEX_BUFFER_SIZE:  vk::DeviceSize = 4 * 1024 * 1024;
const FRAMES: usize = 2;

/// One reusable host-visible upload buffer, shared by every texture upload and
/// atlas patch.
///
/// Each upload used to create and destroy its own staging buffer. Besides the
/// per-call churn — `patch_image` runs whenever egui rasterizes new glyphs — the
/// allocations themselves were expensive: instrumenting `upload_rgba` showed a
/// single 6 MB staging allocation expanding MoltenVK's internal Metal pool by
/// ~172 MB. Allocating once and growing monotonically keeps that to one.
#[derive(Default)]
struct Staging {
    buf:  vk::Buffer,
    mem:  vk::DeviceMemory,
    size: vk::DeviceSize,
}

struct GpuTex {
    image:      vk::Image,
    memory:     vk::DeviceMemory,
    view:       vk::ImageView,
    sampler:    vk::Sampler,
    descriptor: vk::DescriptorSet,
    width:      u32,
    height:     u32,
}

/// The egui pipeline/layout/descriptor-set-layout — the objects whose
/// creation triggers MoltenVK's expensive Metal shader compile — built once
/// per process and shared by every window's `EguiPass`. A pipeline may be
/// used with any render pass "compatible" with the one it was created
/// against (same attachment format/sample count, per the Vulkan spec's
/// render-pass-compatibility rules), not only the exact object — so sharing
/// this across windows, each of which still creates its own `vk::RenderPass`
/// via `GfxContext`, is valid as long as they all pick the same swapchain
/// format, which every window does on a given physical device in practice.
struct SharedEguiPassInner {
    gfx:        SharedGfx,
    set_layout: vk::DescriptorSetLayout,
    layout:     vk::PipelineLayout,
    pipeline:   vk::Pipeline,
}

#[derive(Clone)]
pub struct SharedEguiPass(Arc<SharedEguiPassInner>);

impl SharedEguiPass {
    pub fn new(gfx: &SharedGfx, render_pass: vk::RenderPass) -> Result<Self, String> {
        let set_layout = make_set_layout(gfx.device())?;
        let (layout, pipeline) = make_pipeline(gfx.device(), render_pass, set_layout)?;
        Ok(Self(Arc::new(SharedEguiPassInner {
            gfx: gfx.clone(), set_layout, layout, pipeline,
        })))
    }
}

impl Drop for SharedEguiPassInner {
    fn drop(&mut self) {
        let device = self.gfx.device();
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

pub struct EguiPass {
    shared:          SharedEguiPass,
    desc_pool:       vk::DescriptorPool,
    vert_bufs:       [vk::Buffer;       FRAMES],
    vert_mems:       [vk::DeviceMemory; FRAMES],
    idx_bufs:        [vk::Buffer;       FRAMES],
    idx_mems:        [vk::DeviceMemory; FRAMES],
    textures:        HashMap<egui::TextureId, GpuTex>,
    /// Reusable upload buffer — see `Staging`.
    stage:           Staging,
    pub ctx:         egui::Context,
    pub winit:       egui_winit::State,
}

impl EguiPass {
    pub fn new(
        shared:      &SharedEguiPass,
        instance:    &ash::Instance,
        physical:    vk::PhysicalDevice,
        device:      &ash::Device,
        window:      &winit::window::Window,
    ) -> Result<Self, String> {
        let desc_pool  = make_desc_pool(device)?;

        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let mut vert_bufs  = [vk::Buffer::null();       FRAMES];
        let mut vert_mems  = [vk::DeviceMemory::null(); FRAMES];
        let mut idx_bufs   = [vk::Buffer::null();       FRAMES];
        let mut idx_mems   = [vk::DeviceMemory::null(); FRAMES];

        for i in 0..FRAMES {
            let (vb, vm) = host_buffer(device, &mem_props, VERTEX_BUFFER_SIZE, vk::BufferUsageFlags::VERTEX_BUFFER)?;
            let (ib, im) = host_buffer(device, &mem_props, INDEX_BUFFER_SIZE,  vk::BufferUsageFlags::INDEX_BUFFER)?;
            vert_bufs[i] = vb; vert_mems[i] = vm;
            idx_bufs[i]  = ib; idx_mems[i]  = im;
        }

        let ctx   = egui::Context::default();
        let winit = egui_winit::State::new(
            ctx.clone(), egui::ViewportId::ROOT, window,
            Some(window.scale_factor() as f32), None, None,
        );

        Ok(Self {
            shared: shared.clone(), desc_pool,
            vert_bufs, vert_mems, idx_bufs, idx_mems,
            textures: HashMap::new(), stage: Staging::default(), ctx, winit,
        })
    }

    pub fn update_textures(
        &mut self,
        instance:     &ash::Instance,
        physical:     vk::PhysicalDevice,
        device:       &ash::Device,
        queue:        vk::Queue,
        command_pool: vk::CommandPool,
        delta:        egui::TexturesDelta,
    ) -> Result<(), String> {
        for id in delta.free {
            if let Some(t) = self.textures.remove(&id) { destroy_tex(device, t); }
        }
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        for (id, img_delta) in delta.set {
            let rgba = to_rgba(&img_delta.image);
            let (w, h) = (img_delta.image.size()[0] as u32, img_delta.image.size()[1] as u32);

            if let Some([px, py]) = img_delta.pos {
                if let Some(existing) = self.textures.get(&id) {
                    let (px, py) = (px as u32, py as u32);
                    // A patch that doesn't fit inside the already-allocated
                    // image would issue a copy past its actual GPU memory —
                    // a real page fault, not a benign glitch (this is the
                    // exact shape of a `GPU Address Fault` / lost-device
                    // crash). The delta's pixels only cover the small patched
                    // region, not the full atlas, so there's no safe way to
                    // "fully re-upload" from it — skip the update instead of
                    // trusting the offset/size blindly or corrupting the
                    // existing (still valid) texture with a wrong-sized one.
                    if px + w <= existing.width && py + h <= existing.height {
                        ensure_staging(device, &mem_props, &mut self.stage,
                                       rgba.len() as vk::DeviceSize)?;
                        patch_image(device, &self.stage, queue, command_pool,
                                    existing.image, w, h, px, py, &rgba)?;
                    } else {
                        eprintln!(
                            "egui_pass: dropping out-of-bounds texture patch for {id:?}: \
                             ({px},{py})+{w}x{h} exceeds allocated {}x{}",
                            existing.width, existing.height);
                    }
                    continue;
                }
            }
            if let Some(old) = self.textures.remove(&id) { destroy_tex(device, old); }
            ensure_staging(device, &mem_props, &mut self.stage,
                           rgba.len() as vk::DeviceSize)?;
            let t = upload_tex(device, &mem_props, &self.stage, queue, command_pool,
                               self.desc_pool, self.shared.0.set_layout, w, h, &rgba)?;
            self.textures.insert(id, t);
        }
        Ok(())
    }

    pub fn record(
        &self,
        device:      &ash::Device,
        cmd:         vk::CommandBuffer,
        frame_index: usize,
        extent:      vk::Extent2D,
        primitives:  &[egui::ClippedPrimitive],
        ppp:         f32,
    ) {
        let fi = frame_index % FRAMES;
        let mut verts: Vec<u8> = Vec::new();
        let mut idxs:  Vec<u8> = Vec::new();

        struct Draw { vo: u64, io: u64, ic: u32, sc: vk::Rect2D, tex: egui::TextureId }
        let mut draws: Vec<Draw> = Vec::new();

        for cp in primitives {
            if let egui::epaint::Primitive::Mesh(mesh) = &cp.primitive {
                if mesh.vertices.is_empty() || mesh.indices.is_empty() { continue; }
                let vo = verts.len() as u64;
                let io = idxs.len()  as u64;
                let vb = unsafe { std::slice::from_raw_parts(
                    mesh.vertices.as_ptr().cast::<u8>(),
                    mesh.vertices.len() * std::mem::size_of::<egui::epaint::Vertex>(),
                )};
                let ib = unsafe { std::slice::from_raw_parts(
                    mesh.indices.as_ptr().cast::<u8>(),
                    mesh.indices.len() * 4,
                )};
                if vo + vb.len() as u64 > VERTEX_BUFFER_SIZE { break; }
                if io + ib.len() as u64 > INDEX_BUFFER_SIZE  { break; }
                verts.extend_from_slice(vb);
                idxs.extend_from_slice(ib);
                draws.push(Draw {
                    vo, io,
                    ic:  mesh.indices.len() as u32,
                    sc:  clip_rect(&cp.clip_rect, ppp, extent),
                    tex: mesh.texture_id,
                });
            }
        }

        if draws.is_empty() { return; }
        unsafe {
            upload_bytes(device, self.vert_mems[fi], &verts);
            upload_bytes(device, self.idx_mems[fi],  &idxs);

            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.shared.0.pipeline);
            device.cmd_set_viewport(cmd, 0, &[vk::Viewport {
                x: 0.0, y: 0.0,
                width: extent.width as f32, height: extent.height as f32,
                min_depth: 0.0, max_depth: 1.0,
            }]);
            let screen = [extent.width as f32 / ppp, extent.height as f32 / ppp];
            device.cmd_push_constants(cmd, self.shared.0.layout, vk::ShaderStageFlags::VERTEX, 0,
                bytemuck::bytes_of(&screen));
            device.cmd_bind_vertex_buffers(cmd, 0, &[self.vert_bufs[fi]], &[0]);
            device.cmd_bind_index_buffer(cmd, self.idx_bufs[fi], 0, vk::IndexType::UINT32);

            let mut last = egui::TextureId::default();
            let mut bound = false;
            for d in &draws {
                if !bound || d.tex != last {
                    if let Some(t) = self.textures.get(&d.tex) {
                        device.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS,
                            self.shared.0.layout, 0, &[t.descriptor], &[]);
                        last = d.tex; bound = true;
                    } else { continue; }
                }
                device.cmd_set_scissor(cmd, 0, &[d.sc]);
                device.cmd_draw_indexed(
                    cmd, d.ic, 1,
                    (d.io / 4) as u32,
                    (d.vo / std::mem::size_of::<egui::epaint::Vertex>() as u64) as i32,
                    0,
                );
            }
        }
    }

    pub fn destroy(&mut self, device: &ash::Device) {
        unsafe {
            let _ = device.device_wait_idle();
            for t in self.textures.values() { destroy_tex(device, GpuTex {
                image: t.image, memory: t.memory, view: t.view,
                sampler: t.sampler, descriptor: t.descriptor,
                width: t.width, height: t.height,
            }); }
            for i in 0..FRAMES {
                device.destroy_buffer(self.vert_bufs[i],  None);
                device.free_memory(self.vert_mems[i],     None);
                device.destroy_buffer(self.idx_bufs[i],   None);
                device.free_memory(self.idx_mems[i],      None);
            }
            if self.stage.size > 0 {
                device.destroy_buffer(self.stage.buf, None);
                device.free_memory(self.stage.mem, None);
            }
            device.destroy_descriptor_pool(self.desc_pool, None);
        }
    }
}

// ── pipeline ──────────────────────────────────────────────────────────────────

fn make_set_layout(device: &ash::Device) -> Result<vk::DescriptorSetLayout, String> {
    let b = vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT);
    unsafe { device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[b]), None,
    )}.map_err(|e| format!("set layout: {e:?}"))
}

fn make_desc_pool(device: &ash::Device) -> Result<vk::DescriptorPool, String> {
    let size = vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 64 };
    unsafe { device.create_descriptor_pool(
        &vk::DescriptorPoolCreateInfo::default()
            .max_sets(64).pool_sizes(&[size])
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET),
        None,
    )}.map_err(|e| format!("desc pool: {e:?}"))
}

fn make_pipeline(
    device:     &ash::Device,
    render_pass: vk::RenderPass,
    set_layout:  vk::DescriptorSetLayout,
) -> Result<(vk::PipelineLayout, vk::Pipeline), String> {
    let vert_spv = include_bytes!(concat!(env!("OUT_DIR"), "/egui.vert.spv"));
    let frag_spv = include_bytes!(concat!(env!("OUT_DIR"), "/egui.frag.spv"));
    let vert_mod = shader_module(device, vert_spv)?;
    let frag_mod = shader_module(device, frag_spv)?;

    let pc = vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::VERTEX).offset(0).size(8);
    let layout = unsafe { device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&[set_layout])
            .push_constant_ranges(&[pc]),
        None,
    )}.map_err(|e| format!("pipeline layout: {e:?}"))?;

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vert_mod).name(entry),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(frag_mod).name(entry),
    ];

    let bindings = [vk::VertexInputBindingDescription { binding: 0, stride: 20, input_rate: vk::VertexInputRate::VERTEX }];
    let attrs = [
        vk::VertexInputAttributeDescription { location: 0, binding: 0, format: vk::Format::R32G32_SFLOAT,  offset: 0  },
        vk::VertexInputAttributeDescription { location: 1, binding: 0, format: vk::Format::R32G32_SFLOAT,  offset: 8  },
        vk::VertexInputAttributeDescription { location: 2, binding: 0, format: vk::Format::R8G8B8A8_UNORM, offset: 16 },
    ];

    let blend_att = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA);

    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];

    let pipeline = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vk::PipelineVertexInputStateCreateInfo::default()
                    .vertex_binding_descriptions(&bindings)
                    .vertex_attribute_descriptions(&attrs))
                .input_assembly_state(&vk::PipelineInputAssemblyStateCreateInfo::default()
                    .topology(vk::PrimitiveTopology::TRIANGLE_LIST))
                .viewport_state(&vk::PipelineViewportStateCreateInfo::default()
                    .viewport_count(1).scissor_count(1))
                .rasterization_state(&vk::PipelineRasterizationStateCreateInfo::default()
                    .polygon_mode(vk::PolygonMode::FILL)
                    .cull_mode(vk::CullModeFlags::NONE)
                    .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                    .line_width(1.0))
                .multisample_state(&vk::PipelineMultisampleStateCreateInfo::default()
                    .rasterization_samples(vk::SampleCountFlags::TYPE_1))
                .color_blend_state(&vk::PipelineColorBlendStateCreateInfo::default()
                    .attachments(&[blend_att]))
                .depth_stencil_state(&vk::PipelineDepthStencilStateCreateInfo::default()
                    .depth_test_enable(false).depth_write_enable(false))
                .dynamic_state(&vk::PipelineDynamicStateCreateInfo::default()
                    .dynamic_states(&dynamic))
                .layout(layout)
                .render_pass(render_pass)
                .subpass(0),
        ], None)
    }.map_err(|(_, e)| format!("pipeline: {e:?}"))?[0];

    unsafe {
        device.destroy_shader_module(vert_mod, None);
        device.destroy_shader_module(frag_mod, None);
    }
    Ok((layout, pipeline))
}

fn shader_module(device: &ash::Device, spv: &[u8]) -> Result<vk::ShaderModule, String> {
    let code: Vec<u32> = spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None) }
        .map_err(|e| format!("shader module: {e:?}"))
}

// ── texture helpers ───────────────────────────────────────────────────────────

fn upload_tex(
    device:     &ash::Device,
    mem_props:  &vk::PhysicalDeviceMemoryProperties,
    stage:      &Staging,
    queue:      vk::Queue,
    pool:       vk::CommandPool,
    desc_pool:  vk::DescriptorPool,
    set_layout: vk::DescriptorSetLayout,
    w: u32, h: u32, rgba: &[u8],
) -> Result<GpuTex, String> {
    let image = unsafe { device.create_image(&vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_SRGB)
        .extent(vk::Extent3D { width: w, height: h, depth: 1 })
        .mip_levels(1).array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED),
    None) }.map_err(|e| format!("image: {e:?}"))?;

    let req    = unsafe { device.get_image_memory_requirements(image) };
    let mt     = find_memory_type(mem_props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .ok_or("no device-local memory")?;
    let memory = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(mt), None,
    )}.map_err(|e| format!("image mem: {e:?}"))?;
    unsafe { device.bind_image_memory(image, memory, 0) }.map_err(|e| format!("bind: {e:?}"))?;

    upload_rgba(device, stage, queue, pool, image, w, h, rgba)?;

    let view = unsafe { device.create_image_view(&vk::ImageViewCreateInfo::default()
        .image(image).view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_SRGB)
        .subresource_range(vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1).layer_count(1)),
    None) }.map_err(|e| format!("view: {e:?}"))?;

    let sampler = unsafe { device.create_sampler(&vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR).min_filter(vk::Filter::LINEAR)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
    None) }.map_err(|e| format!("sampler: {e:?}"))?;

    let descriptor = unsafe { device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(desc_pool).set_layouts(&[set_layout]),
    )}.map_err(|e| format!("descriptor: {e:?}"))?[0];

    unsafe { device.update_descriptor_sets(&[
        vk::WriteDescriptorSet::default()
            .dst_set(descriptor).dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&[vk::DescriptorImageInfo::default()
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image_view(view).sampler(sampler)]),
    ], &[]); }

    Ok(GpuTex { image, memory, view, sampler, descriptor, width: w, height: h })
}

fn upload_rgba(
    device: &ash::Device, stage: &Staging,
    queue: vk::Queue, pool: vk::CommandPool,
    image: vk::Image, w: u32, h: u32, rgba: &[u8],
) -> Result<(), String> {
    let size = rgba.len() as vk::DeviceSize;
    debug_assert!(stage.size >= size, "staging buffer too small — call ensure_staging first");
    let staging = stage.buf;
    unsafe {
        let ptr = device.map_memory(stage.mem, 0, size, vk::MemoryMapFlags::empty())
            .map_err(|e| format!("{e:?}"))?;
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), ptr.cast::<u8>(), rgba.len());
        device.unmap_memory(stage.mem);
    }
    let cmd = one_shot_begin(device, pool)?;
    unsafe {
        device.cmd_pipeline_barrier(cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(), &[], &[], &[
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .image(image).subresource_range(img_range()),
        ]);
        device.cmd_copy_buffer_to_image(cmd, staging, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[
            vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1))
                .image_extent(vk::Extent3D { width: w, height: h, depth: 1 }),
        ]);
        device.cmd_pipeline_barrier(cmd,
            vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(), &[], &[], &[
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(image).subresource_range(img_range()),
        ]);
    }
    one_shot_end(device, pool, queue, cmd)?;
    Ok(())
}

fn patch_image(
    device: &ash::Device, stage: &Staging,
    queue: vk::Queue, pool: vk::CommandPool,
    image: vk::Image, w: u32, h: u32, px: u32, py: u32, rgba: &[u8],
) -> Result<(), String> {
    let size = rgba.len() as vk::DeviceSize;
    debug_assert!(stage.size >= size, "staging buffer too small — call ensure_staging first");
    let staging = stage.buf;
    unsafe {
        let ptr = device.map_memory(stage.mem, 0, size, vk::MemoryMapFlags::empty())
            .map_err(|e| format!("{e:?}"))?;
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), ptr.cast::<u8>(), rgba.len());
        device.unmap_memory(stage.mem);
    }
    let cmd = one_shot_begin(device, pool)?;
    unsafe {
        device.cmd_pipeline_barrier(cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(), &[], &[], &[
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .image(image).subresource_range(img_range()),
        ]);
        device.cmd_copy_buffer_to_image(cmd, staging, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[
            vk::BufferImageCopy { buffer_offset: 0, buffer_row_length: 0, buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1),
                image_offset: vk::Offset3D { x: px as i32, y: py as i32, z: 0 },
                image_extent: vk::Extent3D { width: w, height: h, depth: 1 },
            },
        ]);
        device.cmd_pipeline_barrier(cmd,
            vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(), &[], &[], &[
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(image).subresource_range(img_range()),
        ]);
    }
    one_shot_end(device, pool, queue, cmd)?;
    Ok(())
}

fn destroy_tex(device: &ash::Device, t: GpuTex) {
    unsafe {
        device.destroy_image_view(t.view, None);
        device.destroy_image(t.image, None);
        device.free_memory(t.memory, None);
        device.destroy_sampler(t.sampler, None);
    }
}

/// Grow `stage` to hold at least `need` bytes. Never shrinks, so a session
/// settles on one allocation sized to the largest upload it has seen.
fn ensure_staging(
    device:    &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    stage:     &mut Staging,
    need:      vk::DeviceSize,
) -> Result<(), String> {
    if stage.size >= need && stage.size > 0 { return Ok(()); }
    // Round up to a power of two (min 1 MiB) so a slowly-growing atlas doesn't
    // reallocate on every patch.
    let cap = need.max(1024 * 1024).next_power_of_two();
    if stage.size > 0 {
        unsafe {
            device.destroy_buffer(stage.buf, None);
            device.free_memory(stage.mem, None);
        }
    }
    let (buf, mem) = host_buffer(device, mem_props, cap, vk::BufferUsageFlags::TRANSFER_SRC)?;
    *stage = Staging { buf, mem, size: cap };
    Ok(())
}

fn host_buffer(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory), String> {
    let buf = unsafe { device.create_buffer(
        &vk::BufferCreateInfo::default().size(size).usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE), None,
    )}.map_err(|e| format!("buffer: {e:?}"))?;
    let req = unsafe { device.get_buffer_memory_requirements(buf) };
    let mt  = find_memory_type(mem_props, req.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
        .ok_or("no host memory")?;
    let mem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(mt), None,
    )}.map_err(|e| format!("buf mem: {e:?}"))?;
    unsafe { device.bind_buffer_memory(buf, mem, 0) }.map_err(|e| format!("{e:?}"))?;
    Ok((buf, mem))
}

unsafe fn upload_bytes(device: &ash::Device, mem: vk::DeviceMemory, data: &[u8]) {
    if data.is_empty() { return; }
    unsafe {
        let ptr = device.map_memory(mem, 0, data.len() as u64, vk::MemoryMapFlags::empty())
            .expect("map");
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len());
        device.unmap_memory(mem);
    }
}

fn one_shot_begin(device: &ash::Device, pool: vk::CommandPool) -> Result<vk::CommandBuffer, String> {
    let cmd = unsafe { device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(pool).level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1),
    )}.map_err(|e| format!("{e:?}"))?[0];
    unsafe { device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default()
        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)) }
        .map_err(|e| format!("{e:?}"))?;
    Ok(cmd)
}

fn one_shot_end(
    device: &ash::Device, pool: vk::CommandPool,
    queue: vk::Queue, cmd: vk::CommandBuffer,
) -> Result<(), String> {
    unsafe {
        device.end_command_buffer(cmd).map_err(|e| format!("{e:?}"))?;
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)
            .map_err(|e| format!("{e:?}"))?;
        device.queue_submit(queue, &[
            vk::SubmitInfo::default().command_buffers(&[cmd]),
        ], fence).map_err(|e| format!("{e:?}"))?;
        let _ = device.wait_for_fences(&[fence], true, u64::MAX);
        device.destroy_fence(fence, None);
        device.free_command_buffers(pool, &[cmd]);
    }
    Ok(())
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits:  u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0 &&
        props.memory_types[i as usize].property_flags.contains(flags)
    })
}

fn img_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1).layer_count(1)
}

fn clip_rect(rect: &egui::Rect, ppp: f32, extent: vk::Extent2D) -> vk::Rect2D {
    let x      = (rect.min.x * ppp).round().max(0.0) as i32;
    let y      = (rect.min.y * ppp).round().max(0.0) as i32;
    let right  = (rect.max.x * ppp).round() as u32;
    let bottom = (rect.max.y * ppp).round() as u32;
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D {
            width:  right.saturating_sub(x as u32).min(extent.width),
            height: bottom.saturating_sub(y as u32).min(extent.height),
        },
    }
}

fn to_rgba(img: &egui::ImageData) -> Vec<u8> {
    match img {
        egui::ImageData::Color(c) => {
            c.pixels.iter().flat_map(|p| [p.r(), p.g(), p.b(), p.a()]).collect()
        }
        egui::ImageData::Font(f) => {
            f.pixels.iter().flat_map(|&a| {
                let v = (a * 255.0).round() as u8;
                [v, v, v, v]
            }).collect()
        }
    }
}
