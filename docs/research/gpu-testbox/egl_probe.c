#include "probe.h"
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include "va_nv12.h"
static GLuint sh(GLenum t, const char *s) { GLuint o = glCreateShader(t); glShaderSource(o, 1, &s, NULL); glCompileShader(o); return o; }
int main(int argc, char **argv) {
    int full = argc > 1 && !strcmp(argv[1], "full");
    const char *plat = getenv("PLAT"); int use_gbm = plat && !strcmp(plat, "gbm");
    struct nv12_export nv = { 0 };
    cp("start");
    if (full) { if (va_nv12_make(&nv, 1920, 1080, VA_EXPORT_SURFACE_COMPOSED_LAYERS)) return 1; cp("after_va(baseline)"); }
    double t0 = now_ms();
    PFNEGLGETPLATFORMDISPLAYEXTPROC gpd = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay d;
    if (use_gbm) { int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC); d = gpd(EGL_PLATFORM_GBM_KHR, gbm_create_device(fd), NULL); }
    else d = gpd(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    EGLint ma, mi; if (!eglInitialize(d, &ma, &mi)) { printf("eglInitialize failed\n"); return 1; }
    double t_init = now_ms() - t0; cp("eglInitialize");
    eglBindAPI(EGL_OPENGL_ES_API); t0 = now_ms();
    EGLint ca[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_NONE };
    EGLContext c = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ca);
    eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c);
    double t_ctx = now_ms() - t0;
    printf("GL_RENDERER %s | %s\n", glGetString(GL_RENDERER), glGetString(GL_VERSION)); cp("context_current");
    GLuint tex, fbo; glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 1920, 1080, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    glViewport(0, 0, 1920, 1080); glClearColor(0.25f, 0.5f, 0.75f, 1); glClear(GL_COLOR_BUFFER_BIT);
    if (full) {
        PFNGLEGLIMAGETARGETTEXTURE2DOESPROC tgt = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
        VADRMPRIMESurfaceDescriptor *s = &nv.d; uint64_t mod = s->objects[0].drm_format_modifier;
        EGLAttrib at[] = { EGL_WIDTH, 1920, EGL_HEIGHT, 1080, EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_NV12,
            EGL_DMA_BUF_PLANE0_FD_EXT, s->objects[0].fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, s->layers[0].offset[0],
            EGL_DMA_BUF_PLANE0_PITCH_EXT, s->layers[0].pitch[0],
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLAttrib)(mod & 0xffffffff), EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLAttrib)(mod >> 32),
            EGL_DMA_BUF_PLANE1_FD_EXT, s->objects[0].fd, EGL_DMA_BUF_PLANE1_OFFSET_EXT, s->layers[0].offset[1],
            EGL_DMA_BUF_PLANE1_PITCH_EXT, s->layers[0].pitch[1],
            EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, (EGLAttrib)(mod & 0xffffffff), EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT, (EGLAttrib)(mod >> 32),
            EGL_YUV_COLOR_SPACE_HINT_EXT, EGL_ITU_REC709_EXT, EGL_SAMPLE_RANGE_HINT_EXT, EGL_YUV_NARROW_RANGE_EXT, EGL_NONE };
        EGLImage img = eglCreateImage(d, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, at);
        RESULT("egl_import_nv12 (modifier)", img != EGL_NO_IMAGE, "err=0x%x", eglGetError());
        GLuint et; glGenTextures(1, &et); glBindTexture(GL_TEXTURE_EXTERNAL_OES, et); tgt(GL_TEXTURE_EXTERNAL_OES, img);
        GLuint p = glCreateProgram();
        glAttachShader(p, sh(GL_VERTEX_SHADER, "#version 300 es\nout vec2 uv;void main(){vec2 q=vec2((gl_VertexID<<1)&2,gl_VertexID&2);uv=q;gl_Position=vec4(q*2.0-1.0,0,1);}"));
        glAttachShader(p, sh(GL_FRAGMENT_SHADER, "#version 300 es\n#extension GL_OES_EGL_image_external_essl3 : require\nprecision mediump float;in vec2 uv;uniform samplerExternalOES t;out vec4 o;void main(){o=vec4(texture(t,uv).rgb,1);}"));
        glLinkProgram(p); glUseProgram(p); glDrawArrays(GL_TRIANGLES, 0, 3);
    }
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fd = (void *)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    EGLSync sy = eglCreateSync(d, EGL_SYNC_NATIVE_FENCE_ANDROID, NULL); glFlush();
    int sfd = dup_fd(d, sy);
    RESULT("egl_native_fence_fd", sfd >= 0 && fd_signaled(sfd, 2000), "fd=%d", sfd);
    unsigned char px[4]; glReadPixels(960, 540, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    float w[3] = { 0.25f, 0.5f, 0.75f }; if (full) expected_rgb(w);
    RESULT(full ? "gl_external_oes_nv12_readback" : "gl_clear_readback",
           abs(px[0] - (int)(w[0]*255+.5f)) <= 3 && abs(px[1] - (int)(w[1]*255+.5f)) <= 3 && abs(px[2] - (int)(w[2]*255+.5f)) <= 3,
           "got=(%d,%d,%d) want=(%.0f,%.0f,%.0f)", px[0], px[1], px[2], w[0]*255, w[1]*255, w[2]*255);
    cp("first_draw_done");
    printf("TIME eglInitialize=%.1fms context=%.1fms\n", t_init, t_ctx);
    if (getenv("HOLD")) sleep(3);
    return 0;
}
