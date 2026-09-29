/* va_export.c — hand a VA-API NV12 surface to nitro as a client dma-buf (task #3918).
 *   ./va_export [path/to/dmabuf_import] [extra args...]
 * Creates a 1920x1080 NV12 VA surface on renderD128, fills it (TY/TU/TV from probe.h, a flat
 * colour), exports it with vaExportSurfaceHandle(DRM_PRIME_2, composed layers, read-only), prints
 * the layout and execs
 *   dmabuf_import --fd 3 --format NV12 --modifier 0x.. --size 1920x1080 --planes o0:p0,o1:p1
 * with the exported object's fd dup2'd to 3 (default binary: ~/nitro-bin/dmabuf_import).
 * Build: gcc -O2 va_export.c -o va_export -lva -lva-drm
 */
#include "probe.h"
#include "va_nv12.h"

#define W 1920
#define H 1080

int main(int argc, char **argv) {
    struct nv12_export nv = { 0 };
    double t0 = now_ms();
    int r = va_nv12_make(&nv, W, H, VA_EXPORT_SURFACE_COMPOSED_LAYERS);
    if (r) { printf("va_nv12_make failed: %d\n", r); return 1; }
    printf("VA surface %ux%u made, filled and exported in %.1f ms\n", W, H, now_ms() - t0);
    va_desc_print(&nv.d);
    VADRMPRIMESurfaceDescriptor *d = &nv.d;
    if (d->num_layers != 1 || d->layers[0].num_planes != 2 || d->num_objects != 1) {
        printf("unexpected layout (want 1 object, 1 layer of 2 planes)\n"); return 1;
    }
    uint64_t mod = d->objects[0].drm_format_modifier;
    if (dup2(d->objects[0].fd, 3) != 3) { perror("dup2"); return 1; }  /* dup2 clears CLOEXEC */
    char modarg[32], planes[64], size[32];
    snprintf(modarg, sizeof modarg, "0x%016llx", (unsigned long long)mod);
    snprintf(planes, sizeof planes, "%u:%u,%u:%u", d->layers[0].offset[0], d->layers[0].pitch[0],
             d->layers[0].offset[1], d->layers[0].pitch[1]);
    snprintf(size, sizeof size, "%ux%u", W, H);
    char def[512];
    const char *bin = argc > 1 ? argv[1] : (snprintf(def, sizeof def, "%s/nitro-bin/dmabuf_import", getenv("HOME")), def);
    char *args[32] = { (char *)bin, "--fd", "3", "--format", "NV12", "--modifier", modarg,
                       "--size", size, "--planes", planes };
    int n = 11;
    for (int i = 2; i < argc && n < 31; i++) args[n++] = argv[i];
    args[n] = NULL;
    printf("exec:"); for (int i = 0; i < n; i++) printf(" %s", args[i]); printf("\n"); fflush(stdout);
    /* The VA display and surface die with this image; the dma-buf lives on in fd 3. */
    execv(bin, args);
    perror("execv");
    return 1;
}
