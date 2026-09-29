#pragma once
#include <fcntl.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
struct nv12_export { VADisplay dpy; VASurfaceID surf; VADRMPRIMESurfaceDescriptor d; };
static int va_nv12_make(struct nv12_export *e, int w, int h, uint32_t flags) {
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    e->dpy = vaGetDisplayDRM(fd); int maj, min;
    if (vaInitialize(e->dpy, &maj, &min) != VA_STATUS_SUCCESS) return -1;
    VASurfaceAttrib a = { .type = VASurfaceAttribPixelFormat, .flags = VA_SURFACE_ATTRIB_SETTABLE,
                          .value = { .type = VAGenericValueTypeInteger, .value.i = VA_FOURCC_NV12 } };
    if (vaCreateSurfaces(e->dpy, VA_RT_FORMAT_YUV420, w, h, &e->surf, 1, &a, 1) != VA_STATUS_SUCCESS) return -2;
    VAImage img; if (vaDeriveImage(e->dpy, e->surf, &img) != VA_STATUS_SUCCESS) return -3;
    unsigned char *p; vaMapBuffer(e->dpy, img.buf, (void **)&p);
    for (int y = 0; y < h; y++) memset(p + img.offsets[0] + y * img.pitches[0], TY, w);
    for (int y = 0; y < h / 2; y++) for (int x = 0; x < w / 2; x++) {
        p[img.offsets[1] + y * img.pitches[1] + 2 * x] = TU; p[img.offsets[1] + y * img.pitches[1] + 2 * x + 1] = TV; }
    vaUnmapBuffer(e->dpy, img.buf); vaDestroyImage(e->dpy, img.image_id); vaSyncSurface(e->dpy, e->surf);
    if (vaExportSurfaceHandle(e->dpy, e->surf, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                              VA_EXPORT_SURFACE_READ_ONLY | flags, &e->d) != VA_STATUS_SUCCESS) return -4;
    return 0;
}
static void va_desc_print(const VADRMPRIMESurfaceDescriptor *d) {
    printf("  fourcc=%.4s %ux%u objects=%u layers=%u\n", (char *)&d->fourcc, d->width, d->height, d->num_objects, d->num_layers);
    for (unsigned i = 0; i < d->num_objects; i++)
        printf("  object[%u] fd=%d size=%u modifier=0x%016llx\n", i, d->objects[i].fd, d->objects[i].size,
               (unsigned long long)d->objects[i].drm_format_modifier);
    for (unsigned l = 0; l < d->num_layers; l++) {
        printf("  layer[%u] drm_format=%.4s planes=%u", l, (char *)&d->layers[l].drm_format, d->layers[l].num_planes);
        for (unsigned p = 0; p < d->layers[l].num_planes; p++)
            printf(" [obj=%u off=%u pitch=%u]", d->layers[l].object_index[p], d->layers[l].offset[p], d->layers[l].pitch[p]);
        printf("\n");
    }
}
