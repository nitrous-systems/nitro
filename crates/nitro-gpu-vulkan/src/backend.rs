//! [`VkBackend`]: the `nitro_gpu::Backend` on Vulkan.
//!
//! Every call sequence follows `docs/research/gpu-testbox/vk_probe.c`,
//! which verified it on both test boxes (anv and hasvk): explicit-modifier
//! dma-buf import with a dedicated allocation, NV12 through a
//! `VkSamplerYcbcrConversion`, an exported render target restricted to the
//! plane's modifiers, `SYNC_FD` semaphores in and out, and the completion
//! fence attached to the output dma-buf with
//! `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`.
//!
//! Ownership of foreign buffers (client dma-bufs, the udmabuf shadow, the
//! output slots KMS scans out) moves to the helper's queue at the start of
//! every frame and back to `VK_QUEUE_FAMILY_FOREIGN_EXT` at its end, in
//! `GENERAL` layout.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

use ash::vk;
use nitro_core::IRect;
use nitro_gpu::proto::{
    AR24, DeviceInfo, DmabufDesc, ErrorCode, Layer, MOD_LINEAR, NV12, ShadowDesc, ShadowPath,
    SlotLayout, XR24,
};
use nitro_gpu::{Backend, BackendError, Readback, Ring, RingRequest};

use crate::device::{Gpu, vk_format};
use crate::pipeline::{FamilyKey, PUSH_SIZE, Pipelines, blend_index};
use crate::sys;

/// Environment switch: `NITRO_GPU_SHADOW=staging` skips udmabuf.
pub const SHADOW_ENV: &str = "NITRO_GPU_SHADOW";

/// Longest the helper waits on its own fences (they only guard reuse of
/// buffers whose frame the event loop already saw finish, or a debug op).
const FENCE_WAIT_NS: u64 = 2_000_000_000;

fn be(what: &'static str) -> impl Fn(vk::Result) -> BackendError {
    move |e| BackendError::new(format!("{what}: {e}"))
}

const COLOR: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// The CPU side of a shadow texture on the staging path.
struct Staging {
    map: nitro_shm::Mapping,
    stride: u32,
    buf: vk::Buffer,
    mem: vk::DeviceMemory,
    /// Persistently mapped `buf`, `len` bytes.
    ptr: *mut u8,
    len: usize,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// The image holds data (layout `SHADER_READ_ONLY_OPTIMAL`).
    ready: bool,
}

/// A texture.
pub struct Tex {
    image: vk::Image,
    mems: Vec<vk::DeviceMemory>,
    view: vk::ImageView,
    set: vk::DescriptorSet,
    layout: vk::PipelineLayout,
    pipes: [vk::Pipeline; 2],
    fourcc: u32,
    w: u32,
    h: u32,
    /// Owned by another device/process: acquire and release every frame.
    foreign: bool,
    staging: Option<Staging>,
}

struct Slot {
    image: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
    fb: vk::Framebuffer,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    done: vk::Semaphore,
    /// Acquire semaphores of the slot's last frame; destroyed on reuse.
    waits: Vec<vk::Semaphore>,
    /// Our reference to the exported dma-buf, for `IMPORT_SYNC_FILE`.
    dmabuf: Option<OwnedFd>,
    /// Never drawn: its contents are undefined.
    fresh: bool,
}

/// The Vulkan backend.
pub struct VkBackend {
    pipes: Pipelines,
    pool: vk::CommandPool,
    slots: Vec<Slot>,
    out_w: u32,
    out_h: u32,
    shadow_path: ShadowPath,
    force_staging: bool,
    // Last: dropped after everything created from it.
    gpu: Gpu,
}

fn barrier(
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    queues: (u32, u32),
    access: (vk::AccessFlags, vk::AccessFlags),
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .image(image)
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(queues.0)
        .dst_queue_family_index(queues.1)
        .src_access_mask(access.0)
        .dst_access_mask(access.1)
        .subresource_range(COLOR)
}

impl VkBackend {
    /// Open the device on `node` (the loader must already be restricted
    /// to the vendor ICD; see `main`).
    ///
    /// # Errors
    /// No usable device, or pipeline setup failed.
    pub fn open(node: &std::path::Path) -> Result<Self, String> {
        let gpu = Gpu::open(node)?;
        let mut pipes = Pipelines::new(&gpu).map_err(|e| e.msg)?;
        let pci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(gpu.qfi)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: valid create info for a queue family of this device.
        let pool = match unsafe { gpu.device.create_command_pool(&pci, None) } {
            Ok(p) => p,
            Err(e) => {
                pipes.destroy(&gpu);
                return Err(format!("vkCreateCommandPool: {e}"));
            }
        };
        Ok(Self {
            pipes,
            pool,
            slots: Vec::new(),
            out_w: 0,
            out_h: 0,
            shadow_path: ShadowPath::Unknown,
            force_staging: std::env::var(SHADOW_ENV).is_ok_and(|v| v == "staging"),
            gpu,
        })
    }

    fn cmd_buffer(&self) -> Result<vk::CommandBuffer, BackendError> {
        let ai = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: `pool` belongs to this device.
        let v = unsafe { self.gpu.device.allocate_command_buffers(&ai) }
            .map_err(be("vkAllocateCommandBuffers"))?;
        Ok(v[0])
    }

    fn fence(&self, signalled: bool) -> Result<vk::Fence, BackendError> {
        let flags = if signalled {
            vk::FenceCreateFlags::SIGNALED
        } else {
            vk::FenceCreateFlags::empty()
        };
        // SAFETY: valid create info.
        unsafe {
            self.gpu
                .device
                .create_fence(&vk::FenceCreateInfo::default().flags(flags), None)
        }
        .map_err(be("vkCreateFence"))
    }

    fn wait(&self, fence: vk::Fence) -> Result<(), BackendError> {
        // SAFETY: `fence` belongs to this device.
        unsafe {
            self.gpu
                .device
                .wait_for_fences(&[fence], true, FENCE_WAIT_NS)
        }
        .map_err(be("vkWaitForFences"))
    }

    /// Import `fd` as an image with explicit modifier and plane layouts.
    #[allow(clippy::many_single_char_names)]
    fn import_image(
        &self,
        format: vk::Format,
        (w, h): (u32, u32),
        modifier: u64,
        planes: &[(u32, u32)],
        fd: OwnedFd,
    ) -> Result<(vk::Image, vk::DeviceMemory), BackendError> {
        let d = &self.gpu.device;
        let layouts: Vec<vk::SubresourceLayout> = planes
            .iter()
            .map(|&(offset, pitch)| vk::SubresourceLayout {
                offset: u64::from(offset),
                row_pitch: u64::from(pitch),
                ..vk::SubresourceLayout::default()
            })
            .collect();
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&layouts);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext)
            .push_next(&mut explicit);
        // SAFETY: complete create info; the format/modifier pair was
        // advertised importable (validated before this call).
        let image = unsafe { d.create_image(&ci, None) }.map_err(|e| {
            BackendError::with_code(ErrorCode::BadFormat, format!("vkCreateImage: {e}"))
        })?;
        // SAFETY: `image` was just created and has no memory bound.
        let reqs = unsafe { d.get_image_memory_requirements(image) };
        let mut fdp = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: `fd` is a live descriptor we own for the whole call.
        let r = unsafe {
            self.gpu.mem_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fdp,
            )
        };
        let ty = r.ok().and_then(|()| {
            self.gpu.memtype(
                reqs.memory_type_bits & fdp.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        });
        let Some(ty) = ty else {
            // SAFETY: unused image.
            unsafe { d.destroy_image(image, None) };
            return Err(BackendError::with_code(
                ErrorCode::BadBuffer,
                "not an importable dma-buf",
            ));
        };
        let raw = fd.into_raw_fd();
        let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut imp = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(ty)
            .push_next(&mut imp)
            .push_next(&mut ded);
        // SAFETY: `raw` is a dma-buf fd we own; on success Vulkan takes
        // ownership of it (VK_KHR_external_memory_fd), on failure it does
        // not and it is closed below. The dedicated image is unbound.
        let mem = match unsafe { d.allocate_memory(&ai, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: the import failed, so `raw` is still ours alone.
                drop(unsafe { OwnedFd::from_raw_fd(raw) });
                // SAFETY: unused image.
                unsafe { d.destroy_image(image, None) };
                return Err(BackendError::with_code(
                    ErrorCode::BadBuffer,
                    format!("dma-buf import: {e}"),
                ));
            }
        };
        // SAFETY: dedicated allocation for exactly this image, offset 0.
        if let Err(e) = unsafe { d.bind_image_memory(image, mem, 0) } {
            // SAFETY: neither object is in use.
            unsafe {
                d.destroy_image(image, None);
                d.free_memory(mem, None);
            }
            return Err(BackendError::new(format!("vkBindImageMemory: {e}")));
        }
        Ok((image, mem))
    }

    /// View, descriptor set and pipelines for an image: the rest of a `Tex`.
    fn finish_tex(
        &mut self,
        image: vk::Image,
        mems: Vec<vk::DeviceMemory>,
        (fourcc, w, h): (u32, u32, u32),
        key: FamilyKey,
        foreign: bool,
    ) -> Result<Tex, BackendError> {
        let mut t = Tex {
            image,
            mems,
            view: vk::ImageView::null(),
            set: vk::DescriptorSet::null(),
            layout: vk::PipelineLayout::null(),
            pipes: [vk::Pipeline::null(); 2],
            fourcc,
            w,
            h,
            foreign,
            staging: None,
        };
        if let Err(e) = self.fill_tex(&mut t, key) {
            self.release(t);
            return Err(e);
        }
        Ok(t)
    }

    fn fill_tex(&mut self, t: &mut Tex, key: FamilyKey) -> Result<(), BackendError> {
        let fam = self.pipes.family(&self.gpu, key)?;
        let (conv, dsl, layout, pipes) = (fam.conv, fam.dsl, fam.layout, fam.pipes);
        let d = &self.gpu.device;
        let format = vk_format(t.fourcc)
            .ok_or_else(|| BackendError::with_code(ErrorCode::BadFormat, "format"))?;
        let mut cinfo = vk::SamplerYcbcrConversionInfo::default().conversion(conv);
        let mut vci = vk::ImageViewCreateInfo::default()
            .image(t.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(COLOR);
        if conv != vk::SamplerYcbcrConversion::null() {
            vci = vci.push_next(&mut cinfo);
        }
        // SAFETY: `image` is live with bound memory; the chained conversion
        // matches the format.
        t.view = unsafe { d.create_image_view(&vci, None) }.map_err(be("vkCreateImageView"))?;
        let layouts = [dsl];
        let ai = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.pipes.pool)
            .set_layouts(&layouts);
        // SAFETY: the pool allows freeing; `dsl` is live.
        t.set =
            unsafe { d.allocate_descriptor_sets(&ai) }.map_err(be("vkAllocateDescriptorSets"))?[0];
        let ii = [vk::DescriptorImageInfo {
            sampler: vk::Sampler::null(),
            image_view: t.view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }];
        let wr = [vk::WriteDescriptorSet::default()
            .dst_set(t.set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&ii)];
        // SAFETY: the set is not in use by any command buffer yet.
        unsafe { d.update_descriptor_sets(&wr, &[]) };
        t.layout = layout;
        t.pipes = pipes;
        Ok(())
    }

    fn import_shadow_udmabuf(
        &mut self,
        s: &ShadowDesc,
        memfd: &OwnedFd,
    ) -> Result<Tex, BackendError> {
        let page = rustix::param::page_size() as u64;
        let size = (u64::from(s.stride) * u64::from(s.h)).div_ceil(page) * page;
        let len = nitro_shm::sealed_len(memfd).map_err(|e| BackendError::new(e.to_string()))?;
        if size > len {
            return Err(BackendError::new("memfd not page-padded"));
        }
        let buf =
            sys::udmabuf(memfd, 0, size).map_err(|e| BackendError::new(format!("udmabuf: {e}")))?;
        let (image, mem) = self.import_image(
            vk::Format::B8G8R8A8_UNORM,
            (s.w, s.h),
            MOD_LINEAR,
            &[(0, s.stride)],
            buf,
        )?;
        self.finish_tex(
            image,
            vec![mem],
            (s.fourcc, s.w, s.h),
            FamilyKey::Rgba,
            true,
        )
    }

    fn import_shadow_staging(
        &mut self,
        s: &ShadowDesc,
        memfd: OwnedFd,
    ) -> Result<Tex, BackendError> {
        let len = s.stride as usize * s.h as usize;
        let map = nitro_shm::Mapping::map(memfd, len)
            .map_err(|e| BackendError::with_code(ErrorCode::BadBuffer, e.to_string()))?;
        let d = &self.gpu.device;
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width: s.w,
                height: s.h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: complete create info for a core format/usage.
        let image = unsafe { d.create_image(&ci, None) }.map_err(be("vkCreateImage"))?;
        let mem = self.alloc_bind(Bound::Image(image), vk::MemoryPropertyFlags::DEVICE_LOCAL);
        let mem = match mem {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: unused image.
                unsafe { d.destroy_image(image, None) };
                return Err(e);
            }
        };
        let mut t = self.finish_tex(
            image,
            vec![mem],
            (s.fourcc, s.w, s.h),
            FamilyKey::Rgba,
            false,
        )?;
        match self.staging(map, s.stride, len) {
            Ok(st) => t.staging = Some(st),
            Err(e) => {
                self.release(t);
                return Err(e);
            }
        }
        let full = [IRect::new(0, 0, crate::px(s.w), crate::px(s.h))];
        if let Err(e) = self.upload_damage(&mut t, &full) {
            self.release(t);
            return Err(e);
        }
        Ok(t)
    }

    fn staging(
        &self,
        map: nitro_shm::Mapping,
        stride: u32,
        len: usize,
    ) -> Result<Staging, BackendError> {
        let d = &self.gpu.device;
        let bci = vk::BufferCreateInfo::default()
            .size(len as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: valid create info.
        let buf = unsafe { d.create_buffer(&bci, None) }.map_err(be("vkCreateBuffer"))?;
        let mut st = Staging {
            map,
            stride,
            buf,
            mem: vk::DeviceMemory::null(),
            ptr: std::ptr::null_mut(),
            len,
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            ready: false,
        };
        let r = (|| {
            st.mem = self.alloc_bind(
                Bound::Buffer(buf),
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            // SAFETY: host-visible memory, not mapped yet, whole range.
            st.ptr =
                unsafe { d.map_memory(st.mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
                    .map_err(be("vkMapMemory"))?
                    .cast();
            st.cmd = self.cmd_buffer()?;
            st.fence = self.fence(true)?;
            Ok(())
        })();
        match r {
            Ok(()) => Ok(st),
            Err(e) => {
                self.destroy_staging(&st);
                Err(e)
            }
        }
    }

    fn destroy_staging(&self, st: &Staging) {
        let d = &self.gpu.device;
        // SAFETY: the caller waited for the staging fence (or never
        // submitted); null handles are no-ops. Freeing memory unmaps it.
        unsafe {
            d.destroy_fence(st.fence, None);
            if st.cmd != vk::CommandBuffer::null() {
                d.free_command_buffers(self.pool, &[st.cmd]);
            }
            d.destroy_buffer(st.buf, None);
            d.free_memory(st.mem, None);
        }
    }

    fn alloc_bind(
        &self,
        what: Bound,
        want: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory, BackendError> {
        let d = &self.gpu.device;
        // SAFETY: the object is live and unbound.
        let reqs = unsafe {
            match what {
                Bound::Image(i) => d.get_image_memory_requirements(i),
                Bound::Buffer(b) => d.get_buffer_memory_requirements(b),
            }
        };
        let ty = self
            .gpu
            .memtype(reqs.memory_type_bits, want)
            .ok_or_else(|| BackendError::new("no memory type"))?;
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(ty);
        // SAFETY: valid allocate info.
        let mem = unsafe { d.allocate_memory(&ai, None) }.map_err(be("vkAllocateMemory"))?;
        // SAFETY: fresh allocation of the required size; object unbound.
        let r = unsafe {
            match what {
                Bound::Image(i) => d.bind_image_memory(i, mem, 0),
                Bound::Buffer(b) => d.bind_buffer_memory(b, mem, 0),
            }
        };
        if let Err(e) = r {
            // SAFETY: unused allocation.
            unsafe { d.free_memory(mem, None) };
            return Err(BackendError::new(format!("bind memory: {e}")));
        }
        Ok(mem)
    }

    fn destroy_slots(&mut self) {
        let d = &self.gpu.device;
        // SAFETY: waits for all work; afterwards no slot object is in use.
        unsafe {
            let _ = d.device_wait_idle();
            for s in std::mem::take(&mut self.slots) {
                self.destroy_slot(s);
            }
        }
    }

    /// Free one slot's objects.
    ///
    /// # Safety
    /// Nothing submitted with the slot is still executing.
    unsafe fn destroy_slot(&self, s: Slot) {
        let d = &self.gpu.device;
        // SAFETY: the caller's promise; null handles are no-ops.
        unsafe {
            for w in s.waits {
                d.destroy_semaphore(w, None);
            }
            d.destroy_semaphore(s.done, None);
            d.destroy_fence(s.fence, None);
            if s.cmd != vk::CommandBuffer::null() {
                d.free_command_buffers(self.pool, &[s.cmd]);
            }
            d.destroy_framebuffer(s.fb, None);
            d.destroy_image_view(s.view, None);
            d.destroy_image(s.image, None);
            d.free_memory(s.mem, None);
        }
    }

    fn new_slot(
        &self,
        req: &RingRequest,
    ) -> Result<(Slot, SlotLayout, u64, OwnedFd), BackendError> {
        let d = &self.gpu.device;
        let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&req.modifiers);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width: req.w,
                height: req.h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext)
            .push_next(&mut list);
        // SAFETY: complete create info; every modifier was advertised
        // renderable and exportable.
        let image = unsafe { d.create_image(&ci, None) }.map_err(be("vkCreateImage(ring)"))?;
        let mut slot = Slot {
            image,
            mem: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            fb: vk::Framebuffer::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            done: vk::Semaphore::null(),
            waits: Vec::new(),
            dmabuf: None,
            fresh: true,
        };
        match self.fill_slot(&mut slot, req) {
            Ok((layout, modifier, fd)) => Ok((slot, layout, modifier, fd)),
            Err(e) => {
                // SAFETY: nothing was submitted with these objects.
                unsafe { self.destroy_slot(slot) };
                Err(e)
            }
        }
    }

    fn fill_slot(
        &self,
        s: &mut Slot,
        req: &RingRequest,
    ) -> Result<(SlotLayout, u64, OwnedFd), BackendError> {
        let d = &self.gpu.device;
        // SAFETY: live, unbound image.
        let reqs = unsafe { d.get_image_memory_requirements(s.image) };
        let ty = self
            .gpu
            .memtype(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .ok_or_else(|| BackendError::new("no memory type for the ring"))?;
        let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(s.image);
        let mut exp = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(ty)
            .push_next(&mut exp)
            .push_next(&mut ded);
        // SAFETY: valid allocate info with a dedicated, unbound image.
        s.mem = unsafe { d.allocate_memory(&ai, None) }.map_err(be("vkAllocateMemory(ring)"))?;
        // SAFETY: dedicated allocation for this image.
        unsafe { d.bind_image_memory(s.image, s.mem, 0) }.map_err(be("vkBindImageMemory(ring)"))?;
        let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: the image was created with DRM_FORMAT_MODIFIER tiling.
        unsafe {
            self.gpu
                .drm_mod
                .get_image_drm_format_modifier_properties(s.image, &mut props)
        }
        .map_err(be("vkGetImageDrmFormatModifierPropertiesEXT"))?;
        let sub = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            mip_level: 0,
            array_layer: 0,
        };
        // SAFETY: a modifier-tiled single-plane image; plane 0 exists.
        let lay = unsafe { d.get_image_subresource_layout(s.image, sub) };
        let gi = vk::MemoryGetFdInfoKHR::default()
            .memory(s.mem)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: the memory was allocated exportable as a dma-buf.
        let raw = unsafe { self.gpu.mem_fd.get_memory_fd(&gi) }.map_err(be("vkGetMemoryFdKHR"))?;
        if raw < 0 {
            return Err(BackendError::new("vkGetMemoryFdKHR returned no fd"));
        }
        // SAFETY: vkGetMemoryFdKHR returns a new fd owned by the caller.
        let export = unsafe { OwnedFd::from_raw_fd(raw) };
        let vci = vk::ImageViewCreateInfo::default()
            .image(s.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(COLOR);
        // SAFETY: live image with memory bound.
        s.view =
            unsafe { d.create_image_view(&vci, None) }.map_err(be("vkCreateImageView(ring)"))?;
        let atts = [s.view];
        let fci = vk::FramebufferCreateInfo::default()
            .render_pass(self.pipes.render_pass)
            .attachments(&atts)
            .width(req.w)
            .height(req.h)
            .layers(1);
        // SAFETY: the view matches the render pass's attachment.
        s.fb = unsafe { d.create_framebuffer(&fci, None) }.map_err(be("vkCreateFramebuffer"))?;
        s.cmd = self.cmd_buffer()?;
        s.fence = self.fence(true)?;
        let mut esci = vk::ExportSemaphoreCreateInfo::default()
            .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        // SAFETY: valid create info; SYNC_FD export is supported (checked
        // in #3903 on both drivers, and required by the extension set).
        s.done = unsafe {
            d.create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut esci),
                None,
            )
        }
        .map_err(be("vkCreateSemaphore"))?;
        s.dmabuf = Some(
            rustix::io::fcntl_dupfd_cloexec(&export, 0)
                .map_err(|e| BackendError::new(e.to_string()))?,
        );
        let layout = SlotLayout {
            offset: u32::try_from(lay.offset).unwrap_or(0),
            pitch: u32::try_from(lay.row_pitch).unwrap_or(0),
            size: reqs.size,
        };
        Ok((layout, props.drm_format_modifier, export))
    }

    fn import_semaphore(&self, fd: OwnedFd) -> Result<vk::Semaphore, BackendError> {
        let d = &self.gpu.device;
        // SAFETY: valid create info.
        let sem = unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
            .map_err(be("vkCreateSemaphore"))?;
        let raw = fd.into_raw_fd();
        let ii = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(sem)
            .flags(vk::SemaphoreImportFlags::TEMPORARY)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
            .fd(raw);
        // SAFETY: `raw` is a descriptor we own; on success the semaphore
        // takes ownership, on failure it stays ours and is closed below.
        if let Err(e) = unsafe { self.gpu.sem_fd.import_semaphore_fd(&ii) } {
            // SAFETY: the import failed: `raw` is still ours alone; `sem`
            // was never used.
            unsafe {
                drop(OwnedFd::from_raw_fd(raw));
                d.destroy_semaphore(sem, None);
            }
            return Err(BackendError::with_code(
                ErrorCode::Fences,
                format!("sync_file import: {e}"),
            ));
        }
        Ok(sem)
    }

    #[allow(clippy::too_many_lines)] // one frame: barriers, pass, per-rect per-layer draws
    fn record(&self, slot: &Slot, size: (u32, u32), clip: &[IRect], layers: &[(&Tex, Layer)]) {
        let d = &self.gpu.device;
        let cmd = slot.cmd;
        let foreign = vk::QUEUE_FAMILY_FOREIGN_EXT;
        let ignored = vk::QUEUE_FAMILY_IGNORED;
        let q = self.gpu.qfi;
        let mut seen: Vec<vk::Image> = Vec::new();
        let foreign_texs: Vec<&Tex> = layers
            .iter()
            .map(|(t, _)| *t)
            .filter(|t| {
                t.foreign && !seen.contains(&t.image) && {
                    seen.push(t.image);
                    true
                }
            })
            .collect();
        let mut pre = vec![if slot.fresh {
            barrier(
                slot.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                (ignored, ignored),
                (
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
            )
        } else {
            barrier(
                slot.image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                (foreign, q),
                (
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::COLOR_ATTACHMENT_READ
                        | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
            )
        }];
        let mut post = vec![barrier(
            slot.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::GENERAL,
            (q, foreign),
            (
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::empty(),
            ),
        )];
        for t in &foreign_texs {
            pre.push(barrier(
                t.image,
                vk::ImageLayout::GENERAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                (foreign, q),
                (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ),
            ));
            post.push(barrier(
                t.image,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::GENERAL,
                (q, foreign),
                (vk::AccessFlags::SHADER_READ, vk::AccessFlags::empty()),
            ));
        }
        let (w, h) = (size.0 as f32, size.1 as f32);
        let area = clip.iter().fold(IRect::EMPTY, |a, r| a.union(r));
        // SAFETY: `cmd` is this slot's command buffer, reset and not in
        // use (its fence was waited on by the caller); every handle
        // recorded is live until the fence signals (the event loop holds
        // references to the textures until then).
        unsafe {
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::FRAGMENT_SHADER
                    | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &pre,
            );
            if !area.is_empty() || slot.fresh {
                let full = vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: size.0,
                        height: size.1,
                    },
                };
                let ra = if slot.fresh { full } else { rect2d(area) };
                let rbi = vk::RenderPassBeginInfo::default()
                    .render_pass(self.pipes.render_pass)
                    .framebuffer(slot.fb)
                    .render_area(ra);
                d.cmd_begin_render_pass(cmd, &rbi, vk::SubpassContents::INLINE);
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: w,
                        height: h,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                if slot.fresh {
                    let clear = vk::ClearAttachment {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        color_attachment: 0,
                        clear_value: vk::ClearValue {
                            color: vk::ClearColorValue {
                                float32: [0.0, 0.0, 0.0, 1.0],
                            },
                        },
                    };
                    let cr = vk::ClearRect {
                        rect: full,
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    d.cmd_clear_attachments(cmd, &[clear], &[cr]);
                }
                for r in clip {
                    d.cmd_set_scissor(cmd, 0, &[rect2d(*r)]);
                    for (t, l) in layers {
                        if !l.dst.intersects(r) {
                            continue;
                        }
                        let pipe = t.pipes[blend_index(l.blend)];
                        d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipe);
                        d.cmd_bind_descriptor_sets(
                            cmd,
                            vk::PipelineBindPoint::GRAPHICS,
                            t.layout,
                            0,
                            &[t.set],
                            &[],
                        );
                        let push = push_constants(l, t, w, h);
                        d.cmd_push_constants(
                            cmd,
                            t.layout,
                            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                            0,
                            &push,
                        );
                        d.cmd_draw(cmd, 4, 1, 0, 0);
                    }
                }
                d.cmd_end_render_pass(cmd);
            }
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &post,
            );
        }
    }
}

#[derive(Clone, Copy)]
enum Bound {
    Image(vk::Image),
    Buffer(vk::Buffer),
}

fn rect2d(r: IRect) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x: r.x, y: r.y },
        extent: vk::Extent2D {
            width: r.w.max(0) as u32,
            height: r.h.max(0) as u32,
        },
    }
}

/// dst rect in NDC, src rect normalised, opaque flag — the layout of
/// `PC` in `shaders/quad.vert`.
fn push_constants(l: &Layer, t: &Tex, w: f32, h: f32) -> [u8; PUSH_SIZE as usize] {
    let [sx, sy, sw, sh] = l.src;
    let (tw, th) = (t.w as f32, t.h as f32);
    let vals = [
        l.dst.x as f32 / w * 2.0 - 1.0,
        l.dst.y as f32 / h * 2.0 - 1.0,
        l.dst.right() as f32 / w * 2.0 - 1.0,
        l.dst.bottom() as f32 / h * 2.0 - 1.0,
        sx / tw,
        sy / th,
        (sx + sw) / tw,
        (sy + sh) / th,
    ];
    let mut out = [0u8; PUSH_SIZE as usize];
    for (i, v) in vals.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    let opaque = u32::from(t.fourcc != AR24);
    out[32..36].copy_from_slice(&opaque.to_le_bytes());
    out
}

impl Backend for VkBackend {
    type Tex = Tex;

    fn info(&self) -> DeviceInfo {
        self.gpu.info.clone()
    }

    fn shadow_path(&self) -> ShadowPath {
        self.shadow_path
    }

    fn import_dmabuf(&mut self, desc: &DmabufDesc, fds: Vec<OwnedFd>) -> Result<Tex, BackendError> {
        let format = vk_format(desc.fourcc)
            .ok_or_else(|| BackendError::with_code(ErrorCode::BadFormat, "format"))?;
        // All planes in one buffer (the common case: VA, udmabuf, most
        // producers). Separate buffers need a DISJOINT import: not yet.
        let ino = |fd: &OwnedFd| rustix::fs::fstat(fd).map(|s| (s.st_dev, s.st_ino)).ok();
        let first = ino(&fds[0]);
        if first.is_none() || fds.iter().any(|f| ino(f) != first) {
            return Err(BackendError::with_code(
                ErrorCode::BadFormat,
                "planes in separate buffers (disjoint import) are not supported yet",
            ));
        }
        let fd = fds
            .into_iter()
            .next()
            .ok_or_else(|| BackendError::with_code(ErrorCode::Protocol, "no fd"))?;
        let planes: Vec<(u32, u32)> = desc.planes.iter().map(|p| (p.offset, p.pitch)).collect();
        let (image, mem) =
            self.import_image(format, (desc.w, desc.h), desc.modifier, &planes, fd)?;
        let key = if desc.fourcc == NV12 {
            FamilyKey::Ycbcr(desc.encoding, desc.range)
        } else {
            FamilyKey::Rgba
        };
        self.finish_tex(image, vec![mem], (desc.fourcc, desc.w, desc.h), key, true)
    }

    fn import_shadow(&mut self, desc: &ShadowDesc, memfd: OwnedFd) -> Result<Tex, BackendError> {
        if !self.force_staging {
            match self.import_shadow_udmabuf(desc, &memfd) {
                Ok(t) => {
                    self.shadow_path = ShadowPath::Udmabuf;
                    return Ok(t);
                }
                Err(e) => eprintln!(
                    "nitro-gpu: shadow via udmabuf failed ({}), using staging",
                    e.msg
                ),
            }
        }
        let t = self.import_shadow_staging(desc, memfd)?;
        self.shadow_path = ShadowPath::Staging;
        Ok(t)
    }

    #[allow(clippy::many_single_char_names, clippy::too_many_lines)] // CPU copy + GPU copy, one sequence
    fn upload_damage(&mut self, t: &mut Tex, rects: &[IRect]) -> Result<(), BackendError> {
        let Some(st) = t.staging.as_mut() else {
            return Ok(()); // udmabuf: the GPU reads the memfd's pages
        };
        if rects.is_empty() {
            return Ok(());
        }
        let d = &self.gpu.device;
        // The staging buffer is reused: its previous copy must be done.
        // SAFETY: the fence belongs to this device.
        unsafe { d.wait_for_fences(&[st.fence], true, FENCE_WAIT_NS) }
            .map_err(be("vkWaitForFences"))?;
        // SAFETY: `ptr` maps `len` bytes of host-coherent memory for the
        // life of `st`; the GPU no longer reads it (fence above), and no
        // other reference to that memory exists.
        let dst = unsafe { std::slice::from_raw_parts_mut(st.ptr, st.len) };
        let src = st.map.as_bytes();
        let stride = st.stride as usize;
        let mut regions = Vec::with_capacity(rects.len());
        for r in rects {
            let (x, w) = (r.x as usize * 4, r.w as usize * 4);
            for y in r.y as usize..r.bottom() as usize {
                let o = y * stride + x;
                dst[o..o + w].copy_from_slice(&src[o..o + w]);
            }
            regions.push(vk::BufferImageCopy {
                buffer_offset: (r.y as usize * stride + x) as u64,
                buffer_row_length: st.stride / 4,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D {
                    x: r.x,
                    y: r.y,
                    z: 0,
                },
                image_extent: vk::Extent3D {
                    width: r.w as u32,
                    height: r.h as u32,
                    depth: 1,
                },
            });
        }
        let ig = vk::QUEUE_FAMILY_IGNORED;
        let old = if st.ready {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        let to_dst = barrier(
            t.image,
            old,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            (ig, ig),
            (
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::TRANSFER_WRITE,
            ),
        );
        let to_read = barrier(
            t.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            (ig, ig),
            (
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
            ),
        );
        let cmd = st.cmd;
        // SAFETY: `cmd` is idle (fence waited); image and buffer are live;
        // the barriers order the copy after earlier frames' sampling on
        // this queue and before later ones.
        unsafe {
            d.reset_fences(&[st.fence]).map_err(be("vkResetFences"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(be("vkBeginCommandBuffer"))?;
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            d.cmd_copy_buffer_to_image(
                cmd,
                st.buf,
                t.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );
            d.end_command_buffer(cmd)
                .map_err(be("vkEndCommandBuffer"))?;
            let cmds = [cmd];
            let si = vk::SubmitInfo::default().command_buffers(&cmds);
            d.queue_submit(self.gpu.queue, &[si], st.fence)
                .map_err(be("vkQueueSubmit(upload)"))?;
        }
        st.ready = true;
        Ok(())
    }

    fn alloc_output_ring(&mut self, req: &RingRequest) -> Result<Ring, BackendError> {
        self.destroy_slots();
        let mut out = Vec::with_capacity(req.n);
        let mut modifier = 0;
        for _ in 0..req.n {
            match self.new_slot(req) {
                Ok((slot, layout, m, fd)) => {
                    modifier = m;
                    self.slots.push(slot);
                    out.push((layout, fd));
                }
                Err(e) => {
                    self.destroy_slots();
                    return Err(e);
                }
            }
        }
        self.out_w = req.w;
        self.out_h = req.h;
        Ok(Ring {
            modifier,
            slots: out,
        })
    }

    fn composite(
        &mut self,
        out_idx: usize,
        clip: &[IRect],
        layers: &[(&Tex, Layer)],
        acquire: Vec<OwnedFd>,
    ) -> Result<OwnedFd, BackendError> {
        let Some(slot) = self.slots.get(out_idx) else {
            return Err(BackendError::with_code(ErrorCode::NoRing, "slot"));
        };
        self.wait(slot.fence)?;
        let d = &self.gpu.device;
        let mut waits = Vec::with_capacity(acquire.len());
        for fd in acquire {
            match self.import_semaphore(fd) {
                Ok(s) => waits.push(s),
                Err(e) => {
                    // SAFETY: never submitted.
                    unsafe {
                        for s in waits {
                            d.destroy_semaphore(s, None);
                        }
                    }
                    return Err(e);
                }
            }
        }
        let slot = &self.slots[out_idx];
        // SAFETY: the slot's fence is signalled: its command buffer and
        // last frame's wait semaphores are no longer in use.
        unsafe {
            for s in &slot.waits {
                d.destroy_semaphore(*s, None);
            }
            d.reset_command_buffer(slot.cmd, vk::CommandBufferResetFlags::empty())
                .map_err(be("vkResetCommandBuffer"))?;
            d.begin_command_buffer(
                slot.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(be("vkBeginCommandBuffer"))?;
        }
        self.record(slot, (self.out_w, self.out_h), clip, layers);
        let slot = &self.slots[out_idx];
        let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
        let cmds = [slot.cmd];
        let sig = [slot.done];
        let si = vk::SubmitInfo::default()
            .wait_semaphores(&waits)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&sig);
        // SAFETY: recorded command buffer; semaphores and fence live and
        // unsignalled/unused (fence reset right before).
        unsafe {
            d.end_command_buffer(slot.cmd)
                .map_err(be("vkEndCommandBuffer"))?;
            d.reset_fences(&[slot.fence]).map_err(be("vkResetFences"))?;
            d.queue_submit(self.gpu.queue, &[si], slot.fence)
                .map_err(be("vkQueueSubmit"))?;
        }
        let gi = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(slot.done)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        // SAFETY: `done` was just submitted as a signal operation, which a
        // SYNC_FD export requires.
        let raw =
            unsafe { self.gpu.sem_fd.get_semaphore_fd(&gi) }.map_err(be("vkGetSemaphoreFdKHR"))?;
        if raw < 0 {
            return Err(BackendError::new("vkGetSemaphoreFdKHR returned no fd"));
        }
        // SAFETY: a new sync_file fd owned by the caller.
        let sync = unsafe { OwnedFd::from_raw_fd(raw) };
        // Implicit-sync readers of the slot (KMS without IN_FENCE_FD)
        // wait on it too. Best effort: pre-6.0 kernels lack the ioctl.
        if let Some(buf) = &slot.dmabuf {
            let _ = sys::import_sync_file(buf.as_fd(), &sync);
        }
        let slot = &mut self.slots[out_idx];
        slot.waits = waits;
        slot.fresh = false;
        Ok(sync)
    }

    fn release(&mut self, t: Tex) {
        let d = &self.gpu.device;
        if let Some(st) = &t.staging {
            let _ = self.wait(st.fence);
            self.destroy_staging(st);
        }
        // SAFETY: the event loop releases a texture only after every
        // frame sampling it signalled (and the staging copy was waited
        // for above); null handles are no-ops.
        unsafe {
            if t.set != vk::DescriptorSet::null() {
                let _ = d.free_descriptor_sets(self.pipes.pool, &[t.set]);
            }
            d.destroy_image_view(t.view, None);
            d.destroy_image(t.image, None);
            for m in t.mems {
                d.free_memory(m, None);
            }
        }
    }

    fn readback(&mut self, out_idx: usize) -> Result<Readback, BackendError> {
        let Some(slot) = self.slots.get(out_idx) else {
            return Err(BackendError::with_code(ErrorCode::NoRing, "slot"));
        };
        if slot.fresh {
            return Err(BackendError::with_code(
                ErrorCode::NoRing,
                "slot never drawn",
            ));
        }
        let (w, h) = (self.out_w, self.out_h);
        let len = w as usize * h as usize * 4;
        let d = &self.gpu.device;
        let bci = vk::BufferCreateInfo::default()
            .size(len as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: valid create info.
        let buf = unsafe { d.create_buffer(&bci, None) }.map_err(be("vkCreateBuffer"))?;
        let res = (|| {
            let mem = self.alloc_bind(
                Bound::Buffer(buf),
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            let r = self.readback_into(slot.image, buf, mem, (w, h));
            // SAFETY: the copy finished (waited in `readback_into`) or
            // was never submitted.
            unsafe { d.free_memory(mem, None) };
            r
        })();
        // SAFETY: as above.
        unsafe { d.destroy_buffer(buf, None) };
        let bytes = res?;
        let memfd = nitro_shm::memfd_with("nitro-gpu-readback", &bytes)
            .map_err(|e| BackendError::new(e.to_string()))?;
        Ok(Readback {
            memfd,
            stride: w * 4,
        })
    }

    fn capture(
        &mut self,
        w: u32,
        h: u32,
        layers: &[(&Tex, Layer)],
    ) -> Result<Readback, BackendError> {
        // A one-slot "ring" of the capture's size: the same render pass,
        // pipelines and layouts as a frame, freed before returning, so a
        // shot leaves nothing allocated (#3962). LINEAR first: the cheapest
        // copy out, and every driver renders to it.
        let mut modifiers: Vec<u64> = self
            .gpu
            .info
            .render
            .iter()
            .filter(|f| f.fourcc == XR24)
            .map(|f| f.modifier)
            .collect();
        modifiers.sort_by_key(|m| *m != MOD_LINEAR);
        modifiers.dedup();
        if modifiers.is_empty() {
            return Err(BackendError::with_code(
                ErrorCode::BadFormat,
                "no XR24 render modifier",
            ));
        }
        let req = RingRequest {
            n: 1,
            w,
            h,
            fourcc: XR24,
            modifiers,
        };
        let (slot, _, _, export) = self.new_slot(&req)?;
        drop(export);
        let d = &self.gpu.device;
        let full = [IRect::new(0, 0, crate_px(w), crate_px(h))];
        // SAFETY: a fresh slot: its command buffer and fence are unused.
        let r = unsafe {
            (|| {
                d.begin_command_buffer(
                    slot.cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(be("vkBeginCommandBuffer"))?;
                self.record(&slot, (w, h), &full, layers);
                d.end_command_buffer(slot.cmd)
                    .map_err(be("vkEndCommandBuffer"))?;
                d.reset_fences(&[slot.fence]).map_err(be("vkResetFences"))?;
                let cmds = [slot.cmd];
                d.queue_submit(
                    self.gpu.queue,
                    &[vk::SubmitInfo::default().command_buffers(&cmds)],
                    slot.fence,
                )
                .map_err(be("vkQueueSubmit(capture)"))?;
                self.wait(slot.fence)
            })()
        };
        let res = r.and_then(|()| {
            let len = w as usize * h as usize * 4;
            let bci = vk::BufferCreateInfo::default()
                .size(len as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            // SAFETY: valid create info.
            let buf = unsafe { d.create_buffer(&bci, None) }.map_err(be("vkCreateBuffer"))?;
            let r = (|| {
                let mem = self.alloc_bind(
                    Bound::Buffer(buf),
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )?;
                let r = self.readback_into(slot.image, buf, mem, (w, h));
                // SAFETY: the copy finished or was never submitted.
                unsafe { d.free_memory(mem, None) };
                r
            })();
            // SAFETY: as above.
            unsafe { d.destroy_buffer(buf, None) };
            r
        });
        // SAFETY: the draw and the copy were waited for (or a wait failed:
        // then wait for the whole device before freeing).
        unsafe {
            if res.is_err() {
                let _ = d.device_wait_idle();
            }
            self.destroy_slot(slot);
        }
        let bytes = res?;
        let memfd = nitro_shm::memfd_with("nitro-gpu-capture", &bytes)
            .map_err(|e| BackendError::new(e.to_string()))?;
        Ok(Readback {
            memfd,
            stride: w * 4,
        })
    }
}

fn crate_px(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

impl VkBackend {
    fn readback_into(
        &self,
        image: vk::Image,
        buf: vk::Buffer,
        mem: vk::DeviceMemory,
        (width, height): (u32, u32),
    ) -> Result<Vec<u8>, BackendError> {
        let len = width as usize * height as usize * 4;
        let d = &self.gpu.device;
        let cmd = self.cmd_buffer()?;
        let fence = self.fence(false)?;
        let foreign = vk::QUEUE_FAMILY_FOREIGN_EXT;
        let q = self.gpu.qfi;
        let pre = barrier(
            image,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            (foreign, q),
            (vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_READ),
        );
        let post = barrier(
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::GENERAL,
            (q, foreign),
            (vk::AccessFlags::TRANSFER_READ, vk::AccessFlags::empty()),
        );
        let region = vk::BufferImageCopy {
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
            ..vk::BufferImageCopy::default()
        };
        // SAFETY: fresh command buffer and fence; the image and buffer are
        // live; the wait below keeps everything alive until the copy is
        // done; `ptr` maps `len` host-coherent bytes written by the copy.
        let r = unsafe {
            (|| {
                d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                    .map_err(be("vkBeginCommandBuffer"))?;
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[pre],
                );
                d.cmd_copy_image_to_buffer(
                    cmd,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    buf,
                    &[region],
                );
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE | vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[post],
                );
                d.end_command_buffer(cmd)
                    .map_err(be("vkEndCommandBuffer"))?;
                let cmds = [cmd];
                d.queue_submit(
                    self.gpu.queue,
                    &[vk::SubmitInfo::default().command_buffers(&cmds)],
                    fence,
                )
                .map_err(be("vkQueueSubmit(readback)"))?;
                d.wait_for_fences(&[fence], true, FENCE_WAIT_NS)
                    .map_err(be("vkWaitForFences"))?;
                let ptr = d
                    .map_memory(mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                    .map_err(be("vkMapMemory"))?;
                let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec();
                d.unmap_memory(mem);
                Ok(bytes)
            })()
        };
        // SAFETY: waited for (or never submitted).
        unsafe {
            let _ = d.wait_for_fences(&[fence], true, FENCE_WAIT_NS);
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.pool, &[cmd]);
        }
        r
    }
}

impl Drop for VkBackend {
    fn drop(&mut self) {
        self.destroy_slots();
        // SAFETY: the device is idle (`destroy_slots` waited) and every
        // texture was released by the event loop's teardown.
        unsafe { self.gpu.device.destroy_command_pool(self.pool, None) };
        self.pipes.destroy(&self.gpu);
    }
}
