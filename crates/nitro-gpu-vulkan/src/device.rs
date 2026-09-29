//! Instance, physical device, logical device, and the format/modifier
//! tables the helper advertises in `HelloReply`.

use std::ffi::CStr;
use std::path::Path;

use ash::{ext, khr, vk};
use nitro_gpu::proto::{AR24, DeviceInfo, FormatMod, NV12, XR24};

use crate::icd;

/// The Vulkan objects every other module uses.
pub(crate) struct Gpu {
    // Keeps libvulkan loaded for as long as the instance lives.
    _entry: ash::Entry,
    pub instance: ash::Instance,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub qfi: u32,
    pub mem: vk::PhysicalDeviceMemoryProperties,
    pub mem_fd: khr::external_memory_fd::Device,
    pub sem_fd: khr::external_semaphore_fd::Device,
    pub drm_mod: ext::image_drm_format_modifier::Device,
    pub info: DeviceInfo,
    /// Every advertised NV12 modifier supports linear chroma filtering.
    pub nv12_linear: bool,
    /// Every advertised NV12 modifier supports midpoint chroma samples.
    pub nv12_midpoint: bool,
}

const REQUIRED: [&CStr; 5] = [
    khr::external_memory_fd::NAME,
    ext::external_memory_dma_buf::NAME,
    ext::image_drm_format_modifier::NAME,
    khr::external_semaphore_fd::NAME,
    ext::queue_family_foreign::NAME,
];

/// The Vulkan format behind a DRM fourcc.
pub(crate) fn vk_format(fourcc: u32) -> Option<vk::Format> {
    match fourcc {
        XR24 | AR24 => Some(vk::Format::B8G8R8A8_UNORM),
        NV12 => Some(vk::Format::G8_B8R8_2PLANE_420_UNORM),
        _ => None,
    }
}

fn vkerr(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |e| format!("{what}: {e}")
}

impl Gpu {
    /// Load libvulkan (the loader reads `VK_DRIVER_FILES`, which `main`
    /// set), create an instance and a device on the render node `node`.
    pub fn open(node: &Path) -> Result<Self, String> {
        // SAFETY: `Entry::load` dlopens libvulkan and runs its
        // initialisers; the library is the system Vulkan loader, loaded
        // once per attempt and kept alive in `_entry` for as long as any
        // object created through it.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("libvulkan: {e}"))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"nitro-gpu")
            .api_version(vk::API_VERSION_1_2);
        let ici = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: `ici` and `app` are valid, fully initialised create infos
        // that outlive the call.
        let instance =
            unsafe { entry.create_instance(&ici, None) }.map_err(vkerr("vkCreateInstance"))?;
        match Self::with_instance(entry.clone(), &instance, node) {
            Ok(g) => Ok(g),
            Err(e) => {
                // SAFETY: nothing was created from `instance` that is
                // still alive (`with_instance` destroys its device on
                // error), and it is not used afterwards.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    fn with_instance(
        entry: ash::Entry,
        instance: &ash::Instance,
        node: &Path,
    ) -> Result<Self, String> {
        let pd = pick_physical_device(instance, node)?;
        // SAFETY: `pd` came from this instance.
        let (qfams, mem) = unsafe {
            (
                instance.get_physical_device_queue_family_properties(pd),
                instance.get_physical_device_memory_properties(pd),
            )
        };
        let qfi = qfams
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue")? as u32;

        let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut f11);
        // SAFETY: `pd` is valid; `f2` chains a valid 1.1 feature struct.
        unsafe { instance.get_physical_device_features2(pd, &mut f2) };
        if f11.sampler_ycbcr_conversion == vk::FALSE {
            return Err("no samplerYcbcrConversion".into());
        }

        let mut exts: Vec<*const std::ffi::c_char> = REQUIRED.iter().map(|n| n.as_ptr()).collect();
        if has_extension(instance, pd, ext::physical_device_drm::NAME) {
            exts.push(ext::physical_device_drm::NAME.as_ptr());
        }
        let prio = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&prio)];
        let mut f11_on =
            vk::PhysicalDeviceVulkan11Features::default().sampler_ycbcr_conversion(true);
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qci)
            .enabled_extension_names(&exts)
            .push_next(&mut f11_on);
        // SAFETY: every extension name is a 'static C string checked
        // present on `pd`; the feature enabled was checked supported.
        let device =
            unsafe { instance.create_device(pd, &dci, None) }.map_err(vkerr("vkCreateDevice"))?;
        // SAFETY: family `qfi` was created with one queue.
        let queue = unsafe { device.get_device_queue(qfi, 0) };

        let (device_name, driver) = names(instance, pd);
        let tables = Tables::query(instance, pd);
        Ok(Self {
            mem_fd: khr::external_memory_fd::Device::new(instance, &device),
            sem_fd: khr::external_semaphore_fd::Device::new(instance, &device),
            drm_mod: ext::image_drm_format_modifier::Device::new(instance, &device),
            _entry: entry,
            instance: instance.clone(),
            device,
            queue,
            qfi,
            mem,
            info: DeviceInfo {
                device: device_name,
                driver,
                sampleable: tables.sampleable,
                render: tables.render,
            },
            nv12_linear: tables.nv12_linear,
            nv12_midpoint: tables.nv12_midpoint,
        })
    }

    /// A memory type in `bits` with every flag of `want`.
    pub fn memtype(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem.memory_type_count).find(|&i| {
            bits & (1 << i) != 0
                && self.mem.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        })
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: the owner (`VkBackend`) destroyed every object created
        // from the device before dropping the `Gpu`; the device is idle.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn has_extension(instance: &ash::Instance, pd: vk::PhysicalDevice, name: &CStr) -> bool {
    // SAFETY: `pd` came from `instance`.
    let exts = unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
    exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
}

fn pick_physical_device(
    instance: &ash::Instance,
    node: &Path,
) -> Result<vk::PhysicalDevice, String> {
    // SAFETY: `instance` is valid.
    let pds = unsafe { instance.enumerate_physical_devices() }
        .map_err(vkerr("vkEnumeratePhysicalDevices"))?;
    let usable: Vec<_> = pds
        .into_iter()
        .filter(|&pd| REQUIRED.iter().all(|n| has_extension(instance, pd, n)))
        .collect();
    if usable.is_empty() {
        return Err("no physical device with the dma-buf/sync_fd extensions".into());
    }
    let want = icd::dev_numbers(node).ok();
    for &pd in &usable {
        if !has_extension(instance, pd, ext::physical_device_drm::NAME) {
            continue;
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        // SAFETY: `pd` supports VK_EXT_physical_device_drm (checked), so
        // the chained struct is understood.
        unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
        if drm.has_render != vk::FALSE
            && want == Some((drm.render_major as u32, drm.render_minor as u32))
        {
            return Ok(pd);
        }
    }
    // No DRM match (hasvk may lack the extension): with one vendor ICD
    // loaded, the first usable device is that vendor's.
    Ok(usable[0])
}

fn names(instance: &ash::Instance, pd: vk::PhysicalDevice) -> (String, String) {
    let mut drv = vk::PhysicalDeviceDriverProperties::default();
    let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drv);
    // SAFETY: Vulkan 1.2 core struct on a 1.2 instance.
    unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
    let dev = p2
        .properties
        .device_name_as_c_str()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let d = drv
        .driver_name_as_c_str()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (dev, d)
}

/// The modifier list of `format`.
fn modifiers(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<vk::DrmFormatModifierPropertiesEXT> {
    let n = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut p2 = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: the device supports VK_EXT_image_drm_format_modifier
        // (required), so the chained list struct is filled in.
        unsafe { instance.get_physical_device_format_properties2(pd, format, &mut p2) };
        list.drm_format_modifier_count as usize
    };
    let mut v = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
    let got = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut v);
        let mut p2 = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: as above; `list` points at `v`, `n` entries long.
        unsafe { instance.get_physical_device_format_properties2(pd, format, &mut p2) };
        list.drm_format_modifier_count as usize
    };
    v.truncate(got.min(n));
    v
}

/// Whether `format` + `modifier` with `usage` supports `want` as a
/// dma-buf.
fn external_ok(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    want: vk::ExternalMemoryFeatureFlags,
) -> bool {
    let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .push_next(&mut ext_info)
        .push_next(&mut drm);
    let mut eprops = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut eprops);
    // SAFETY: all chained structs belong to extensions the device has.
    let r = unsafe { instance.get_physical_device_image_format_properties2(pd, &info, &mut props) };
    r.is_ok()
        && eprops
            .external_memory_properties
            .external_memory_features
            .contains(want)
}

struct Tables {
    sampleable: Vec<FormatMod>,
    render: Vec<FormatMod>,
    nv12_linear: bool,
    nv12_midpoint: bool,
}

impl Tables {
    fn query(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Self {
        let mut t = Self {
            sampleable: Vec::new(),
            render: Vec::new(),
            nv12_linear: true,
            nv12_midpoint: true,
        };
        let bgra = modifiers(instance, pd, vk::Format::B8G8R8A8_UNORM);
        for m in bgra
            .iter()
            .filter(|m| m.drm_format_modifier_plane_count == 1)
        {
            let f = m.drm_format_modifier_tiling_features;
            let id = m.drm_format_modifier;
            if f.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                && external_ok(
                    instance,
                    pd,
                    vk::Format::B8G8R8A8_UNORM,
                    id,
                    vk::ImageUsageFlags::SAMPLED,
                    vk::ExternalMemoryFeatureFlags::IMPORTABLE,
                )
            {
                for fourcc in [XR24, AR24] {
                    t.sampleable.push(FormatMod {
                        fourcc,
                        modifier: id,
                    });
                }
            }
            if f.contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT)
                && external_ok(
                    instance,
                    pd,
                    vk::Format::B8G8R8A8_UNORM,
                    id,
                    vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                    vk::ExternalMemoryFeatureFlags::EXPORTABLE,
                )
            {
                for fourcc in [XR24, AR24] {
                    t.render.push(FormatMod {
                        fourcc,
                        modifier: id,
                    });
                }
            }
        }
        let nv12 = modifiers(instance, pd, vk::Format::G8_B8R8_2PLANE_420_UNORM);
        let chroma = vk::FormatFeatureFlags::MIDPOINT_CHROMA_SAMPLES
            | vk::FormatFeatureFlags::COSITED_CHROMA_SAMPLES;
        for m in nv12
            .iter()
            .filter(|m| m.drm_format_modifier_plane_count == 2)
        {
            let f = m.drm_format_modifier_tiling_features;
            if !f.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                || !f.intersects(chroma)
                || !external_ok(
                    instance,
                    pd,
                    vk::Format::G8_B8R8_2PLANE_420_UNORM,
                    m.drm_format_modifier,
                    vk::ImageUsageFlags::SAMPLED,
                    vk::ExternalMemoryFeatureFlags::IMPORTABLE,
                )
            {
                continue;
            }
            t.sampleable.push(FormatMod {
                fourcc: NV12,
                modifier: m.drm_format_modifier,
            });
            t.nv12_linear &=
                f.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER);
            t.nv12_midpoint &= f.contains(vk::FormatFeatureFlags::MIDPOINT_CHROMA_SAMPLES);
        }
        t
    }
}
