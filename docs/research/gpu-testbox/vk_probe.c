/* vk_probe.c — does hasvk meet the nitro-gpu helper minimum? (task #3903)
 *   ./vk_probe full   VA NV12 dma-buf -> VkImage(modifier) -> YCbCr sample -> render pass into an
 *                     exported scanout-modifier XRGB image -> AddFB2 check; SYNC_FD export/import.
 *   ./vk_probe rss    instance + device + exported render target + clear/submit only (no VA).
 * Build: gcc -O2 vk_probe.c -o vk_probe -lvulkan -lva -lva-drm -ldrm -I/usr/include/libdrm
 */
#include "probe.h"
#include <vulkan/vulkan.h>
#include <sys/ioctl.h>
#include <linux/dma-buf.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>
#include "va_nv12.h"

#define W 1920
#define H 1080
#define CK(x) do { VkResult r_ = (x); if (r_) { printf("VKFAIL %s = %d (line %d)\n", #x, r_, __LINE__); fflush(stdout); exit(1);} } while (0)
#define DPFN(n) PFN_##n n = (PFN_##n)vkGetDeviceProcAddr(dev, #n)

static VkPhysicalDevice pd;
static VkDevice dev;
static VkPhysicalDeviceMemoryProperties mp;

static uint32_t memtype(uint32_t bits, VkMemoryPropertyFlags want) {
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
    return ~0u;
}

static VkFormatFeatureFlags mod_features(VkFormat f, uint64_t mod, int print, const char *name) {
    VkDrmFormatModifierPropertiesEXT m[32];
    VkDrmFormatModifierPropertiesListEXT l = { .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
                                               .drmFormatModifierCount = 32, .pDrmFormatModifierProperties = m };
    VkFormatProperties2 p = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &l };
    vkGetPhysicalDeviceFormatProperties2(pd, f, &p);
    VkFormatFeatureFlags ret = 0;
    if (print) printf("FORMAT %s modifiers=%u\n", name, l.drmFormatModifierCount);
    for (uint32_t i = 0; i < l.drmFormatModifierCount; i++) {
        VkFormatFeatureFlags ff = m[i].drmFormatModifierTilingFeatures;
        if (m[i].drmFormatModifier == mod) ret = ff;
        if (print)
            printf("  mod 0x%016llx planes=%u%s%s%s%s%s%s\n", (unsigned long long)m[i].drmFormatModifier,
                   m[i].drmFormatModifierPlaneCount, ff & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT ? " sampled" : "",
                   ff & VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT ? " color_attachment" : "",
                   ff & VK_FORMAT_FEATURE_MIDPOINT_CHROMA_SAMPLES_BIT ? " ycbcr_midpoint" : "",
                   ff & VK_FORMAT_FEATURE_COSITED_CHROMA_SAMPLES_BIT ? " ycbcr_cosited" : "",
                   ff & VK_FORMAT_FEATURE_SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER_BIT ? " ycbcr_linear" : "",
                   ff & VK_FORMAT_FEATURE_DISJOINT_BIT ? " disjoint" : "");
    }
    return ret;
}

static void ext_image_props(VkFormat f, uint64_t mod, VkImageUsageFlags usage, const char *name) {
    VkPhysicalDeviceImageDrmFormatModifierInfoEXT mi = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT, .drmFormatModifier = mod };
    VkPhysicalDeviceExternalImageFormatInfo ei = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
                                                   .pNext = &mi,
                                                   .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    VkPhysicalDeviceImageFormatInfo2 fi = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
                                            .pNext = &ei, .format = f, .type = VK_IMAGE_TYPE_2D,
                                            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, .usage = usage };
    VkExternalImageFormatProperties ep = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
    VkImageFormatProperties2 p = { .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &ep };
    VkResult r = vkGetPhysicalDeviceImageFormatProperties2(pd, &fi, &p);
    VkExternalMemoryFeatureFlags ff = ep.externalMemoryProperties.externalMemoryFeatures;
    printf("EXTIMG %-28s mod=0x%016llx r=%d max=%ux%u%s%s%s\n", name, (unsigned long long)mod, r,
           p.imageFormatProperties.maxExtent.width, p.imageFormatProperties.maxExtent.height,
           ff & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT ? " importable" : "",
           ff & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT ? " exportable" : "",
           ff & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT ? " dedicated_only" : "");
}

static VkShaderModule load_spv(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) { printf("missing %s\n", path); exit(1); }
    static uint32_t buf[16384];
    size_t n = fread(buf, 1, sizeof buf, f);
    fclose(f);
    VkShaderModuleCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = n, .pCode = buf };
    VkShaderModule m;
    CK(vkCreateShaderModule(dev, &ci, NULL, &m));
    return m;
}

/* Try AddFB2 on the primary node (no DRM master needed): proves KMS accepts the buffer. */
static int try_addfb(int dmabuf, uint32_t fourcc, int nplanes, const uint32_t *pitch, const uint32_t *off,
                     uint64_t mod, int w, int h, char *why) {
    int card = open("/dev/dri/card1", O_RDWR | O_CLOEXEC);
    uint32_t handle = 0, fb = 0;
    if (card < 0 || drmPrimeFDToHandle(card, dmabuf, &handle)) { sprintf(why, "prime import: %m"); return 0; }
    uint32_t hs[4] = { 0 }, ps[4] = { 0 }, os[4] = { 0 };
    uint64_t ms[4] = { 0 };
    for (int i = 0; i < nplanes; i++) { hs[i] = handle; ps[i] = pitch[i]; os[i] = off[i]; ms[i] = mod; }
    int r = drmModeAddFB2WithModifiers(card, w, h, fourcc, hs, ps, os, ms, &fb, DRM_MODE_FB_MODIFIERS);
    sprintf(why, "AddFB2 r=%d%s%s", r, r ? " " : "", r ? strerror(-r) : "");
    if (!r) drmModeRmFB(card, fb);
    drmCloseBufferHandle(card, handle);
    close(card);
    return r == 0;
}

int main(int argc, char **argv) {
    int full = argc > 1 && !strcmp(argv[1], "full");
    struct nv12_export nv = { 0 };
    cp("start");
    if (full) {
        int r = va_nv12_make(&nv, W, H, VA_EXPORT_SURFACE_COMPOSED_LAYERS);
        RESULT("va_export_nv12_composed", r == 0, "r=%d", r);
        if (r) return 1;
        va_desc_print(&nv.d);
        char why[128];
        int ok = try_addfb(nv.d.objects[0].fd, DRM_FORMAT_NV12, 2, nv.d.layers[0].pitch, nv.d.layers[0].offset,
                           nv.d.objects[0].drm_format_modifier, W, H, why);
        RESULT("kms_addfb_va_nv12", ok, "%s", why);
        cp("after_va(baseline)");
    }

    /* ---- instance ---- */
    double t0 = now_ms();
    VkApplicationInfo ai = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_2 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &ai };
    VkInstance inst;
    CK(vkCreateInstance(&ici, NULL, &inst));
    uint32_t n = 8;
    VkPhysicalDevice pds[8];
    CK(vkEnumeratePhysicalDevices(inst, &n, pds));
    double t_inst = now_ms() - t0;
    VkPhysicalDeviceProperties pp;
    for (uint32_t i = 0; i < n; i++) {
        vkGetPhysicalDeviceProperties(pds[i], &pp);
        if (pp.vendorID == 0x8086) { pd = pds[i]; break; }
    }
    if (!pd) { printf("no intel device\n"); return 1; }
    printf("DEVICE %s api=%u.%u.%u (%u physical devices enumerated)\n", pp.deviceName, VK_API_VERSION_MAJOR(pp.apiVersion),
           VK_API_VERSION_MINOR(pp.apiVersion), VK_API_VERSION_PATCH(pp.apiVersion), n);
    cp("instance");
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);

    /* ---- capability queries ---- */
    VkPhysicalDeviceExternalSemaphoreInfo si = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO,
                                                 .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT };
    VkExternalSemaphoreProperties sp = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES };
    vkGetPhysicalDeviceExternalSemaphoreProperties(pd, &si, &sp);
    RESULT("semaphore_sync_fd_props",
           (sp.externalSemaphoreFeatures & 3) == 3, "export=%d import=%d",
           !!(sp.externalSemaphoreFeatures & VK_EXTERNAL_SEMAPHORE_FEATURE_EXPORTABLE_BIT),
           !!(sp.externalSemaphoreFeatures & VK_EXTERNAL_SEMAPHORE_FEATURE_IMPORTABLE_BIT));
    mod_features(VK_FORMAT_B8G8R8A8_UNORM, 0, 1, "B8G8R8A8_UNORM (XR24/AR24)");
    mod_features(VK_FORMAT_G8_B8R8_2PLANE_420_UNORM, 0, 1, "G8_B8R8_2PLANE_420 (NV12)");
    mod_features(VK_FORMAT_G8B8G8R8_422_UNORM, 0, 1, "G8B8G8R8_422 (YUYV)");
    uint64_t probe_mods[] = { DRM_FORMAT_MOD_LINEAR, I915_FORMAT_MOD_X_TILED, I915_FORMAT_MOD_Y_TILED };
    for (int i = 0; i < 3; i++) {
        ext_image_props(VK_FORMAT_B8G8R8A8_UNORM, probe_mods[i],
                        VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT, "XRGB render target");
        ext_image_props(VK_FORMAT_G8_B8R8_2PLANE_420_UNORM, probe_mods[i], VK_IMAGE_USAGE_SAMPLED_BIT, "NV12 sampled");
    }

    /* ---- device ---- */
    t0 = now_ms();
    const char *exts[] = { "VK_EXT_external_memory_dma_buf", "VK_KHR_external_memory_fd",
                           "VK_EXT_image_drm_format_modifier", "VK_KHR_external_semaphore_fd",
                           "VK_EXT_queue_family_foreign" };
    VkPhysicalDeviceVulkan11Features f11 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
                                             .samplerYcbcrConversion = VK_TRUE };
    VkPhysicalDeviceFeatures2 f2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &f11 };
    float prio = 1;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = 0,
                                    .queueCount = 1, .pQueuePriorities = &prio };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f2, .queueCreateInfoCount = 1,
                               .pQueueCreateInfos = &qci, .enabledExtensionCount = 5, .ppEnabledExtensionNames = exts };
    CK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue q;
    vkGetDeviceQueue(dev, 0, 0, &q);
    double t_dev = now_ms() - t0;
    RESULT("device_with_interop_exts", 1, "exts=dma_buf,memory_fd,drm_format_modifier,semaphore_fd,queue_family_foreign +samplerYcbcrConversion");
    cp("device");
    DPFN(vkGetMemoryFdKHR);
    DPFN(vkGetMemoryFdPropertiesKHR);
    DPFN(vkGetSemaphoreFdKHR);
    DPFN(vkImportSemaphoreFdKHR);
    DPFN(vkGetImageDrmFormatModifierPropertiesEXT);

    /* ---- exportable render target restricted to scanout modifiers ---- */
    const char *modenv = getenv("RT_MODS"); /* "x", "linear" or unset = both */
    uint64_t rt_mods[2];
    uint32_t nmods = 0;
    if (!modenv || !strcmp(modenv, "x")) rt_mods[nmods++] = I915_FORMAT_MOD_X_TILED;
    if (!modenv || !strcmp(modenv, "linear")) rt_mods[nmods++] = DRM_FORMAT_MOD_LINEAR;
    VkImageDrmFormatModifierListCreateInfoEXT ml = { .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
                                                     .drmFormatModifierCount = nmods, .pDrmFormatModifiers = rt_mods };
    VkExternalMemoryImageCreateInfo emi = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO, .pNext = &ml,
                                            .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    VkImageCreateInfo ic = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &emi, .imageType = VK_IMAGE_TYPE_2D,
                             .format = VK_FORMAT_B8G8R8A8_UNORM, .extent = { W, H, 1 }, .mipLevels = 1, .arrayLayers = 1,
                             .samples = 1, .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
                             .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                             .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    VkImage rt;
    CK(vkCreateImage(dev, &ic, NULL, &rt));
    VkMemoryRequirements mr;
    vkGetImageMemoryRequirements(dev, rt, &mr);
    VkMemoryDedicatedAllocateInfo ded = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = rt };
    VkExportMemoryAllocateInfo exa = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO, .pNext = &ded,
                                       .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    VkMemoryAllocateInfo ma = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &exa, .allocationSize = mr.size,
                                .memoryTypeIndex = memtype(mr.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
    VkDeviceMemory rtmem;
    CK(vkAllocateMemory(dev, &ma, NULL, &rtmem));
    CK(vkBindImageMemory(dev, rt, rtmem, 0));
    VkImageDrmFormatModifierPropertiesEXT rtm = { .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT };
    CK(vkGetImageDrmFormatModifierPropertiesEXT(dev, rt, &rtm));
    VkImageSubresource sub = { .aspectMask = VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT };
    VkSubresourceLayout rtl;
    vkGetImageSubresourceLayout(dev, rt, &sub, &rtl);
    VkMemoryGetFdInfoKHR gfi = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR, .memory = rtmem,
                                 .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    int rtfd = -1;
    VkResult er = vkGetMemoryFdKHR(dev, &gfi, &rtfd);
    RESULT("rt_export_dmabuf", er == 0 && rtfd >= 0, "mod=0x%016llx pitch=%llu offset=%llu size=%llu",
           (unsigned long long)rtm.drmFormatModifier, (unsigned long long)rtl.rowPitch, (unsigned long long)rtl.offset,
           (unsigned long long)mr.size);
    {
        char why[128];
        uint32_t pitch = rtl.rowPitch, off = rtl.offset;
        int ok = try_addfb(rtfd, DRM_FORMAT_XRGB8888, 1, &pitch, &off, rtm.drmFormatModifier, W, H, why);
        RESULT("kms_addfb_vk_rt (scanout-capable)", ok, "%s", why);
    }
    cp("rt_image_exported");

    /* ---- render pass + framebuffer ---- */
    VkAttachmentDescription att = { .format = VK_FORMAT_B8G8R8A8_UNORM, .samples = 1,
                                    .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                    .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                    .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                    .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                    .finalLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkAttachmentReference ar = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkSubpassDescription spd = { .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS, .colorAttachmentCount = 1,
                                 .pColorAttachments = &ar };
    VkRenderPassCreateInfo rpci = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, .attachmentCount = 1,
                                    .pAttachments = &att, .subpassCount = 1, .pSubpasses = &spd };
    VkRenderPass rp;
    CK(vkCreateRenderPass(dev, &rpci, NULL, &rp));
    VkImageViewCreateInfo vci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = rt,
                                  .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
                                  .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
    VkImageView rtv;
    CK(vkCreateImageView(dev, &vci, NULL, &rtv));
    VkFramebufferCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = rp,
                                    .attachmentCount = 1, .pAttachments = &rtv, .width = W, .height = H, .layers = 1 };
    VkFramebuffer fb;
    CK(vkCreateFramebuffer(dev, &fci, NULL, &fb));

    /* ---- NV12 import + YCbCr pipeline (full mode) ---- */
    VkPipeline pipe = VK_NULL_HANDLE;
    VkPipelineLayout pl = VK_NULL_HANDLE;
    VkDescriptorSet ds = VK_NULL_HANDLE;
    VkImage nvimg = VK_NULL_HANDLE;
    double t_pipe = 0;
    VkSemaphore waitsem = VK_NULL_HANDLE;
    if (full) {
        VADRMPRIMESurfaceDescriptor *d = &nv.d;
        uint64_t mod = d->objects[0].drm_format_modifier;
        VkFormatFeatureFlags ff = mod_features(VK_FORMAT_G8_B8R8_2PLANE_420_UNORM, mod, 0, "");
        VkSubresourceLayout pls[2] = { { .offset = d->layers[0].offset[0], .rowPitch = d->layers[0].pitch[0] },
                                       { .offset = d->layers[0].offset[1], .rowPitch = d->layers[0].pitch[1] } };
        VkImageDrmFormatModifierExplicitCreateInfoEXT exi = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT, .drmFormatModifier = mod,
            .drmFormatModifierPlaneCount = 2, .pPlaneLayouts = pls };
        VkExternalMemoryImageCreateInfo emi2 = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
                                                 .pNext = &exi,
                                                 .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
        VkImageCreateInfo nic = ic;
        nic.pNext = &emi2;
        nic.format = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM;
        nic.usage = VK_IMAGE_USAGE_SAMPLED_BIT;
        VkResult r = vkCreateImage(dev, &nic, NULL, &nvimg);
        if (r) { RESULT("nv12_import_image", 0, "vkCreateImage=%d (mod 0x%llx features=0x%x)", r, (unsigned long long)mod, ff); return 1; }
        VkMemoryFdPropertiesKHR fp = { .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR };
        CK(vkGetMemoryFdPropertiesKHR(dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, d->objects[0].fd, &fp));
        vkGetImageMemoryRequirements(dev, nvimg, &mr);
        VkMemoryDedicatedAllocateInfo ded2 = { .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO, .image = nvimg };
        VkImportMemoryFdInfoKHR imp = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR, .pNext = &ded2,
                                        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                        .fd = dup(d->objects[0].fd) };
        VkMemoryAllocateInfo ma2 = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &imp,
                                     .allocationSize = mr.size, .memoryTypeIndex = memtype(mr.memoryTypeBits & fp.memoryTypeBits, 0) };
        VkDeviceMemory nvmem;
        r = vkAllocateMemory(dev, &ma2, NULL, &nvmem);
        if (!r) r = vkBindImageMemory(dev, nvimg, nvmem, 0);
        RESULT("nv12_import_image (VA dma-buf+modifier)", r == 0, "r=%d mod=0x%016llx req=%llu dmabuf=%u", r,
               (unsigned long long)mod, (unsigned long long)mr.size, d->objects[0].size);
        if (r) return 1;

        VkChromaLocation cl = (ff & VK_FORMAT_FEATURE_MIDPOINT_CHROMA_SAMPLES_BIT) ? VK_CHROMA_LOCATION_MIDPOINT
                                                                                   : VK_CHROMA_LOCATION_COSITED_EVEN;
        VkSamplerYcbcrConversionCreateInfo yci = {
            .sType = VK_STRUCTURE_TYPE_SAMPLER_YCBCR_CONVERSION_CREATE_INFO, .format = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
            .ycbcrModel = VK_SAMPLER_YCBCR_MODEL_CONVERSION_YCBCR_709, .ycbcrRange = VK_SAMPLER_YCBCR_RANGE_ITU_NARROW,
            .xChromaOffset = cl, .yChromaOffset = cl, .chromaFilter = VK_FILTER_NEAREST };
        VkSamplerYcbcrConversion conv;
        CK(vkCreateSamplerYcbcrConversion(dev, &yci, NULL, &conv));
        VkSamplerYcbcrConversionInfo cinfo = { .sType = VK_STRUCTURE_TYPE_SAMPLER_YCBCR_CONVERSION_INFO, .conversion = conv };
        VkSamplerCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO, .pNext = &cinfo,
                                    .magFilter = VK_FILTER_NEAREST, .minFilter = VK_FILTER_NEAREST,
                                    .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
                                    .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
                                    .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE };
        VkSampler smp;
        CK(vkCreateSampler(dev, &sci, NULL, &smp));
        VkImageViewCreateInfo nvci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .pNext = &cinfo, .image = nvimg,
                                       .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
                                       .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
        VkImageView nvv;
        CK(vkCreateImageView(dev, &nvci, NULL, &nvv));
        VkDescriptorSetLayoutBinding b = { 0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_FRAGMENT_BIT, &smp };
        VkDescriptorSetLayoutCreateInfo dli = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                                                .bindingCount = 1, .pBindings = &b };
        VkDescriptorSetLayout dsl;
        CK(vkCreateDescriptorSetLayout(dev, &dli, NULL, &dsl));
        VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 3 };
        VkDescriptorPoolCreateInfo dpi = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1,
                                           .poolSizeCount = 1, .pPoolSizes = &ps };
        VkDescriptorPool dp;
        CK(vkCreateDescriptorPool(dev, &dpi, NULL, &dp));
        VkDescriptorSetAllocateInfo dai = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = dp,
                                            .descriptorSetCount = 1, .pSetLayouts = &dsl };
        CK(vkAllocateDescriptorSets(dev, &dai, &ds));
        VkDescriptorImageInfo dii = { smp, nvv, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL };
        VkWriteDescriptorSet wds = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .descriptorCount = 1,
                                     .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, .pImageInfo = &dii };
        vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);
        cp("nv12_imported");

        t0 = now_ms();
        VkPipelineLayoutCreateInfo pli = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1,
                                           .pSetLayouts = &dsl };
        CK(vkCreatePipelineLayout(dev, &pli, NULL, &pl));
        VkPipelineShaderStageCreateInfo st[2] = {
            { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
              .module = load_spv("tri.vert.spv"), .pName = "main" },
            { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
              .module = load_spv("nv12.frag.spv"), .pName = "main" } };
        VkPipelineVertexInputStateCreateInfo vi = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
        VkPipelineInputAssemblyStateCreateInfo ia = { .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                                      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
        VkViewport vp = { 0, 0, W, H, 0, 1 };
        VkRect2D sc = { { 0, 0 }, { W, H } };
        VkPipelineViewportStateCreateInfo vps = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                                  .viewportCount = 1, .pViewports = &vp, .scissorCount = 1, .pScissors = &sc };
        VkPipelineRasterizationStateCreateInfo rs = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                                      .cullMode = VK_CULL_MODE_NONE, .lineWidth = 1 };
        VkPipelineMultisampleStateCreateInfo ms = { .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                                    .rasterizationSamples = 1 };
        VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
        VkPipelineColorBlendStateCreateInfo cb = { .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                                   .attachmentCount = 1, .pAttachments = &cba };
        VkGraphicsPipelineCreateInfo gpi = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .stageCount = 2,
                                             .pStages = st, .pVertexInputState = &vi, .pInputAssemblyState = &ia,
                                             .pViewportState = &vps, .pRasterizationState = &rs,
                                             .pMultisampleState = &ms, .pColorBlendState = &cb, .layout = pl,
                                             .renderPass = rp };
        CK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpi, NULL, &pipe));
        t_pipe = now_ms() - t0;
        cp("pipeline");

        /* SYNC_FD import: the producer's implicit write fence from the dma-buf becomes our wait semaphore. */
        struct dma_buf_export_sync_file es = { .flags = DMA_BUF_SYNC_READ, .fd = -1 };
        int ir = ioctl(d->objects[0].fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &es);
        VkSemaphoreCreateInfo sci0 = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
        CK(vkCreateSemaphore(dev, &sci0, NULL, &waitsem));
        VkImportSemaphoreFdInfoKHR isi = { .sType = VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR, .semaphore = waitsem,
                                           .flags = VK_SEMAPHORE_IMPORT_TEMPORARY_BIT,
                                           .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT, .fd = es.fd };
        VkResult sr = ir ? VK_ERROR_UNKNOWN : vkImportSemaphoreFdKHR(dev, &isi);
        RESULT("sync_fd_import (dmabuf EXPORT_SYNC_FILE -> VkSemaphore)", ir == 0 && sr == 0, "ioctl=%d vk=%d", ir, sr);
        if (sr) { vkDestroySemaphore(dev, waitsem, NULL); waitsem = VK_NULL_HANDLE; }
    }

    /* ---- record + submit ---- */
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 4,
                               .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    VkBuffer rb;
    CK(vkCreateBuffer(dev, &bci, NULL, &rb));
    vkGetBufferMemoryRequirements(dev, rb, &mr);
    VkMemoryAllocateInfo rma = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size,
                                 .memoryTypeIndex = memtype(mr.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                                                                   VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
    VkDeviceMemory rbm;
    CK(vkAllocateMemory(dev, &rma, NULL, &rbm));
    CK(vkBindBufferMemory(dev, rb, rbm, 0));

    VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = 0 };
    VkCommandPool cpool;
    CK(vkCreateCommandPool(dev, &cpi, NULL, &cpool));
    VkCommandBufferAllocateInfo cbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
    VkCommandBuffer cmd;
    CK(vkAllocateCommandBuffers(dev, &cbi, &cmd));
    VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                    .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    CK(vkBeginCommandBuffer(cmd, &bi));
    if (full) { /* acquire the foreign NV12 image */
        VkImageMemoryBarrier acq = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
                                     .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
                                     .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                                     .srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT, .dstQueueFamilyIndex = 0,
                                     .image = nvimg, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
        vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0,
                             NULL, 1, &acq);
    }
    VkClearValue clr = { .color.float32 = { 0.25f, 0.5f, 0.75f, 1 } };
    VkRenderPassBeginInfo rbi = { .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = rp, .framebuffer = fb,
                                  .renderArea = { { 0, 0 }, { W, H } }, .clearValueCount = 1, .pClearValues = &clr };
    vkCmdBeginRenderPass(cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
    if (full) {
        vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
        vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pl, 0, 1, &ds, 0, NULL);
        vkCmdDraw(cmd, 3, 1, 0, 0);
    }
    vkCmdEndRenderPass(cmd);
    VkImageMemoryBarrier b1 = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                                .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                                .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                                .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                                .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                .image = rt, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0,
                         NULL, 1, &b1);
    VkBufferImageCopy reg = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                              .imageOffset = { W / 2, H / 2, 0 }, .imageExtent = { 1, 1, 1 } };
    vkCmdCopyImageToBuffer(cmd, rt, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, rb, 1, &reg);
    /* release the render target to the foreign queue (KMS) */
    VkImageMemoryBarrier rel = b1;
    rel.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    rel.dstAccessMask = 0;
    rel.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    rel.newLayout = VK_IMAGE_LAYOUT_GENERAL;
    rel.srcQueueFamilyIndex = 0;
    rel.dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT;
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1,
                         &rel);
    CK(vkEndCommandBuffer(cmd));

    VkExportSemaphoreCreateInfo esc = { .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
                                        .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT };
    VkSemaphoreCreateInfo sci1 = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &esc };
    VkSemaphore donesem;
    CK(vkCreateSemaphore(dev, &sci1, NULL, &donesem));
    VkFenceCreateInfo fnci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    CK(vkCreateFence(dev, &fnci, NULL, &fence));
    VkPipelineStageFlags wst = VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT;
    VkSubmitInfo sub_i = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = waitsem ? 1 : 0,
                           .pWaitSemaphores = &waitsem, .pWaitDstStageMask = &wst, .commandBufferCount = 1,
                           .pCommandBuffers = &cmd, .signalSemaphoreCount = 1, .pSignalSemaphores = &donesem };
    t0 = now_ms();
    CK(vkQueueSubmit(q, 1, &sub_i, fence));
    VkSemaphoreGetFdInfoKHR sgi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR, .semaphore = donesem,
                                    .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT };
    int syncfd = -1;
    VkResult ser = vkGetSemaphoreFdKHR(dev, &sgi, &syncfd);
    int sig = syncfd >= 0 && fd_signaled(syncfd, 2000);
    double t_submit = now_ms() - t0;
    RESULT("sync_fd_export (VkSemaphore -> sync_file)", ser == 0 && sig, "vk=%d fd=%d signalled=%d", ser, syncfd, sig);
    /* attach it to the exported dma-buf so KMS / other importers implicitly wait on it */
    struct dma_buf_import_sync_file isf = { .flags = DMA_BUF_SYNC_WRITE, .fd = syncfd };
    int iret = syncfd >= 0 ? ioctl(rtfd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &isf) : -1;
    RESULT("sync_fd -> rt dma-buf IMPORT_SYNC_FILE", iret == 0, "ioctl=%d %s", iret, iret ? strerror(errno) : "");
    CK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 2000000000ull));
    cp("first_submit_done");

    uint8_t *px;
    CK(vkMapMemory(dev, rbm, 0, 4, 0, (void **)&px));
    float want[3];
    if (full) expected_rgb(want);
    else { want[0] = 0.25f; want[1] = 0.5f; want[2] = 0.75f; }
    int got[3] = { px[2], px[1], px[0] }; /* BGRA */
    int okc = 1;
    for (int i = 0; i < 3; i++) {
        int w8 = (int)(want[i] * 255 + 0.5f);
        if (w8 < 0) w8 = 0;
        if (w8 > 255) w8 = 255;
        if (abs(got[i] - w8) > 3) okc = 0;
    }
    RESULT(full ? "ycbcr_sample_nv12_readback" : "clear_readback", okc, "got rgb=(%d,%d,%d) want=(%.0f,%.0f,%.0f)",
           got[0], got[1], got[2], want[0] * 255, want[1] * 255, want[2] * 255);
    printf("TIME instance+enumerate=%.1fms device=%.1fms pipeline=%.1fms first_submit=%.1fms\n", t_inst, t_dev, t_pipe,
           t_submit);
    cp("end");
    if (getenv("HOLD")) sleep(3);
    return 0;
}
