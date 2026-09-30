//! The decoder seam's player side: which output to use and what size to
//! scale to. The seam itself (the [`VideoSource`] trait, the frame types
//! and the synthetic source) lives in `nitro-media` (#3988) and is
//! re-exported here, so `nitro_video::decode::…` paths keep working.
//!
//! v1 has one real backend, [`crate::ffmpeg::LibavDecoder`] (`FFmpeg`'s
//! libavformat + libavcodec, software decode), which writes NV12 rows
//! straight into a shared-memory ring slot the player lends it. It runs
//! on the player's decode thread, so a backend is `Send` and blocking.
//!
//! The same backend decodes on VA-API when it can (#3923: `FFmpeg`'s
//! hwaccel, `AV_HWDEVICE_TYPE_VAAPI` + DRM PRIME export). A hardware
//! decoder has two outputs, and the player picks one at start
//! ([`choose_output`]): **download** into the shm ring through
//! [`VideoSource::next_frame`] as before, or hand each decoded surface back as
//! an NV12 dma-buf through [`VideoSource::next_dmabuf`] ([`FrameBuf::DmaBuf`],
//! registered once per surface, returned with [`VideoSource::release`]).
//! Nothing in the player's pacing or controls changes: it presents
//! whatever buffer a frame names.
//!
//! No `FFmpeg` type appears here or anywhere outside `ffmpeg.rs`, so the
//! backend can move into a `nitro-media` helper without touching the player.

pub use nitro_media::{
    DmabufDesc, DmabufFrame, DmabufPlane, FrameBuf, HwDec, HwInfo, Matrix, Nv12Layout, StreamInfo,
    SyntheticDecoder, VideoSource,
};

/// How the decode thread hands frames over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// NV12 copies into the shm ring ([`VideoSource::next_frame`]).
    Shm,
    /// The decoder's own surfaces as dma-bufs ([`VideoSource::next_dmabuf`]).
    DmaBuf,
}

/// Most dma-buf registrations the player keeps: headroom under the
/// server's 32 buffers per client. A decoder that uses more surfaces
/// than this (a pool that grew) has its least recently used idle ones
/// re-registered on demand.
pub const MAX_DMABUF_BUFFERS: usize = 24;

/// Pick the output for a decoder: `hw` its facts (`None`: software),
/// `granted` whether the server gave `DMABUF`, `direct_scanout` its
/// `DIRECT_SCANOUT` bit, `formats` its default feedback (`None`: none
/// arrived). Returns the output and, for a hardware decoder that ends up
/// downloading, why.
#[must_use]
pub fn choose_output(
    pref: HwDec,
    hw: Option<HwInfo>,
    granted: bool,
    direct_scanout: bool,
    formats: Option<&[nitro_wire::types::DmabufFormat]>,
) -> (Output, Option<String>) {
    use nitro_wire::types::{dmabuf_flags, format, modifier};
    let Some(hw) = hw else {
        return (Output::Shm, None);
    };
    let why = |s: String| (Output::Shm, Some(s));
    match pref {
        HwDec::Download | HwDec::Off => return (Output::Shm, None),
        HwDec::Auto | HwDec::DmaBuf => {}
    }
    if hw.modifier == modifier::INVALID {
        return why("the VA surfaces cannot be exported as dma-bufs".into());
    }
    if !granted {
        return why("the server did not grant caps::DMABUF".into());
    }
    let Some(formats) = formats else {
        return why("no DmabufFeedback from the server".into());
    };
    let Some(f) = formats
        .iter()
        .find(|f| f.format == format::NV12 && f.modifier == hw.modifier)
        .filter(|f| f.flags & dmabuf_flags::IMPORT != 0)
    else {
        return why(format!(
            "the server does not import NV12 with modifier {:#x}",
            hw.modifier
        ));
    };
    let shown = f.flags & dmabuf_flags::CPU != 0
        || (f.flags & dmabuf_flags::SCANOUT != 0 && direct_scanout);
    if pref == HwDec::Auto && !shown {
        return why(format!(
            "NV12 with modifier {:#x} is neither CPU-readable nor scanned out by the server",
            hw.modifier
        ));
    }
    if hw.pool > MAX_DMABUF_BUFFERS {
        return why(format!(
            "a {}-surface pool is more than {MAX_DMABUF_BUFFERS} registrations",
            hw.pool
        ));
    }
    (Output::DmaBuf, None)
}

/// Scale-pool surfaces: what the decode thread holds out ([`crate::player::RING`])
/// plus the one being filled.
pub const SCALE_POOL: usize = crate::player::RING + 1;

// The synthetic source's default scaled pool mirrors this one.
const _: () = assert!(SCALE_POOL == SyntheticDecoder::DEFAULT_SCALE_POOL);

/// Upscale a plane may absorb before a rescale is worth it, percent: a
/// window grown by up to this much keeps its scaled size (#3956).
pub const SCALE_KEEP_UP_PCT: u64 = 110;

/// The size to have the decoder scale to (#3956), or `None` for native.
///
/// `src` is the stream, `hint` the Surface's device-pixel size from
/// `SurfacePlaneHint`, `min_pct` the plane's downscale floor (0: no
/// planes, so size does not matter), `current` what is scaled to now.
/// The target keeps the stream's aspect inside `hint`, rounded to even.
/// Scale only when the plane could not take the stream as it is (never
/// upscale; a plane takes that). Hysteresis: `current` is kept while the
/// plane can take it to the hinted size — downscale to the floor, upscale
/// to [`SCALE_KEEP_UP_PCT`] — so a window drag does not reallocate the
/// pool at every step.
#[must_use]
pub fn scale_target(
    src: (u32, u32),
    hint: (u32, u32),
    min_pct: u8,
    current: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    let (sw, sh) = (u64::from(src.0.max(1)), u64::from(src.1.max(1)));
    let (hw, hh) = (u64::from(hint.0), u64::from(hint.1));
    if min_pct == 0 || hw == 0 || hh == 0 {
        return None;
    }
    let min = u64::from(min_pct);
    // The stream's aspect inside the hint.
    let (fw, fh) = if sw * hh > hw * sh {
        (hw, (hw * sh + sw / 2) / sw)
    } else {
        ((hh * sw + sh / 2) / sh, hh)
    };
    let (fw, fh) = (fw.min(sw), fh.min(sh));
    if fw * 100 >= sw * min && fh * 100 >= sh * min {
        return None;
    }
    if let Some((cw, ch)) = current {
        let (cw, ch) = (u64::from(cw), u64::from(ch));
        let keeps = |f: u64, c: u64| f * 100 >= c * min && f * 100 <= c * SCALE_KEEP_UP_PCT;
        if keeps(fw, cw) && keeps(fh, ch) {
            return current;
        }
    }
    let even = |v: u64| u32::try_from((v & !1).max(2)).unwrap_or(u32::MAX);
    Some((even(fw), even(fh)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_targets_fit_the_plane_with_hysteresis() {
        let src = (1920, 1080);
        // KBL, 1080p in the default 1600×900 window: 0.83× < 0.94×.
        assert_eq!(scale_target(src, (1600, 900), 94, None), Some((1600, 900)));
        // Letterboxed hint: the aspect is kept.
        assert_eq!(scale_target(src, (1600, 1000), 94, None), Some((1600, 900)));
        // Fullscreen 2560×1440: the plane upscales.
        assert_eq!(scale_target(src, (2560, 1440), 94, None), None);
        // Within the floor: no needless downscale.
        assert_eq!(scale_target(src, (1820, 1024), 94, None), None);
        // No planes: size does not matter.
        assert_eq!(scale_target(src, (800, 450), 0, None), None);
        // HSW (no plane scaling): any smaller hint scales.
        assert_eq!(
            scale_target(src, (1900, 1068), 100, None),
            Some((1898, 1068))
        );
        // A drag: small moves keep the current size, big ones rescale,
        // and fitting again goes native.
        let mut cur = scale_target(src, (1600, 900), 94, None);
        let mut sizes = Vec::new();
        for w in [1590u32, 1560, 1520, 1500, 1480, 1560, 1620, 1700, 1850] {
            let h = w * 9 / 16;
            let next = scale_target(src, (w, h), 94, cur);
            if next != cur {
                sizes.push(next);
            }
            cur = next;
        }
        assert_eq!(
            sizes,
            vec![Some((1498, 842)), Some((1700, 956)), None],
            "one rescale per big step"
        );
    }

    #[test]
    fn the_output_policy() {
        use nitro_wire::types::{DmabufFormat, dmabuf_flags as fl, format, modifier};
        let lin = HwInfo {
            modifier: modifier::LINEAR,
            pool: 0,
        };
        let y = HwInfo {
            modifier: modifier::I915_Y_TILED,
            pool: 0,
        };
        let fb = [
            DmabufFormat {
                format: format::NV12,
                modifier: modifier::LINEAR,
                flags: fl::CPU | fl::IMPORT,
            },
            DmabufFormat {
                format: format::NV12,
                modifier: modifier::I915_Y_TILED,
                flags: fl::SCANOUT | fl::IMPORT,
            },
        ];
        let go = |p, hw, ds, f: Option<&[DmabufFormat]>| choose_output(p, hw, true, ds, f).0;
        assert_eq!(go(HwDec::Auto, None, false, Some(&fb)), Output::Shm);
        assert_eq!(go(HwDec::Auto, Some(lin), false, Some(&fb)), Output::DmaBuf);
        assert_eq!(go(HwDec::Auto, Some(y), false, Some(&fb)), Output::Shm);
        assert_eq!(go(HwDec::Auto, Some(y), true, Some(&fb)), Output::DmaBuf);
        assert_eq!(go(HwDec::DmaBuf, Some(y), false, Some(&fb)), Output::DmaBuf);
        assert_eq!(
            go(HwDec::DmaBuf, Some(y), false, Some(&fb[..1])),
            Output::Shm
        );
        assert_eq!(
            go(HwDec::Download, Some(lin), false, Some(&fb)),
            Output::Shm
        );
        assert_eq!(go(HwDec::Auto, Some(lin), false, None), Output::Shm);
        let big = HwInfo { pool: 30, ..lin };
        assert_eq!(go(HwDec::Auto, Some(big), false, Some(&fb)), Output::Shm);
        let (o, why) = choose_output(HwDec::Auto, Some(lin), false, false, Some(&fb));
        assert_eq!(o, Output::Shm);
        assert!(why.unwrap().contains("DMABUF"));
    }
}
