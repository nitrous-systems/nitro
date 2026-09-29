//! Render pass, samplers and pipelines.
//!
//! One render pass (a single `B8G8R8A8_UNORM` attachment, `LOAD`/`STORE`,
//! kept in `COLOR_ATTACHMENT_OPTIMAL` — the ownership transfers to and
//! from KMS are explicit barriers around it). Pipelines come in
//! **families**, one per sampler: RGBA textures share a linear sampler,
//! and each YUV colour setup (matrix × range, at most six) has its own
//! `VkSamplerYcbcrConversion`, whose sampler must be immutable in the
//! descriptor set layout. Each family has two pipelines: opaque and
//! premultiplied source-over. Families are created on first use.

use std::collections::HashMap;
use std::io::Cursor;

use ash::vk;
use nitro_gpu::BackendError;
use nitro_gpu::proto::{Blend, ColorEncoding, ColorRange};

use crate::device::Gpu;

static QUAD_VERT: &[u8] = include_bytes!("../shaders/quad.vert.spv");
static TEX_FRAG: &[u8] = include_bytes!("../shaders/rgba.frag.spv");

/// Push constants: dst rect (NDC), src rect (normalised), opaque flag.
pub(crate) const PUSH_SIZE: u32 = 36;

/// Which sampler a texture needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FamilyKey {
    /// XR24 / AR24.
    Rgba,
    /// NV12 with this colour setup.
    Ycbcr(ColorEncoding, ColorRange),
}

pub(crate) struct Family {
    pub conv: vk::SamplerYcbcrConversion,
    pub sampler: vk::Sampler,
    pub dsl: vk::DescriptorSetLayout,
    pub layout: vk::PipelineLayout,
    /// Indexed by [`blend_index`].
    pub pipes: [vk::Pipeline; 2],
}

pub(crate) fn blend_index(b: Blend) -> usize {
    match b {
        Blend::Opaque => 0,
        Blend::PremulOver => 1,
    }
}

pub(crate) struct Pipelines {
    pub render_pass: vk::RenderPass,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
    pub pool: vk::DescriptorPool,
    families: HashMap<FamilyKey, Family>,
}

fn be(what: &'static str) -> impl Fn(vk::Result) -> BackendError {
    move |e| BackendError::new(format!("{what}: {e}"))
}

fn module(gpu: &Gpu, spv: &[u8]) -> Result<vk::ShaderModule, BackendError> {
    let words = ash::util::read_spv(&mut Cursor::new(spv))
        .map_err(|e| BackendError::new(format!("spir-v: {e}")))?;
    let ci = vk::ShaderModuleCreateInfo::default().code(&words);
    // SAFETY: `words` is the checked-in SPIR-V, u32-aligned by `read_spv`.
    unsafe { gpu.device.create_shader_module(&ci, None) }.map_err(be("vkCreateShaderModule"))
}

impl Pipelines {
    pub fn new(gpu: &Gpu) -> Result<Self, BackendError> {
        let att = [vk::AttachmentDescription::default()
            .format(vk::Format::B8G8R8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let refs = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let sub = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&refs)];
        let rpci = vk::RenderPassCreateInfo::default()
            .attachments(&att)
            .subpasses(&sub);
        let mut p = Self {
            render_pass: vk::RenderPass::null(),
            vert: vk::ShaderModule::null(),
            frag: vk::ShaderModule::null(),
            pool: vk::DescriptorPool::null(),
            families: HashMap::new(),
        };
        let r = (|| {
            // SAFETY: the create infos above are complete and outlive the call.
            p.render_pass = unsafe { gpu.device.create_render_pass(&rpci, None) }
                .map_err(be("vkCreateRenderPass"))?;
            p.vert = module(gpu, QUAD_VERT)?;
            p.frag = module(gpu, TEX_FRAG)?;
            // A YCbCr descriptor may consume up to three descriptors.
            let sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 3 * 1024,
            }];
            let dpci = vk::DescriptorPoolCreateInfo::default()
                .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                .max_sets(1024)
                .pool_sizes(&sizes);
            // SAFETY: valid create info.
            p.pool = unsafe { gpu.device.create_descriptor_pool(&dpci, None) }
                .map_err(be("vkCreateDescriptorPool"))?;
            Ok(())
        })();
        match r {
            Ok(()) => Ok(p),
            Err(e) => {
                p.destroy(gpu);
                Err(e)
            }
        }
    }

    /// The family for `key`, created on first use.
    pub fn family(&mut self, gpu: &Gpu, key: FamilyKey) -> Result<&Family, BackendError> {
        if !self.families.contains_key(&key) {
            let f = self.create_family(gpu, key)?;
            self.families.insert(key, f);
        }
        self.families
            .get(&key)
            .ok_or_else(|| BackendError::new("family vanished"))
    }

    fn create_family(&self, gpu: &Gpu, key: FamilyKey) -> Result<Family, BackendError> {
        let mut f = Family {
            conv: vk::SamplerYcbcrConversion::null(),
            sampler: vk::Sampler::null(),
            dsl: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            pipes: [vk::Pipeline::null(); 2],
        };
        let r = self.fill_family(gpu, key, &mut f);
        match r {
            Ok(()) => Ok(f),
            Err(e) => {
                destroy_family(gpu, &f);
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // one pipeline description, read top to bottom
    fn fill_family(&self, gpu: &Gpu, key: FamilyKey, f: &mut Family) -> Result<(), BackendError> {
        let d = &gpu.device;
        let mut filter = vk::Filter::LINEAR;
        let mut cinfo = vk::SamplerYcbcrConversionInfo::default();
        if let FamilyKey::Ycbcr(enc, range) = key {
            filter = if gpu.nv12_linear {
                vk::Filter::LINEAR
            } else {
                vk::Filter::NEAREST
            };
            let loc = if gpu.nv12_midpoint {
                vk::ChromaLocation::MIDPOINT
            } else {
                vk::ChromaLocation::COSITED_EVEN
            };
            let yci = vk::SamplerYcbcrConversionCreateInfo::default()
                .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
                .ycbcr_model(match enc {
                    ColorEncoding::Bt601 => vk::SamplerYcbcrModelConversion::YCBCR_601,
                    ColorEncoding::Bt709 => vk::SamplerYcbcrModelConversion::YCBCR_709,
                    ColorEncoding::Bt2020 => vk::SamplerYcbcrModelConversion::YCBCR_2020,
                })
                .ycbcr_range(match range {
                    ColorRange::Limited => vk::SamplerYcbcrRange::ITU_NARROW,
                    ColorRange::Full => vk::SamplerYcbcrRange::ITU_FULL,
                })
                .x_chroma_offset(loc)
                .y_chroma_offset(loc)
                .chroma_filter(filter);
            // SAFETY: samplerYcbcrConversion is enabled; the format's
            // chroma location and filter features were checked when the
            // modifier tables were built.
            f.conv = unsafe { d.create_sampler_ycbcr_conversion(&yci, None) }
                .map_err(be("vkCreateSamplerYcbcrConversion"))?;
            cinfo = cinfo.conversion(f.conv);
        }
        let mut sci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        if f.conv != vk::SamplerYcbcrConversion::null() {
            sci = sci.push_next(&mut cinfo);
        }
        // SAFETY: valid create info; the chained conversion is alive.
        f.sampler = unsafe { d.create_sampler(&sci, None) }.map_err(be("vkCreateSampler"))?;
        let samplers = [f.sampler];
        let bind = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        let dlci = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bind);
        // SAFETY: valid create info; `samplers` outlives the call.
        f.dsl = unsafe { d.create_descriptor_set_layout(&dlci, None) }
            .map_err(be("vkCreateDescriptorSetLayout"))?;
        let push = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: PUSH_SIZE,
        }];
        let sets = [f.dsl];
        let plci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&sets)
            .push_constant_ranges(&push);
        // SAFETY: valid create info.
        f.layout = unsafe { d.create_pipeline_layout(&plci, None) }
            .map_err(be("vkCreatePipelineLayout"))?;

        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.frag)
                .name(c"main"),
        ];
        let vi = vk::PipelineVertexInputStateCreateInfo::default();
        let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let vp = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rs = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let ms = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let dyns = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let ds = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyns);
        let opaque = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let over = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)];
        let cb = [
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&opaque),
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&over),
        ];
        let gpci: Vec<_> = cb
            .iter()
            .map(|cb| {
                vk::GraphicsPipelineCreateInfo::default()
                    .stages(&stages)
                    .vertex_input_state(&vi)
                    .input_assembly_state(&ia)
                    .viewport_state(&vp)
                    .rasterization_state(&rs)
                    .multisample_state(&ms)
                    .color_blend_state(cb)
                    .dynamic_state(&ds)
                    .layout(f.layout)
                    .render_pass(self.render_pass)
                    .subpass(0)
            })
            .collect();
        // SAFETY: every create info references live objects created above
        // or in `new`, and state structs that outlive the call.
        let pipes = unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &gpci, None) }
            .map_err(|(made, e)| {
                for p in made {
                    // SAFETY: created just now, never used.
                    unsafe { d.destroy_pipeline(p, None) };
                }
                BackendError::new(format!("vkCreateGraphicsPipelines: {e}"))
            })?;
        f.pipes = [pipes[0], pipes[1]];
        Ok(())
    }

    /// Destroy everything. The device must be idle.
    pub fn destroy(&mut self, gpu: &Gpu) {
        for f in self.families.values() {
            destroy_family(gpu, f);
        }
        self.families.clear();
        let d = &gpu.device;
        // SAFETY: the device is idle (caller) and no descriptor set,
        // framebuffer or pipeline using these objects survives; destroying
        // a null handle is a no-op.
        unsafe {
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.frag, None);
            d.destroy_render_pass(self.render_pass, None);
        }
        self.pool = vk::DescriptorPool::null();
        self.vert = vk::ShaderModule::null();
        self.frag = vk::ShaderModule::null();
        self.render_pass = vk::RenderPass::null();
    }
}

fn destroy_family(gpu: &Gpu, f: &Family) {
    let d = &gpu.device;
    // SAFETY: the device is idle (callers: teardown, or a family that
    // failed creation and was never used); null handles are no-ops.
    unsafe {
        for p in f.pipes {
            d.destroy_pipeline(p, None);
        }
        d.destroy_pipeline_layout(f.layout, None);
        d.destroy_descriptor_set_layout(f.dsl, None);
        d.destroy_sampler(f.sampler, None);
        d.destroy_sampler_ycbcr_conversion(f.conv, None);
    }
}
