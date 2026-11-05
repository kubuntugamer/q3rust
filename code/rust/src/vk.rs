//! Vulkan renderer core.
//!
//! This is the base the rest of the renderer is built on: a `VkInstance`, a
//! presentable surface for a native window, one graphics/present queue, and a
//! swapchain whose images the frame loop clears and presents. It knows
//! nothing about Quake yet; `begin_registration`/`render_scene` from
//! `refexport` will drive it once the draw passes land.
//!
//! The window and its Vulkan surface come from SDL, which is the only
//! platform layer in the engine. There is no GLX, no GL context and no Xlib
//! call in here.

use std::ffi::{CStr, CString};

use ash::{vk, Device, Entry, Instance};

/// A swapchain plus everything needed to clear and present one frame.
pub struct Vulkan {
    _entry: Entry,
    instance: Instance,
    surface: vk::SurfaceKHR,
    surface_loader: ash::khr::surface::Instance,
    physical_device: vk::PhysicalDevice,
    device: Device,
    queue: vk::Queue,
    swapchain_loader: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    image_views: Vec<vk::ImageView>,
    render_pass: vk::RenderPass,
    framebuffers: Vec<vk::Framebuffer>,
    command_pool: vk::CommandPool,
    command_buffers: Vec<vk::CommandBuffer>,
    image_available: vk::Semaphore,
    render_finished: Vec<vk::Semaphore>,
    in_flight: vk::Fence,
    descriptor_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    /// `blendFunc add`, for shader stages that sum with the frame.
    additive_pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    /// `GL_REPEAT`, for shader stages whose `tcMod`s run past 1.0.
    repeat_sampler: vk::Sampler,
    vertex_buffer: vk::Buffer,
    vertex_memory: vk::DeviceMemory,
    vertex_mapped: *mut u8,
    vertex_capacity: usize,
    frame_vertices: u32,
    current_image: Option<u32>,
}

/// The mapping the 2D vertex shader applies to engine pixels.
#[repr(C)]
#[derive(Clone, Copy)]
struct Mapping {
    scale: [f32; 2],
    offset: [f32; 2],
}

impl Mapping {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: the struct is `repr(C)` f32 fields with no padding, so all
        // sixteen bytes are initialised.
        unsafe {
            std::slice::from_raw_parts(self as *const Mapping as *const u8, 16)
        }
    }
}

/// One vertex of the 2D batch: screen pixels, texture coordinates, and the
/// colour the shader multiplies with the texel.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Vertex2D {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}

/// A texture the renderer can draw with. It is owned by the renderer it was
/// created from, and every handle has to be released before that renderer.
pub struct Texture {
    view: vk::ImageView,
    descriptor: vk::DescriptorSet,
    width: u32,
    height: u32,
}

impl Texture {
    /// Pixel size of the texture, which is what `shader_t` needs for its
    /// default texture coordinates.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Vertices a frame can hold before the batch buffer is grown.
const INITIAL_VERTEX_CAPACITY: usize = 4096;
/// Textures one renderer can hold at once; the game's media pool is far below.
const MAX_TEXTURES: u32 = 1024;

impl Vulkan {
    /// Creates the device and swapchain for a surface SDL already created.
    pub fn new(
        entry: Entry,
        instance: Instance,
        surface: vk::SurfaceKHR,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        unsafe {
            let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);

            let (physical_device, queue_family) = pick_device(&instance, &surface_loader, surface)?;

            let priorities = [1.0f32];
            let queue_info = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&priorities)];
            let device_extensions = [ash::khr::swapchain::NAME.as_ptr()];
            let device_info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_info)
                .enabled_extension_names(&device_extensions);
            let device = instance
                .create_device(physical_device, &device_info, None)
                .map_err(|e| format!("creating Vulkan device: {e}"))?;
            let queue = device.get_device_queue(queue_family, 0);
            let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);

            let mut vk = Self {
                _entry: entry,
                instance,
                surface,
                surface_loader,
                physical_device,
                device,
                queue,
                swapchain_loader,
                swapchain: vk::SwapchainKHR::null(),
                format: vk::Format::UNDEFINED,
                extent: vk::Extent2D { width, height },
                images: Vec::new(),
                image_views: Vec::new(),
                render_pass: vk::RenderPass::null(),
                framebuffers: Vec::new(),
                command_pool: vk::CommandPool::null(),
                command_buffers: Vec::new(),
                image_available: vk::Semaphore::null(),
                render_finished: Vec::new(),
                in_flight: vk::Fence::null(),
                descriptor_layout: vk::DescriptorSetLayout::null(),
                descriptor_pool: vk::DescriptorPool::null(),
                pipeline_layout: vk::PipelineLayout::null(),
                pipeline: vk::Pipeline::null(),
                additive_pipeline: vk::Pipeline::null(),
                sampler: vk::Sampler::null(),
                repeat_sampler: vk::Sampler::null(),
                vertex_buffer: vk::Buffer::null(),
                vertex_memory: vk::DeviceMemory::null(),
                vertex_mapped: std::ptr::null_mut(),
                vertex_capacity: 0,
                frame_vertices: 0,
                current_image: None,
            };

            vk.create_swapchain(vk::SwapchainKHR::null())?;
            vk.create_render_pass()?;
            vk.create_framebuffers()?;
            vk.create_command_pool()?;
            vk.create_sync()?;
            vk.create_descriptor_resources()?;
            vk.create_pipeline()?;
            vk.create_vertex_buffer(INITIAL_VERTEX_CAPACITY)?;
            Ok(vk)
        }
    }

    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Name of the physical device in use, for the console's renderer line.
    pub fn device_name(&self) -> String {
        unsafe {
            let properties = self
                .instance
                .get_physical_device_properties(self.physical_device);
            std::ffi::CStr::from_ptr(properties.device_name.as_ptr())
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Starts a frame: acquires a swapchain image, opens the render pass and
    /// sets the dynamic viewport. Drawing happens between this and
    /// [`Vulkan::end_frame`]. Returns `false` when the surface was rebuilt and
    /// the caller should skip this frame's drawing.
    pub fn begin_frame(&mut self, color: [f32; 4]) -> Result<bool, String> {
        unsafe {
            self.device
                .wait_for_fences(&[self.in_flight], true, u64::MAX)
                .map_err(|e| format!("waiting for the frame fence: {e}"))?;

            let (image_index, _suboptimal) = match self.swapchain_loader.acquire_next_image(
                self.swapchain,
                u64::MAX,
                self.image_available,
                vk::Fence::null(),
            ) {
                Ok(value) => value,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.recreate()?;
                    return Ok(false);
                }
                Err(e) => return Err(format!("acquiring a swapchain image: {e}")),
            };

            self.device
                .reset_fences(&[self.in_flight])
                .map_err(|e| format!("resetting the frame fence: {e}"))?;

            let command_buffer = self.command_buffers[image_index as usize];
            self.device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("resetting the command buffer: {e}"))?;

            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(|e| format!("beginning the command buffer: {e}"))?;

            let clears = [vk::ClearValue {
                color: vk::ClearColorValue { float32: color },
            }];
            let render_area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent,
            };
            let render_pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffers[image_index as usize])
                .render_area(render_area)
                .clear_values(&clears);
            self.device.cmd_begin_render_pass(
                command_buffer,
                &render_pass_begin,
                vk::SubpassContents::INLINE,
            );

            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: self.extent.width as f32,
                height: self.extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            self.device.cmd_set_viewport(command_buffer, 0, &[viewport]);
            self.device
                .cmd_set_scissor(command_buffer, 0, &[render_area]);

            self.frame_vertices = 0;
            self.current_image = Some(image_index);
            Ok(true)
        }
    }

    /// Closes the render pass, submits the frame and presents it.
    pub fn end_frame(&mut self) -> Result<(), String> {
        let image_index = match self.current_image.take() {
            Some(index) => index,
            None => return Err("end_frame called without begin_frame".into()),
        };
        unsafe {
            let command_buffer = self.command_buffers[image_index as usize];
            self.device.cmd_end_render_pass(command_buffer);
            self.device
                .end_command_buffer(command_buffer)
                .map_err(|e| format!("ending the command buffer: {e}"))?;

            let wait_semaphores = [self.image_available];
            let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let signal_semaphores = [self.render_finished[image_index as usize]];
            let command_buffers = [command_buffer];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_semaphores)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&command_buffers)
                .signal_semaphores(&signal_semaphores);
            self.device
                .queue_submit(self.queue, &[submit], self.in_flight)
                .map_err(|e| format!("submitting the frame: {e}"))?;

            let swapchains = [self.swapchain];
            let image_indices = [image_index];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signal_semaphores)
                .swapchains(&swapchains)
                .image_indices(&image_indices);
            let presented = self.swapchain_loader.queue_present(self.queue, &present);

            if matches!(presented, Err(vk::Result::ERROR_OUT_OF_DATE_KHR)) {
                self.recreate()?;
            } else if let Err(e) = presented {
                return Err(format!("presenting the frame: {e}"));
            }

            Ok(())
        }
    }

    /// Clears the surface to `color` and presents one frame.
    pub fn draw_frame(&mut self, color: [f32; 4]) -> Result<(), String> {
        if self.begin_frame(color)? {
            self.end_frame()?;
        }
        Ok(())
    }

    /// Replaces the swapchain after the window changed size. A zero-sized
    /// window is ignored: there is nothing to present until it comes back.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 || (width == self.extent.width && height == self.extent.height)
        {
            return Ok(());
        }
        self.extent = vk::Extent2D { width, height };
        self.recreate()
    }

    fn create_swapchain(&mut self, old: vk::SwapchainKHR) -> Result<(), String> {
        unsafe {
            let caps = self
                .surface_loader
                .get_physical_device_surface_capabilities(self.physical_device, self.surface)
                .map_err(|e| format!("querying surface capabilities: {e}"))?;
            let formats = self
                .surface_loader
                .get_physical_device_surface_formats(self.physical_device, self.surface)
                .map_err(|e| format!("querying surface formats: {e}"))?;
            let present_modes = self
                .surface_loader
                .get_physical_device_surface_present_modes(self.physical_device, self.surface)
                .map_err(|e| format!("querying present modes: {e}"))?;

            let format = formats
                .iter()
                .find(|f| {
                    f.format == vk::Format::B8G8R8A8_SRGB
                        && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                })
                .or_else(|| formats.first())
                .copied()
                .ok_or("the surface reports no formats")?;

            let present_mode = if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
                vk::PresentModeKHR::MAILBOX
            } else {
                vk::PresentModeKHR::FIFO
            };

            // current_extent == (u32::MAX, u32::MAX) means "use what the
            // window says", clamped to what the surface allows.
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: self
                        .extent
                        .width
                        .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: self
                        .extent
                        .height
                        .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };

            let mut image_count = caps.min_image_count + 1;
            if caps.max_image_count > 0 && image_count > caps.max_image_count {
                image_count = caps.max_image_count;
            }

            let composite_alpha = if caps
                .supported_composite_alpha
                .contains(vk::CompositeAlphaFlagsKHR::OPAQUE)
            {
                vk::CompositeAlphaFlagsKHR::OPAQUE
            } else {
                vk::CompositeAlphaFlagsKHR::INHERIT
            };

            let info = vk::SwapchainCreateInfoKHR::default()
                .surface(self.surface)
                .min_image_count(image_count)
                .image_format(format.format)
                .image_color_space(format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(composite_alpha)
                .present_mode(present_mode)
                .clipped(true)
                .old_swapchain(old);

            let swapchain = self
                .swapchain_loader
                .create_swapchain(&info, None)
                .map_err(|e| format!("creating the swapchain: {e}"))?;

            self.swapchain = swapchain;
            self.format = format.format;
            self.extent = extent;
            self.images = self
                .swapchain_loader
                .get_swapchain_images(swapchain)
                .map_err(|e| format!("getting swapchain images: {e}"))?;

            self.image_views = self
                .images
                .iter()
                .map(|image| {
                    let view_info = vk::ImageViewCreateInfo::default()
                        .image(*image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.format)
                        .components(vk::ComponentMapping::default())
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .base_mip_level(0)
                                .level_count(1)
                                .base_array_layer(0)
                                .layer_count(1),
                        );
                    self.device
                        .create_image_view(&view_info, None)
                        .map_err(|e| format!("creating a swapchain image view: {e}"))
                })
                .collect::<Result<Vec<_>, String>>()?;

            if old != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(old, None);
            }

            Ok(())
        }
    }

    fn create_framebuffers(&mut self) -> Result<(), String> {
        unsafe {
            self.framebuffers = self
                .image_views
                .iter()
                .map(|view| {
                    let attachments = [*view];
                    let framebuffer_info = vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&attachments)
                        .width(self.extent.width)
                        .height(self.extent.height)
                        .layers(1);
                    self.device
                        .create_framebuffer(&framebuffer_info, None)
                        .map_err(|e| format!("creating a framebuffer: {e}"))
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(())
        }
    }

    fn create_render_pass(&mut self) -> Result<(), String> {
        unsafe {
            let attachment = vk::AttachmentDescription::default()
                .format(self.format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);

            let color_attachment = vk::AttachmentReference::default()
                .attachment(0)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

            let color_attachments = [color_attachment];
            let subpass = vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_attachments);

            let dependency = vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

            let attachments = [attachment];
            let subpasses = [subpass];
            let dependencies = [dependency];
            let render_pass_info = vk::RenderPassCreateInfo::default()
                .attachments(&attachments)
                .subpasses(&subpasses)
                .dependencies(&dependencies);
            self.render_pass = self
                .device
                .create_render_pass(&render_pass_info, None)
                .map_err(|e| format!("creating the render pass: {e}"))?;
            Ok(())
        }
    }

    fn create_command_pool(&mut self) -> Result<(), String> {
        unsafe {
            let family = self.queue_family_index()?;
            let pool_info = vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            self.command_pool = self
                .device
                .create_command_pool(&pool_info, None)
                .map_err(|e| format!("creating the command pool: {e}"))?;

            let allocate_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(self.images.len() as u32);
            self.command_buffers = self
                .device
                .allocate_command_buffers(&allocate_info)
                .map_err(|e| format!("allocating command buffers: {e}"))?;
            Ok(())
        }
    }

    fn create_sync(&mut self) -> Result<(), String> {
        unsafe {
            let semaphore_info = vk::SemaphoreCreateInfo::default();
            self.image_available = self
                .device
                .create_semaphore(&semaphore_info, None)
                .map_err(|e| format!("creating the image-available semaphore: {e}"))?;
            self.render_finished = (0..self.images.len())
                .map(|_| {
                    self.device
                        .create_semaphore(&semaphore_info, None)
                        .map_err(|e| format!("creating a render-finished semaphore: {e}"))
                })
                .collect::<Result<Vec<_>, String>>()?;

            let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
            self.in_flight = self
                .device
                .create_fence(&fence_info, None)
                .map_err(|e| format!("creating the frame fence: {e}"))?;
            Ok(())
        }
    }

    fn recreate(&mut self) -> Result<(), String> {
        unsafe {
            self.device
                .device_wait_idle()
                .map_err(|e| format!("waiting for the device: {e}"))?;

            // Views must go before the swapchain they came from, and the
            // framebuffers before the views.
            for framebuffer in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(framebuffer, None);
            }
            for view in self.image_views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            self.destroy_frame_objects();

            let old = self.swapchain;
            self.swapchain = vk::SwapchainKHR::null();
            self.create_swapchain(old)?;

            // The new surface may pick a different format, so the render
            // pass, its pipeline and its framebuffers are rebuilt from scratch.
            if self.render_pass != vk::RenderPass::null() {
                // The pipeline references the render pass it was built for.
                if self.pipeline != vk::Pipeline::null() {
                    self.device.destroy_pipeline(self.pipeline, None);
                    self.pipeline = vk::Pipeline::null();
                }
                self.device.destroy_render_pass(self.render_pass, None);
                self.render_pass = vk::RenderPass::null();
            }
            self.create_render_pass()?;
            self.create_pipeline()?;
            self.create_framebuffers()?;
            self.create_command_pool()?;
            self.create_sync()?;
            Ok(())
        }
    }

    /// Destroys per-frame sync and command state, but not the images.
    fn destroy_frame_objects(&mut self) {
        unsafe {
            for semaphore in self.render_finished.drain(..) {
                self.device.destroy_semaphore(semaphore, None);
            }
            if self.image_available != vk::Semaphore::null() {
                self.device.destroy_semaphore(self.image_available, None);
                self.image_available = vk::Semaphore::null();
            }
            if self.in_flight != vk::Fence::null() {
                self.device.destroy_fence(self.in_flight, None);
                self.in_flight = vk::Fence::null();
            }
            if self.command_pool != vk::CommandPool::null() {
                self.device.destroy_command_pool(self.command_pool, None);
                self.command_pool = vk::CommandPool::null();
                self.command_buffers.clear();
            }
            self.images.clear();
        }
    }

    fn destroy_swapchain_objects(&mut self) {
        for framebuffer in self.framebuffers.drain(..) {
            unsafe { self.device.destroy_framebuffer(framebuffer, None) };
        }
        for view in self.image_views.drain(..) {
            unsafe { self.device.destroy_image_view(view, None) };
        }
        self.destroy_frame_objects();
    }

    fn queue_family_index(&self) -> Result<u32, String> {
        unsafe {
            let families = self
                .instance
                .get_physical_device_queue_family_properties(self.physical_device);
            families
                .iter()
                .enumerate()
                .find(|(index, family)| {
                    family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && self
                            .surface_loader
                            .get_physical_device_surface_support(
                                self.physical_device,
                                *index as u32,
                                self.surface,
                            )
                            .unwrap_or(false)
                })
                .map(|(index, _)| index as u32)
                .ok_or_else(|| "no graphics queue can present to this surface".into())
        }
    }
}

/// Drawing, texturing and the per-frame vertex batch.
impl Vulkan {
    /// Uploads RGBA pixels and returns a drawable texture.
    pub fn create_texture(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<Texture, String> {
        self.create_texture_with_wrap(width, height, rgba, false)
    }

    /// Uploads RGBA pixels with a sampler that wraps texture coordinates, as
    /// an unclamped shader image does.
    pub fn create_repeating_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<Texture, String> {
        self.create_texture_with_wrap(width, height, rgba, true)
    }

    fn create_texture_with_wrap(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        wrap: bool,
    ) -> Result<Texture, String> {
        if width == 0 || height == 0 {
            return Err("a texture needs a non-zero width and height".into());
        }
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            return Err(format!(
                "a {width}x{height} texture needs {expected} bytes of RGBA, got {}",
                rgba.len()
            ));
        }

        unsafe {
            // Stage the pixels in host memory; the upload itself runs on the
            // queue, so the mapping never survives this function.
            let staging_info = vk::BufferCreateInfo::default()
                .size(rgba.len() as vk::DeviceSize)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let staging = self
                .device
                .create_buffer(&staging_info, None)
                .map_err(|e| format!("creating texture staging memory: {e}"))?;
            let staging_requirements = self.device.get_buffer_memory_requirements(staging);
            let staging_type = self.find_memory_type(
                staging_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            let staging_alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(staging_requirements.size)
                .memory_type_index(staging_type);
            let staging_memory = self
                .device
                .allocate_memory(&staging_alloc, None)
                .map_err(|e| format!("allocating texture staging memory: {e}"))?;
            self.device
                .bind_buffer_memory(staging, staging_memory, 0)
                .map_err(|e| format!("binding texture staging memory: {e}"))?;
            let mapped = self
                .device
                .map_memory(staging_memory, 0, rgba.len() as vk::DeviceSize, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("mapping texture staging memory: {e}"))? as *mut u8;
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), mapped, rgba.len());
            self.device.unmap_memory(staging_memory);

            let image_info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let image = self
                .device
                .create_image(&image_info, None)
                .map_err(|e| format!("creating a texture image: {e}"))?;
            let requirements = self.device.get_image_memory_requirements(image);
            let memory_type = self.find_memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type);
            let memory = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("allocating texture memory: {e}"))?;
            self.device
                .bind_image_memory(image, memory, 0)
                .map_err(|e| format!("binding texture memory: {e}"))?;

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });

            let range = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            self.one_time(|device, command_buffer| {
                let to_transfer = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
                device.cmd_pipeline_barrier(
                    command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_transfer],
                );
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
                let to_shader = vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(range)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ);
                device.cmd_pipeline_barrier(
                    command_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_shader],
                );
            })?;

            self.device.destroy_buffer(staging, None);
            self.device.free_memory(staging_memory, None);

            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .subresource_range(range);
            let view = self
                .device
                .create_image_view(&view_info, None)
                .map_err(|e| format!("creating a texture view: {e}"))?;

            let sets = self
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.descriptor_pool)
                        .set_layouts(&[self.descriptor_layout]),
                )
                .map_err(|e| format!("allocating a texture descriptor set: {e}"))?;
            let descriptor = sets[0];
            let image_info = vk::DescriptorImageInfo::default()
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image_view(view)
                .sampler(if wrap { self.repeat_sampler } else { self.sampler });
            let write = vk::WriteDescriptorSet::default()
                .dst_set(descriptor)
                .dst_binding(0)
                .dst_array_element(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&image_info));
            self.device.update_descriptor_sets(&[write], &[]);

            Ok(Texture {
                view,
                descriptor,
                width,
                height,
            })
        }
    }

    /// Releases a texture. The caller must not use the handle afterwards.
    pub fn destroy_texture(&mut self, texture: Texture) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_image_view(texture.view, None);
            self.device
                .free_descriptor_sets(self.descriptor_pool, &[texture.descriptor])
                .ok();
        }
    }

    /// Adds one textured batch to the frame being recorded, blended as
    /// `blendFunc blend`.
    pub fn draw_quads(&mut self, texture: &Texture, vertices: &[Vertex2D]) -> Result<(), String> {
        self.draw_quads_with(texture, vertices, self.pipeline)
    }

    /// Adds one textured batch that sums with the frame, as `blendFunc add`.
    pub fn draw_quads_additive(
        &mut self,
        texture: &Texture,
        vertices: &[Vertex2D],
    ) -> Result<(), String> {
        self.draw_quads_with(texture, vertices, self.additive_pipeline)
    }

    fn draw_quads_with(
        &mut self,
        texture: &Texture,
        vertices: &[Vertex2D],
        pipeline: vk::Pipeline,
    ) -> Result<(), String> {
        if vertices.is_empty() {
            return Ok(());
        }
        let image_index = match self.current_image {
            Some(index) => index,
            None => return Err("draw_quads called outside of a frame".into()),
        };

        let vertex_size = std::mem::size_of::<Vertex2D>();
        if self.frame_vertices as usize + vertices.len() > self.vertex_capacity {
            let mut capacity = self.vertex_capacity.max(INITIAL_VERTEX_CAPACITY);
            while capacity < self.frame_vertices as usize + vertices.len() {
                capacity *= 2;
            }
            self.create_vertex_buffer(capacity)?;
        }

        let offset = self.frame_vertices as usize * vertex_size;
        let first = self.frame_vertices;
        unsafe {
            std::ptr::copy_nonoverlapping(
                vertices.as_ptr() as *const u8,
                self.vertex_mapped.add(offset),
                vertices.len() * vertex_size,
            );
        }
        self.frame_vertices += vertices.len() as u32;

        let command_buffer = self.command_buffers[image_index as usize];
        // Vulkan's clip space has +Y pointing down, so engine pixels (origin
        // top left, Y down) map straight through with a positive Y scale.
        // GL's flipped Y is the one thing the old 2D projection was hiding.
        let mapping = Mapping {
            scale: [
                2.0 / self.extent.width as f32,
                2.0 / self.extent.height as f32,
            ],
            offset: [-1.0, -1.0],
        };
        unsafe {
            self.device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline,
            );
            // The buffer is bound from its start and `first_vertex` selects
            // this batch: doing both would double the offset and read another
            // draw's vertices.
            self.device.cmd_bind_vertex_buffers(
                command_buffer,
                0,
                &[self.vertex_buffer],
                &[0],
            );
            self.device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[texture.descriptor],
                &[],
            );
            self.device.cmd_push_constants(
                command_buffer,
                self.pipeline_layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                mapping.as_bytes(),
            );
            self.device.cmd_draw(
                command_buffer,
                vertices.len() as u32,
                1,
                first,
                0,
            );
        }
        Ok(())
    }

    fn create_descriptor_resources(&mut self) -> Result<(), String> {
        unsafe {
            let binding = vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT);
            self.descriptor_layout = self
                .device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[binding]),
                    None,
                )
                .map_err(|e| format!("creating the texture descriptor layout: {e}"))?;

            let pool_sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(MAX_TEXTURES)];
            self.descriptor_pool = self
                .device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(MAX_TEXTURES)
                        .pool_sizes(&pool_sizes),
                    None,
                )
                .map_err(|e| format!("creating the texture descriptor pool: {e}"))?;

            let sampler_info = vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .max_lod(0.0);
            self.sampler = self
                .device
                .create_sampler(&sampler_info, None)
                .map_err(|e| format!("creating the texture sampler: {e}"))?;

            // Shader images are unclamped unless `clampmap` is given, so a
            // scrolled or scaled stage needs the coordinates to wrap.
            let repeat_info = vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::REPEAT)
                .address_mode_v(vk::SamplerAddressMode::REPEAT)
                .address_mode_w(vk::SamplerAddressMode::REPEAT)
                .max_lod(0.0);
            self.repeat_sampler = self
                .device
                .create_sampler(&repeat_info, None)
                .map_err(|e| format!("creating the repeating texture sampler: {e}"))?;

            let push_range = vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX)
                .offset(0)
                .size(16);
            self.pipeline_layout = self
                .device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[self.descriptor_layout])
                        .push_constant_ranges(&[push_range]),
                    None,
                )
                .map_err(|e| format!("creating the 2D pipeline layout: {e}"))?;
            Ok(())
        }
    }

    /// Builds the 2D pipeline against the current render pass. The render pass
    /// is rebuilt on resize, so this runs again with it.
    fn create_pipeline(&mut self) -> Result<(), String> {
        if self.pipeline != vk::Pipeline::null() {
            unsafe {
                self.device.destroy_pipeline(self.pipeline, None);
                self.pipeline = vk::Pipeline::null();
            }
        }
        if self.additive_pipeline != vk::Pipeline::null() {
            unsafe {
                self.device.destroy_pipeline(self.additive_pipeline, None);
                self.additive_pipeline = vk::Pipeline::null();
            }
        }

        let vertex_code = read_spirv(include_bytes!(concat!(env!("OUT_DIR"), "/q3_2d.vert.spv")))?;
        let fragment_code = read_spirv(include_bytes!(concat!(env!("OUT_DIR"), "/q3_2d.frag.spv")))?;
        let entry = CStr::from_bytes_with_nul(b"main\0").unwrap();

        unsafe {
            let vertex_module = self
                .device
                .create_shader_module(
                    &vk::ShaderModuleCreateInfo::default().code(&vertex_code),
                    None,
                )
                .map_err(|e| format!("creating the 2D vertex module: {e}"))?;
            let fragment_module = self
                .device
                .create_shader_module(
                    &vk::ShaderModuleCreateInfo::default().code(&fragment_code),
                    None,
                )
                .map_err(|e| format!("creating the 2D fragment module: {e}"))?;

            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vertex_module)
                    .name(entry),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fragment_module)
                    .name(entry),
            ];

            let bindings = [vk::VertexInputBindingDescription::default()
                .binding(0)
                .stride(std::mem::size_of::<Vertex2D>() as u32)
                .input_rate(vk::VertexInputRate::VERTEX)];
            let attributes = [
                vk::VertexInputAttributeDescription::default()
                    .location(0)
                    .binding(0)
                    .format(vk::Format::R32G32_SFLOAT)
                    .offset(0),
                vk::VertexInputAttributeDescription::default()
                    .location(1)
                    .binding(0)
                    .format(vk::Format::R32G32_SFLOAT)
                    .offset(8),
                vk::VertexInputAttributeDescription::default()
                    .location(2)
                    .binding(0)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .offset(16),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
                .vertex_binding_descriptions(&bindings)
                .vertex_attribute_descriptions(&attributes);

            let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);

            // Quake's blendfunc blend: src alpha, one minus src alpha. Opaque
            // stages draw with alpha 1 through the same pipeline. blendFunc
            // add needs its own pipeline, since blending is fixed state here.
            let attachments = [
                vk::PipelineColorBlendAttachmentState::default()
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                    .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ONE)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .alpha_blend_op(vk::BlendOp::ADD)
                    .color_write_mask(vk::ColorComponentFlags::RGBA),
                vk::PipelineColorBlendAttachmentState::default()
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::ONE)
                    .dst_color_blend_factor(vk::BlendFactor::ONE)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ONE)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE)
                    .alpha_blend_op(vk::BlendOp::ADD)
                    .color_write_mask(vk::ColorComponentFlags::RGBA),
            ];

            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic = vk::PipelineDynamicStateCreateInfo::default()
                .dynamic_states(&dynamic_states);

            let mut created: Vec<vk::Pipeline> = Vec::with_capacity(attachments.len());
            for attachment in &attachments {
                let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
                    .attachments(std::slice::from_ref(attachment));
                let info = vk::GraphicsPipelineCreateInfo::default()
                    .stages(&stages)
                    .vertex_input_state(&vertex_input)
                    .input_assembly_state(&input_assembly)
                    .viewport_state(&viewport_state)
                    .rasterization_state(&rasterization)
                    .multisample_state(&multisample)
                    .color_blend_state(&color_blend)
                    .dynamic_state(&dynamic)
                    .layout(self.pipeline_layout)
                    .render_pass(self.render_pass)
                    .subpass(0);

                match self
                    .device
                    .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                {
                    Ok(pipelines) => created.push(pipelines[0]),
                    Err((_, e)) => {
                        for pipeline in created {
                            self.device.destroy_pipeline(pipeline, None);
                        }
                        self.device.destroy_shader_module(vertex_module, None);
                        self.device.destroy_shader_module(fragment_module, None);
                        return Err(format!("creating the 2D pipeline: {e}"));
                    }
                }
            }

            self.device.destroy_shader_module(vertex_module, None);
            self.device.destroy_shader_module(fragment_module, None);

            self.pipeline = created[0];
            self.additive_pipeline = created[1];
            Ok(())
        }
    }

    fn create_vertex_buffer(&mut self, capacity: usize) -> Result<(), String> {
        unsafe {
            let size = (capacity * std::mem::size_of::<Vertex2D>()) as vk::DeviceSize;
            let buffer_info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let buffer = self
                .device
                .create_buffer(&buffer_info, None)
                .map_err(|e| format!("creating the 2D vertex buffer: {e}"))?;
            let requirements = self.device.get_buffer_memory_requirements(buffer);
            let memory_type = self.find_memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type);
            let memory = self
                .device
                .allocate_memory(&alloc, None)
                .map_err(|e| format!("allocating the 2D vertex buffer memory: {e}"))?;
            self.device
                .bind_buffer_memory(buffer, memory, 0)
                .map_err(|e| format!("binding the 2D vertex buffer: {e}"))?;
            let mapped = self
                .device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("mapping the 2D vertex buffer: {e}"))?
                as *mut u8;

            // The frame in progress keeps its vertices: copy them across, then
            // release the buffer they came from.
            if !self.vertex_mapped.is_null() && self.frame_vertices > 0 {
                std::ptr::copy_nonoverlapping(
                    self.vertex_mapped,
                    mapped,
                    self.frame_vertices as usize * std::mem::size_of::<Vertex2D>(),
                );
            }
            if !self.vertex_mapped.is_null() {
                self.device.unmap_memory(self.vertex_memory);
            }
            if self.vertex_buffer != vk::Buffer::null() {
                self.device.destroy_buffer(self.vertex_buffer, None);
            }
            if self.vertex_memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.vertex_memory, None);
            }

            self.vertex_buffer = buffer;
            self.vertex_memory = memory;
            self.vertex_mapped = mapped;
            self.vertex_capacity = capacity;
            Ok(())
        }
    }

    fn find_memory_type(
        &self,
        bits: u32,
        properties: vk::MemoryPropertyFlags,
    ) -> Result<u32, String> {
        let memory = unsafe { self.instance.get_physical_device_memory_properties(self.physical_device) };
        memory
            .memory_types
            .iter()
            .enumerate()
            .find(|(index, candidate)| {
                bits & (1 << index) != 0 && candidate.property_flags.contains(properties)
            })
            .map(|(index, _)| index as u32)
            .ok_or_else(|| format!("no memory type matches {properties:?}"))
    }

    /// Runs `record` in a one-shot command buffer and waits for it to finish.
    /// The device is handed to `record` so callers do not borrow the renderer.
    fn one_time(
        &mut self,
        record: impl FnOnce(&Device, vk::CommandBuffer),
    ) -> Result<(), String> {
        unsafe {
            let allocate = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let command_buffers = self
                .device
                .allocate_command_buffers(&allocate)
                .map_err(|e| format!("allocating a one-shot command buffer: {e}"))?;
            let command_buffer = command_buffers[0];

            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(|e| format!("beginning a one-shot command buffer: {e}"))?;
            record(&self.device, command_buffer);
            self.device
                .end_command_buffer(command_buffer)
                .map_err(|e| format!("ending a one-shot command buffer: {e}"))?;

            let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
            self.device
                .queue_submit(self.queue, &[submit], vk::Fence::null())
                .map_err(|e| format!("submitting a one-shot command buffer: {e}"))?;
            self.device
                .queue_wait_idle(self.queue)
                .map_err(|e| format!("waiting for a one-shot command buffer: {e}"))?;
            self.device
                .free_command_buffers(self.command_pool, &command_buffers);
            Ok(())
        }
    }
}

/// Creates the Vulkan instance with the extensions SDL says its window needs.
/// The entry point is returned alongside it so the renderer does not load
/// libvulkan a second time.
pub fn create_instance(extensions: &[&str]) -> Result<(Entry, Instance), String> {
    unsafe {
        let entry = Entry::load().map_err(|e| format!("loading libvulkan: {e}"))?;

        // The names have to outlive the instance creation call.
        let mut names = Vec::with_capacity(extensions.len());
        let mut pointers: Vec<*const i8> = Vec::with_capacity(extensions.len());
        for extension in extensions {
            let name = CString::new(*extension)
                .map_err(|_| format!("Vulkan extension name {extension:?} contains a NUL"))?;
            pointers.push(name.as_ptr());
            names.push(name);
        }

        let app_name = CStr::from_bytes_with_nul(b"q3rust\0").unwrap();
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(app_name)
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::make_api_version(0, 1, 3, 0));

        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&pointers);
        let instance = entry
            .create_instance(&instance_info, None)
            .map_err(|e| format!("creating the Vulkan instance: {e}"))?;
        Ok((entry, instance))
    }
}

fn read_spirv(bytes: &'static [u8]) -> Result<Vec<u32>, String> {
    ash::util::read_spv(&mut std::io::Cursor::new(bytes))
        .map_err(|e| format!("reading a compiled 2D shader: {e}"))
}

fn pick_device(
    instance: &Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32), String> {
    unsafe {
        let devices = instance
            .enumerate_physical_devices()
            .map_err(|e| format!("enumerating physical devices: {e}"))?;

        let mut best: Option<(vk::PhysicalDevice, u32, bool)> = None;
        for device in devices {
            let families = instance.get_physical_device_queue_family_properties(device);
            let present_family = families.iter().enumerate().find_map(|(index, family)| {
                if !family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                    return None;
                }
                let supports_present = surface_loader
                    .get_physical_device_surface_support(device, index as u32, surface)
                    .unwrap_or(false);
                let has_formats = !surface_loader
                    .get_physical_device_surface_formats(device, surface)
                    .map(|formats| formats.is_empty())
                    .unwrap_or(true);
                if supports_present && has_formats {
                    Some(index as u32)
                } else {
                    None
                }
            });

            if let Some(family) = present_family {
                let properties = instance.get_physical_device_properties(device);
                let discrete = properties.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
                if best.is_none() || (discrete && !best.as_ref().unwrap().2) {
                    best = Some((device, family, discrete));
                }
            }
        }

        best.map(|(device, family, _)| (device, family))
            .ok_or_else(|| "no Vulkan device can render to this window".into())
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.destroy_swapchain_objects();
            if self.pipeline != vk::Pipeline::null() {
                self.device.destroy_pipeline(self.pipeline, None);
            }
            if self.additive_pipeline != vk::Pipeline::null() {
                self.device.destroy_pipeline(self.additive_pipeline, None);
            }
            if self.pipeline_layout != vk::PipelineLayout::null() {
                self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            }
            if self.sampler != vk::Sampler::null() {
                self.device.destroy_sampler(self.sampler, None);
            }
            if self.repeat_sampler != vk::Sampler::null() {
                self.device.destroy_sampler(self.repeat_sampler, None);
            }
            if !self.vertex_mapped.is_null() {
                self.device.unmap_memory(self.vertex_memory);
            }
            if self.vertex_buffer != vk::Buffer::null() {
                self.device.destroy_buffer(self.vertex_buffer, None);
            }
            if self.vertex_memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.vertex_memory, None);
            }
            if self.descriptor_pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.descriptor_pool, None);
            }
            if self.descriptor_layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.descriptor_layout, None);
            }
            if self.render_pass != vk::RenderPass::null() {
                self.device.destroy_render_pass(self.render_pass, None);
            }
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(self.swapchain, None);
            }
            self.surface_loader.destroy_surface(self.surface, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
