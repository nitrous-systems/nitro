/*
 * The whole of nitro-video's contact with FFmpeg (#3906).
 *
 * A deliberately small C API over libavformat/libavcodec/libavutil, so the
 * Rust side needs a handful of `extern "C"` functions and no struct
 * layouts: FFmpeg's structs change between majors, these functions do
 * not. Everything returns plain integers, doubles, byte buffers and fds.
 *
 *   nv_open     open a file, pick the best video stream, open its decoder
 *               (software, or VA-API through FFmpeg's hwaccel, #3923)
 *   nv_info     size, duration, colour metadata
 *   nv_hw_info  VA-API only: the surface pool's size and the modifier
 *   nv_seek     seek to the keyframe at or before a time, flush the decoder
 *   nv_next     decode the next frame into a tightly packed NV12 buffer
 *               (a VA-API frame is downloaded first)
 *   nv_next_hw  VA-API only: the next frame as a held VA surface, exported
 *               as an NV12 dma-buf (fds, offsets, pitches) the first time
 *               that surface is seen
 *   nv_release  give a held VA surface back to the decoder's pool
 *   nv_close    free everything
 *
 * VA-API (#3923): `AV_HWDEVICE_TYPE_VAAPI` on a render node, frames stay
 * in VA surfaces (`AV_PIX_FMT_VAAPI`), and `av_hwframe_map` to
 * `AV_PIX_FMT_DRM_PRIME` exports them (which also `vaSyncSurface`s, so a
 * frame handed out is complete). Anything the hardware cannot do -- no
 * device, no hwaccel for the codec, a profile the driver refuses (FFmpeg
 * then falls back to its software format), not 8-bit 4:2:0 -- makes
 * `nv_open` fail with `*unsupported = 1`, and the caller opens the file
 * again in software. To find that out without a seek, `nv_open` decodes
 * the first frame and keeps it pending for the first `nv_next*`.
 *
 * No libswscale: the software decoders nitro-video meets output 8-bit
 * 4:2:0 (yuv420p, yuvj420p) or NV12 already, and interleaving chroma is
 * a loop, not a library. Anything else is refused with its name.
 */

/* F_DUPFD_CLOEXEC */
#define _POSIX_C_SOURCE 200809L

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/avutil.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixdesc.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* DRM fourccs, spelled out so the build needs no libdrm headers. */
#define NV_FOURCC(a, b, c, d) \
    ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define NV_DRM_NV12 NV_FOURCC('N', 'V', '1', '2')
#define NV_DRM_R8 NV_FOURCC('R', '8', ' ', ' ')
#define NV_MOD_INVALID 0x00ffffffffffffffULL

/* VA surfaces handed out and not yet released; more than any pool. */
#define NV_MAX_HELD 64

typedef struct nv_ctx {
    AVFormatContext *fmt;
    AVCodecContext *dec;
    AVPacket *pkt;
    AVFrame *frame;
    int stream;
    AVRational tb;
    int64_t start; /* stream start_time, in tb; 0 if unknown */
    int draining;
    /* VA-API */
    AVBufferRef *hw_dev;
    int hw;          /* the decoder outputs AV_PIX_FMT_VAAPI frames */
    int hw_refused;  /* get_format was offered no VAAPI (fell back) */
    AVFrame *pending; /* the first frame, decoded by nv_open */
    int has_pending;
    AVFrame *sw;     /* download target */
    int w0, h0;      /* the first frame's size */
    int pool;        /* surfaces in the VA frames pool */
    uint64_t modifier;
    /* Keys: generation << 24 | VA surface id. The generation moves when
     * FFmpeg re-creates the frames pool (a mid-stream change), since
     * surface ids are only unique within one pool. A ref on the current
     * pool keeps its address from being reused by the next. */
    AVBufferRef *frames_ref;
    uint32_t gen;
    struct {
        uint32_t key;
        AVFrame *f;
    } held[NV_MAX_HELD];
} nv_ctx;

static void set_err(char *err, int errlen, const char *what, int code) {
    if (!err || errlen <= 0) return;
    if (code) {
        char buf[AV_ERROR_MAX_STRING_SIZE] = {0};
        av_strerror(code, buf, sizeof buf);
        snprintf(err, (size_t)errlen, "%s: %s", what, buf);
    } else {
        snprintf(err, (size_t)errlen, "%s", what);
    }
}

void nv_close(nv_ctx *c) {
    if (!c) return;
    for (int i = 0; i < NV_MAX_HELD; i++) av_frame_free(&c->held[i].f);
    av_frame_free(&c->pending);
    av_frame_free(&c->sw);
    av_buffer_unref(&c->frames_ref);
    av_frame_free(&c->frame);
    av_packet_free(&c->pkt);
    avcodec_free_context(&c->dec);
    avformat_close_input(&c->fmt);
    av_buffer_unref(&c->hw_dev);
    av_free(c);
}

/* get_format: VAAPI when offered. FFmpeg calls this again without VAAPI
 * when the hwaccel cannot init (a profile the driver refuses); that is
 * the fallback, noted so nv_open can say why. */
static enum AVPixelFormat pick_format(AVCodecContext *dec, const enum AVPixelFormat *fmts) {
    nv_ctx *c = dec->opaque;
    for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++)
        if (*p == AV_PIX_FMT_VAAPI) return *p;
    c->hw_refused = 1;
    for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++) {
        const AVPixFmtDescriptor *d = av_pix_fmt_desc_get(*p);
        if (d && !(d->flags & AV_PIX_FMT_FLAG_HWACCEL)) return *p;
    }
    return fmts[0];
}

static int next_raw(nv_ctx *c, AVFrame *out, char *err, int errlen);
static int export_drm(const AVFrame *f, int fds[4], uint32_t offsets[4], uint32_t pitches[4],
                      int *nplanes, uint64_t *modifier, char *err, int errlen);

/* hw: 0 software, 1 VA-API on `device` (a render node path). */
nv_ctx *nv_open(const char *path, int threads, int hw, const char *device, char *err,
                int errlen) {
    av_log_set_level(AV_LOG_ERROR);
    nv_ctx *c = av_mallocz(sizeof *c);
    if (!c) {
        set_err(err, errlen, "out of memory", 0);
        return NULL;
    }
    int r = avformat_open_input(&c->fmt, path, NULL, NULL);
    if (r < 0) {
        set_err(err, errlen, "cannot open", r);
        nv_close(c);
        return NULL;
    }
    r = avformat_find_stream_info(c->fmt, NULL);
    if (r < 0) {
        set_err(err, errlen, "cannot read stream info", r);
        nv_close(c);
        return NULL;
    }
    const AVCodec *codec = NULL;
    r = av_find_best_stream(c->fmt, AVMEDIA_TYPE_VIDEO, -1, -1, &codec, 0);
    if (r < 0 || !codec) {
        set_err(err, errlen, r == AVERROR_DECODER_NOT_FOUND ? "no decoder for the video stream"
                                                             : "no video stream",
                0);
        nv_close(c);
        return NULL;
    }
    c->stream = r;
    AVStream *st = c->fmt->streams[r];
    /* Only the video stream is read: audio packets are skipped by the
     * demuxer instead of being handed to us and dropped. */
    for (unsigned i = 0; i < c->fmt->nb_streams; i++)
        if ((int)i != r) c->fmt->streams[i]->discard = AVDISCARD_ALL;
    c->tb = st->time_base;
    c->start = st->start_time == AV_NOPTS_VALUE ? 0 : st->start_time;
    c->dec = avcodec_alloc_context3(codec);
    c->pkt = av_packet_alloc();
    c->frame = av_frame_alloc();
    if (!c->dec || !c->pkt || !c->frame) {
        set_err(err, errlen, "out of memory", 0);
        nv_close(c);
        return NULL;
    }
    r = avcodec_parameters_to_context(c->dec, st->codecpar);
    if (r < 0) {
        set_err(err, errlen, "bad codec parameters", r);
        nv_close(c);
        return NULL;
    }
    /* Frame threads only above 1080p (#3924). Each frame thread holds its
     * own H.264 context and a picture in flight: +10.5 MB per thread at
     * 1080p. One thread decodes 1080p30 x264 High in ~37 % of box1's
     * Pentium core, and on box1's two cores the frame-threaded decoder
     * cost *more* CPU (51 % vs 37 %) and more late frames, because it
     * competes with the UI thread and the server. Slice threads are no
     * alternative: x264 writes one slice per frame by default, so they
     * add a thread and no parallelism. */
    if ((int64_t)c->dec->width * c->dec->height <= 1920 * 1088 || hw) threads = 1;
    c->dec->thread_count = threads;
    if (hw) {
        int offered = 0;
        for (int i = 0;; i++) {
            const AVCodecHWConfig *cfg = avcodec_get_hw_config(codec, i);
            if (!cfg) break;
            if ((cfg->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX) &&
                cfg->device_type == AV_HWDEVICE_TYPE_VAAPI)
                offered = 1;
        }
        if (!offered) {
            char msg[128];
            snprintf(msg, sizeof msg, "FFmpeg's %s decoder has no VA-API hwaccel", codec->name);
            set_err(err, errlen, msg, 0);
            nv_close(c);
            return NULL;
        }
        r = av_hwdevice_ctx_create(&c->hw_dev, AV_HWDEVICE_TYPE_VAAPI, device, NULL, 0);
        if (r < 0) {
            char msg[256];
            snprintf(msg, sizeof msg, "no VA-API device at %s", device ? device : "(default)");
            set_err(err, errlen, msg, r);
            nv_close(c);
            return NULL;
        }
        c->dec->hw_device_ctx = av_buffer_ref(c->hw_dev);
        c->dec->opaque = c;
        c->dec->get_format = pick_format;
        /* Frames the player and the server hold beyond the decoder's own
         * references. Only a fixed-size pool (pre-VA-API-1 FFmpeg) reads
         * it; a dynamic pool grows to what is in flight by itself. */
        c->dec->extra_hw_frames = 4;
        c->hw = 1;
    }
    c->dec->pkt_timebase = st->time_base;
    r = avcodec_open2(c->dec, codec, NULL);
    if (r < 0) {
        set_err(err, errlen, "cannot open the decoder", r);
        nv_close(c);
        return NULL;
    }
    if (c->dec->width <= 0 || c->dec->height <= 0) {
        set_err(err, errlen, "the video stream has no size", 0);
        nv_close(c);
        return NULL;
    }
    if (hw) {
        /* Decode the first frame now: only a frame proves the hardware
         * takes this stream. It waits in `pending` for nv_next*. */
        c->pending = av_frame_alloc();
        c->sw = av_frame_alloc();
        if (!c->pending || !c->sw) {
            set_err(err, errlen, "out of memory", 0);
            nv_close(c);
            return NULL;
        }
        char why[256] = {0};
        r = next_raw(c, c->pending, why, sizeof why);
        if (r <= 0 || c->pending->format != AV_PIX_FMT_VAAPI) {
            const char *codec_name = c->dec->codec->name;
            char msg[384];
            if (r < 0)
                snprintf(msg, sizeof msg, "VA-API decode of %s failed: %s", codec_name, why);
            else if (r == 0)
                snprintf(msg, sizeof msg, "no frame to decode");
            else
                snprintf(msg, sizeof msg, "the VA-API driver does not decode this %s profile",
                         codec_name);
            set_err(err, errlen, msg, 0);
            nv_close(c);
            return NULL;
        }
        const AVHWFramesContext *fc = (const AVHWFramesContext *)c->pending->hw_frames_ctx->data;
        if (fc->sw_format != AV_PIX_FMT_NV12) {
            const char *name = av_get_pix_fmt_name(fc->sw_format);
            char msg[128];
            snprintf(msg, sizeof msg, "VA-API decodes into %s, not NV12 (8-bit 4:2:0 only)",
                     name ? name : "?");
            set_err(err, errlen, msg, 0);
            nv_close(c);
            return NULL;
        }
        c->pool = fc->initial_pool_size;
        c->has_pending = 1;
        /* A trial export: the modifier for the player's policy, and
         * whether dma-bufs come out at all (INVALID if not). */
        int fds[4] = {-1, -1, -1, -1}, n = 0;
        uint32_t off[4], pitch[4];
        c->modifier = NV_MOD_INVALID;
        if (export_drm(c->pending, fds, off, pitch, &n, &c->modifier, NULL, 0) == 0)
            for (int i = 0; i < n; i++) close(fds[i]);
        else
            c->modifier = NV_MOD_INVALID;
    }
    return c;
}

/* colorspace: 0 = BT.601, 1 = BT.709, 2 = BT.2020, -1 = unspecified.
 * full_range: 1 for "JPEG" range. codec: the decoder's short name. */
void nv_info(const nv_ctx *c, int *width, int *height, double *duration, int *colorspace,
             int *full_range, char *codec, int codeclen) {
    const AVStream *st = c->fmt->streams[c->stream];
    *width = c->dec->width;
    *height = c->dec->height;
    if (c->fmt->duration != AV_NOPTS_VALUE && c->fmt->duration > 0)
        *duration = (double)c->fmt->duration / AV_TIME_BASE;
    else if (st->duration != AV_NOPTS_VALUE && st->duration > 0)
        *duration = (double)st->duration * av_q2d(st->time_base);
    else
        *duration = 0.0;
    switch (c->dec->colorspace) {
    case AVCOL_SPC_BT709: *colorspace = 1; break;
    case AVCOL_SPC_BT470BG:
    case AVCOL_SPC_SMPTE170M: *colorspace = 0; break;
    case AVCOL_SPC_BT2020_NCL:
    case AVCOL_SPC_BT2020_CL: *colorspace = 2; break;
    default: *colorspace = -1; break;
    }
    *full_range = c->dec->color_range == AVCOL_RANGE_JPEG || c->dec->pix_fmt == AV_PIX_FMT_YUVJ420P;
    if (codec && codeclen > 0) snprintf(codec, (size_t)codeclen, "%s", c->dec->codec->name);
}

/* VA-API only (0 otherwise): the fixed pool's size (0 = dynamic) and the
 * exported surfaces' DRM modifier (DRM_FORMAT_MOD_INVALID: no export). */
int nv_hw_info(const nv_ctx *c, int *pool, uint64_t *modifier) {
    *pool = c->pool;
    *modifier = c->modifier;
    return c->hw;
}

int nv_seek(nv_ctx *c, double secs, char *err, int errlen) {
    int64_t ts = (int64_t)(secs / av_q2d(c->tb)) + c->start;
    int r = av_seek_frame(c->fmt, c->stream, ts, AVSEEK_FLAG_BACKWARD);
    if (r < 0) {
        /* Some demuxers refuse a backward seek before the first packet. */
        r = av_seek_frame(c->fmt, c->stream, c->start, AVSEEK_FLAG_BACKWARD | AVSEEK_FLAG_ANY);
    }
    if (r < 0) {
        set_err(err, errlen, "seek failed", r);
        return r;
    }
    avcodec_flush_buffers(c->dec);
    c->draining = 0;
    if (c->has_pending) {
        av_frame_unref(c->pending);
        c->has_pending = 0;
    }
    return 0;
}

/* Copy c->frame into dst as NV12 of w x h (even, <= the frame's size). */
static int to_nv12(const AVFrame *f, uint8_t *dst, int w, int h, char *err, int errlen) {
    uint8_t *y = dst, *uv = dst + (size_t)w * h;
    for (int row = 0; row < h; row++)
        memcpy(y + (size_t)row * w, f->data[0] + (size_t)row * f->linesize[0], (size_t)w);
    switch (f->format) {
    case AV_PIX_FMT_YUV420P:
    case AV_PIX_FMT_YUVJ420P:
        for (int row = 0; row < h / 2; row++) {
            const uint8_t *u = f->data[1] + (size_t)row * f->linesize[1];
            const uint8_t *v = f->data[2] + (size_t)row * f->linesize[2];
            uint8_t *o = uv + (size_t)row * w;
            for (int x = 0; x < w / 2; x++) {
                o[2 * x] = u[x];
                o[2 * x + 1] = v[x];
            }
        }
        return 0;
    case AV_PIX_FMT_NV12:
        for (int row = 0; row < h / 2; row++)
            memcpy(uv + (size_t)row * w, f->data[1] + (size_t)row * f->linesize[1], (size_t)w);
        return 0;
    default: {
        const char *name = av_get_pix_fmt_name(f->format);
        char msg[128];
        snprintf(msg, sizeof msg, "pixel format %s is not supported (8-bit 4:2:0 only)",
                 name ? name : "?");
        set_err(err, errlen, msg, 0);
        return -1;
    }
    }
}

/* The next decoded frame, in presentation order, into `out` (the pending
 * first frame before anything else). 1: a frame; 0: end; <0: error. */
static int next_raw(nv_ctx *c, AVFrame *out, char *err, int errlen) {
    if (c->has_pending) {
        c->has_pending = 0;
        av_frame_move_ref(out, c->pending);
        return 1;
    }
    for (;;) {
        int r = avcodec_receive_frame(c->dec, out);
        if (r == 0) return 1;
        if (r == AVERROR_EOF) return 0;
        if (r != AVERROR(EAGAIN)) {
            set_err(err, errlen, "decode failed", r);
            return -1;
        }
        if (c->draining) return 0;
        r = av_read_frame(c->fmt, c->pkt);
        if (r == AVERROR_EOF) {
            c->draining = 1;
            avcodec_send_packet(c->dec, NULL);
            continue;
        }
        if (r < 0) {
            set_err(err, errlen, "read failed", r);
            return -1;
        }
        if (c->pkt->stream_index == c->stream) {
            r = avcodec_send_packet(c->dec, c->pkt);
            av_packet_unref(c->pkt);
            /* A damaged packet is skipped, not fatal: the next keyframe
             * recovers, as every player does. */
            if (r < 0 && r != AVERROR(EAGAIN) && r != AVERROR_INVALIDDATA) {
                set_err(err, errlen, "decode failed", r);
                return -1;
            }
        } else {
            av_packet_unref(c->pkt);
        }
    }
}

static int64_t pts_of(const nv_ctx *c, const AVFrame *f) {
    int64_t ts = f->best_effort_timestamp;
    if (ts == AV_NOPTS_VALUE) ts = f->pts;
    if (ts == AV_NOPTS_VALUE) ts = c->start;
    return av_rescale_q(ts - c->start, c->tb, AV_TIME_BASE_Q);
}

/* 1: a frame, *pts_us set; 0: end of stream; <0: error. A VA-API frame
 * is downloaded (av_hwframe_transfer_data) and then copied. */
int nv_next(nv_ctx *c, uint8_t *dst, size_t dstlen, int w, int h, int64_t *pts_us, char *err,
            int errlen) {
    if (w <= 0 || h <= 0 || (w & 1) || (h & 1) || dstlen < (size_t)w * h * 3 / 2 ||
        w > c->dec->width || h > c->dec->height) {
        set_err(err, errlen, "bad destination", 0);
        return -1;
    }
    int r = next_raw(c, c->frame, err, errlen);
    if (r <= 0) return r;
    const AVFrame *f = c->frame;
    if (c->frame->format == AV_PIX_FMT_VAAPI) {
        av_frame_unref(c->sw);
        r = av_hwframe_transfer_data(c->sw, c->frame, 0);
        if (r < 0) {
            set_err(err, errlen, "VA-API download failed", r);
            av_frame_unref(c->frame);
            return -1;
        }
        f = c->sw;
    }
    if (c->frame->width < w || c->frame->height < h) {
        set_err(err, errlen, "the stream changed size", 0);
        av_frame_unref(c->frame);
        return -1;
    }
    *pts_us = pts_of(c, c->frame);
    r = to_nv12(f, dst, w, h, err, errlen);
    av_frame_unref(c->frame);
    if (f == c->sw) av_frame_unref(c->sw);
    return r < 0 ? -1 : 1;
}

/* Export VA frame `f` as NV12 planes: map to DRM PRIME (READ, which syncs
 * the surface: the frame is complete once this returns), compose FFmpeg's
 * layers (one NV12 layer, or R8 + GR88 separate layers) into plane 0/1,
 * dup each plane's object fd (CLOEXEC), unmap. */
static int export_drm(const AVFrame *f, int fds[4], uint32_t offsets[4], uint32_t pitches[4],
                      int *nplanes, uint64_t *modifier, char *err, int errlen) {
    AVFrame *m = av_frame_alloc();
    if (!m) {
        set_err(err, errlen, "out of memory", 0);
        return -1;
    }
    m->format = AV_PIX_FMT_DRM_PRIME;
    int r = av_hwframe_map(m, f, AV_HWFRAME_MAP_READ);
    if (r < 0) {
        set_err(err, errlen, "exporting a VA surface as a dma-buf", r);
        av_frame_free(&m);
        return -1;
    }
    const AVDRMFrameDescriptor *d = (const AVDRMFrameDescriptor *)m->data[0];
    int n = 0, ok = 1;
    for (int l = 0; l < d->nb_layers && ok; l++)
        for (int p = 0; p < d->layers[l].nb_planes; p++) {
            if (n >= 2) {
                ok = 0;
                break;
            }
            const AVDRMPlaneDescriptor *pl = &d->layers[l].planes[p];
            if (pl->object_index < 0 || pl->object_index >= d->nb_objects) {
                ok = 0;
                break;
            }
            offsets[n] = (uint32_t)pl->offset;
            pitches[n] = (uint32_t)pl->pitch;
            fds[n] = pl->object_index; /* an object index until dup below */
            n++;
        }
    if (ok) {
        if (d->nb_layers == 1)
            ok = d->layers[0].format == NV_DRM_NV12 && n == 2;
        else
            ok = d->nb_layers == 2 && d->layers[0].format == NV_DRM_R8 && n == 2;
    }
    uint64_t mod = d->objects[0].format_modifier;
    for (int i = 1; i < d->nb_objects && ok; i++)
        if (d->objects[i].format_modifier != mod) ok = 0;
    if (!ok) {
        char msg[160];
        snprintf(msg, sizeof msg, "the exported VA surface is not NV12 (%d layers, format %#x)",
                 d->nb_layers, d->nb_layers ? d->layers[0].format : 0);
        set_err(err, errlen, msg, 0);
        av_frame_free(&m);
        return -1;
    }
    for (int i = 0; i < n; i++) {
        int dup = fcntl(d->objects[fds[i]].fd, F_DUPFD_CLOEXEC, 3);
        if (dup < 0) {
            for (int j = 0; j < i; j++) close(fds[j]);
            set_err(err, errlen, "dup of an exported dma-buf failed", 0);
            av_frame_free(&m);
            return -1;
        }
        fds[i] = dup;
    }
    *nplanes = n;
    *modifier = mod;
    av_frame_free(&m); /* unmaps, closing FFmpeg's own fds */
    return 0;
}

/* VA-API: the next frame as a held surface. 1: a frame (its key, size,
 * pts, and n planes of fresh CLOEXEC fds the caller owns); 0: end; <0:
 * error. The surface stays out of the decoder's pool until nv_release. */
int nv_next_hw(nv_ctx *c, int64_t *pts_us, uint32_t *key, int *width, int *height, int fds[4],
               uint32_t offsets[4], uint32_t pitches[4], int *nplanes, uint64_t *modifier,
               char *err, int errlen) {
    int slot = -1;
    for (int i = 0; i < NV_MAX_HELD; i++)
        if (!c->held[i].f) {
            slot = i;
            break;
        }
    if (slot < 0) {
        set_err(err, errlen, "too many VA surfaces held", 0);
        return -1;
    }
    AVFrame *f = av_frame_alloc();
    if (!f) {
        set_err(err, errlen, "out of memory", 0);
        return -1;
    }
    int r = next_raw(c, f, err, errlen);
    if (r <= 0) {
        av_frame_free(&f);
        return r;
    }
    if (f->format != AV_PIX_FMT_VAAPI || !f->hw_frames_ctx) {
        set_err(err, errlen, "the decoder left VA-API mid-stream", 0);
        av_frame_free(&f);
        return -1;
    }
    /* A new frames pool (mid-stream re-init): its surface ids start over,
     * so keys get a new generation. */
    if (!c->frames_ref || c->frames_ref->data != f->hw_frames_ctx->data) {
        av_buffer_unref(&c->frames_ref);
        c->frames_ref = av_buffer_ref(f->hw_frames_ctx);
        c->gen = (c->gen + 1) & 0xff;
    }
    if (export_drm(f, fds, offsets, pitches, nplanes, modifier, err, errlen) < 0) {
        av_frame_free(&f);
        return -1;
    }
    uint32_t surface = (uint32_t)(uintptr_t)f->data[3];
    *key = (c->gen << 24) | (surface & 0xffffff);
    *width = f->width;
    *height = f->height;
    *pts_us = pts_of(c, f);
    c->held[slot].key = *key;
    c->held[slot].f = f;
    return 1;
}

/* Give the surface `key` back to the pool. 0, or -1 if it is not held. */
int nv_release(nv_ctx *c, uint32_t key) {
    for (int i = 0; i < NV_MAX_HELD; i++)
        if (c->held[i].f && c->held[i].key == key) {
            av_frame_free(&c->held[i].f);
            return 0;
        }
    return -1;
}
