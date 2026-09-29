/*
 * The whole of nitro-video's contact with FFmpeg (#3906).
 *
 * A deliberately tiny C API over libavformat/libavcodec/libavutil, so the
 * Rust side needs a handful of `extern "C"` functions and no struct
 * layouts: FFmpeg's structs change between majors, these five functions
 * do not. Everything returns plain integers, doubles and byte buffers.
 *
 *   nv_open   open a file, pick the best video stream, open its decoder
 *   nv_info   size, duration, colour metadata
 *   nv_seek   seek to the keyframe at or before a time, flush the decoder
 *   nv_next   decode the next frame into a tightly packed NV12 buffer
 *   nv_close  free everything
 *
 * No libswscale: the software decoders nitro-video meets output 8-bit
 * 4:2:0 (yuv420p, yuvj420p) or NV12 already, and interleaving chroma is
 * a loop, not a library. Anything else is refused with its name.
 */

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/avutil.h>
#include <libavutil/pixdesc.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef struct nv_ctx {
    AVFormatContext *fmt;
    AVCodecContext *dec;
    AVPacket *pkt;
    AVFrame *frame;
    int stream;
    AVRational tb;
    int64_t start; /* stream start_time, in tb; 0 if unknown */
    int draining;
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
    av_frame_free(&c->frame);
    av_packet_free(&c->pkt);
    avcodec_free_context(&c->dec);
    avformat_close_input(&c->fmt);
    av_free(c);
}

nv_ctx *nv_open(const char *path, int threads, char *err, int errlen) {
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
    c->dec->thread_count = threads;
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

/* 1: a frame, *pts_us set; 0: end of stream; <0: error. */
int nv_next(nv_ctx *c, uint8_t *dst, size_t dstlen, int w, int h, int64_t *pts_us, char *err,
            int errlen) {
    if (w <= 0 || h <= 0 || (w & 1) || (h & 1) || dstlen < (size_t)w * h * 3 / 2 ||
        w > c->dec->width || h > c->dec->height) {
        set_err(err, errlen, "bad destination", 0);
        return -1;
    }
    for (;;) {
        int r = avcodec_receive_frame(c->dec, c->frame);
        if (r == 0) {
            if (c->frame->width < w || c->frame->height < h) {
                set_err(err, errlen, "the stream changed size", 0);
                av_frame_unref(c->frame);
                return -1;
            }
            int64_t ts = c->frame->best_effort_timestamp;
            if (ts == AV_NOPTS_VALUE) ts = c->frame->pts;
            if (ts == AV_NOPTS_VALUE) ts = c->start;
            *pts_us = av_rescale_q(ts - c->start, c->tb, AV_TIME_BASE_Q);
            r = to_nv12(c->frame, dst, w, h, err, errlen);
            av_frame_unref(c->frame);
            return r < 0 ? -1 : 1;
        }
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
