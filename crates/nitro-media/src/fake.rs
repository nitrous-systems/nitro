//! A source that makes its frames up, for tests and `--synthetic`.
//!
//! A plain module, not behind a feature: `nitro-video --synthetic` ships
//! it (the `nitro-gpu/src/fake.rs` precedent).

use crate::frame::{DmabufDesc, DmabufFrame, DmabufPlane, HwInfo, Nv12Layout, StreamInfo};
use crate::source::VideoSource;

/// A decoder that makes its frames up: a luma ramp whose level is the
/// frame number, at a fixed rate, with a keyframe every
/// [`SyntheticDecoder::GOP`] frames.
///
/// For the tests (which run without any video file) and for pacing
/// measurements that should not include decode cost.
#[derive(Debug)]
pub struct SyntheticDecoder {
    info: StreamInfo,
    fps: u32,
    frames: u32,
    next: u32,
    hw: Option<SynthHw>,
    /// Surfaces in a scaled pool ([`SyntheticDecoder::with_scale_pool`]).
    scale_pool: usize,
}

/// [`SyntheticDecoder::with_dmabuf`]'s emulated hardware.
#[derive(Debug)]
struct SynthHw {
    modifier: u64,
    /// One sealed memfd per surface (mapped per fill: a mapping is not
    /// `Send`, and the decoder moves to the decode thread).
    surfaces: Vec<std::os::fd::OwnedFd>,
    held: Vec<bool>,
    /// Where the round-robin search for a free surface starts, so every
    /// surface of the pool gets used, as a real decoder's might.
    cursor: usize,
    /// The pool's size (the stream's, or the scale target's).
    size: (u32, u32),
    /// Pool generation, the key's top byte, as the shim's.
    pool_gen: u32,
    /// Native pool size, restored by `set_scale(None)`.
    native_pool: usize,
    /// `set_scale` calls that took effect (tests).
    rescales: u32,
}

impl SyntheticDecoder {
    /// Frames between keyframes, which is where a seek lands.
    pub const GOP: u32 = 10;

    /// Scaled-pool surfaces by default: `nitro-video`'s ring (4) plus
    /// the one being filled, as its `SCALE_POOL`.
    pub const DEFAULT_SCALE_POOL: usize = 5;

    /// `frames` frames of `width × height` at `fps`.
    #[must_use]
    pub fn new(width: u32, height: u32, fps: u32, frames: u32) -> Self {
        let fps = fps.max(1);
        Self {
            info: StreamInfo {
                width,
                height,
                duration: f64::from(frames) / f64::from(fps),
                matrix: StreamInfo::default_matrix(height),
                full_range: false,
                codec: "synthetic".to_owned(),
            },
            fps,
            frames,
            next: 0,
            hw: None,
            scale_pool: Self::DEFAULT_SCALE_POOL,
        }
    }

    /// As [`SyntheticDecoder::new`], emulating a VA-API decoder with
    /// `pool` surfaces, used round-robin and exported with `modifier`
    /// (it reports a pool that grows on demand, [`HwInfo::pool`] 0): sealed memfds
    /// (which the server takes as linear dma-bufs) filled like
    /// [`VideoSource::next_frame`] would. A frame needing a surface when all
    /// are held is an error, which is how a test sees a player holding
    /// more than it should.
    ///
    /// # Errors
    /// A memfd failure.
    pub fn with_dmabuf(
        width: u32,
        height: u32,
        fps: u32,
        frames: u32,
        pool: usize,
        modifier: u64,
    ) -> Result<Self, String> {
        let mut d = Self::new(width, height, fps, frames);
        let l = Nv12Layout::for_video(width, height);
        "synthetic-hw".clone_into(&mut d.info.codec);
        d.hw = Some(SynthHw {
            modifier,
            surfaces: synth_pool(l, pool)?,
            held: vec![false; pool],
            cursor: 0,
            size: (l.width, l.height),
            pool_gen: 0,
            native_pool: pool,
            rescales: 0,
        });
        Ok(d)
    }

    /// Use `n` surfaces for a scaled pool instead of
    /// [`SyntheticDecoder::DEFAULT_SCALE_POOL`].
    #[must_use]
    pub fn with_scale_pool(mut self, n: usize) -> Self {
        self.scale_pool = n.max(1);
        self
    }

    /// The emulated hardware's pool size and effective `set_scale`
    /// calls (tests).
    #[must_use]
    pub fn scale_state(&self) -> Option<((u32, u32), u32)> {
        self.hw.as_ref().map(|h| (h.size, h.rescales))
    }

    /// Surfaces currently held by the player (tests).
    #[must_use]
    pub fn held(&self) -> usize {
        self.hw
            .as_ref()
            .map_or(0, |h| h.held.iter().filter(|&&b| b).count())
    }

    /// The luma level frame `n` is filled with.
    #[must_use]
    pub fn level(n: u32) -> u8 {
        (16 + n % 200) as u8
    }

    /// Frame `n`'s presentation time, microseconds.
    #[must_use]
    pub fn pts_us(&self, n: u32) -> i64 {
        i64::from(n) * 1_000_000 / i64::from(self.fps)
    }
}

impl VideoSource for SyntheticDecoder {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn seek(&mut self, secs: f64) -> Result<(), String> {
        let n = (secs.max(0.0) * f64::from(self.fps)) as u32;
        self.next = (n / Self::GOP * Self::GOP).min(self.frames);
        Ok(())
    }

    fn next_frame(&mut self, dst: &mut [u8], layout: Nv12Layout) -> Result<Option<i64>, String> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let n = self.next;
        self.next += 1;
        let (y, uv) = dst.split_at_mut(layout.luma_len());
        y.fill(Self::level(n));
        uv.fill(128);
        Ok(Some(self.pts_us(n)))
    }

    fn hw(&self) -> Option<HwInfo> {
        self.hw.as_ref().map(|h| HwInfo {
            modifier: h.modifier,
            pool: 0,
        })
    }

    fn next_dmabuf(&mut self) -> Result<Option<DmabufFrame>, String> {
        let Some(size) = self.hw.as_ref().map(|h| h.size) else {
            return Err("this decoder has no dma-bufs".to_owned());
        };
        let l = Nv12Layout::for_video(size.0, size.1);
        if self.next >= self.frames {
            return Ok(None);
        }
        let n = self.next;
        let pts_us = self.pts_us(n);
        let hw = self.hw.as_mut().expect("checked");
        let len = hw.held.len();
        let Some(key) = (0..len)
            .map(|i| (hw.cursor + i) % len)
            .find(|&k| !hw.held[k])
        else {
            return Err(format!(
                "all {} surfaces are held: the player keeps too many frames",
                hw.surfaces.len()
            ));
        };
        self.next += 1;
        hw.held[key] = true;
        hw.cursor = (key + 1) % len;
        let fd = &hw.surfaces[key];
        let mut map = nitro_shm::MappingMut::map_mut(std::os::fd::AsFd::as_fd(fd), l.frame_len())
            .map_err(|e| format!("map: {e}"))?;
        let (y, uv) = map.as_bytes_mut().split_at_mut(l.luma_len());
        y.fill(Self::level(n));
        uv.fill(128);
        drop(map);
        let dup = |fd: &std::os::fd::OwnedFd| rustix::io::dup(fd).map_err(|e| format!("dup: {e}"));
        let desc = DmabufDesc {
            width: l.width,
            height: l.height,
            modifier: hw.modifier,
            planes: vec![
                DmabufPlane {
                    fd: dup(fd)?,
                    offset: 0,
                    stride: l.width,
                },
                DmabufPlane {
                    fd: dup(fd)?,
                    offset: l.luma_len() as u32,
                    stride: l.width,
                },
            ],
        };
        Ok(Some(DmabufFrame {
            pts_us,
            key: (hw.pool_gen << 24) | key as u32,
            desc,
        }))
    }

    fn release(&mut self, key: u32) {
        // A key of an older pool: that pool is gone with its surfaces.
        if let Some(h) = self
            .hw
            .as_mut()
            .filter(|hw| key >> 24 == hw.pool_gen)
            .and_then(|hw| hw.held.get_mut((key & 0xff_ffff) as usize))
        {
            *h = false;
        }
    }

    fn set_scale(&mut self, size: Option<(u32, u32)>) -> Result<(), String> {
        let native = Nv12Layout::for_video(self.info.width, self.info.height);
        let scale_pool = self.scale_pool;
        let Some(hw) = self.hw.as_mut() else {
            return Err("software decode cannot scale".to_owned());
        };
        let (l, pool) = match size {
            Some((w, h)) => (Nv12Layout::for_video(w, h), scale_pool),
            None => (native, hw.native_pool),
        };
        if (l.width, l.height) == hw.size {
            return Ok(());
        }
        hw.surfaces = synth_pool(l, pool)?;
        hw.held = vec![false; pool];
        hw.cursor = 0;
        hw.size = (l.width, l.height);
        hw.pool_gen = (hw.pool_gen + 1) & 0xff;
        hw.rescales += 1;
        Ok(())
    }
}

/// `pool` sealed memfds of one `l` frame each.
fn synth_pool(l: Nv12Layout, pool: usize) -> Result<Vec<std::os::fd::OwnedFd>, String> {
    (0..pool)
        .map(|_| {
            nitro_shm::create_sealed("nitro-media-synth", l.frame_len() as u64)
                .map_err(|e| format!("memfd: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Matrix;

    #[test]
    fn the_synthetic_decoder_seeks_to_a_keyframe() {
        let mut d = SyntheticDecoder::new(4, 2, 10, 25);
        let l = Nv12Layout::for_video(4, 2);
        let mut buf = vec![0; l.frame_len()];
        d.seek(1.7).unwrap();
        assert_eq!(d.next_frame(&mut buf, l).unwrap(), Some(1_000_000));
        assert_eq!(buf[0], SyntheticDecoder::level(10));
        d.seek(9.0).unwrap();
        assert_eq!(d.next_frame(&mut buf, l).unwrap(), None);
        assert_eq!(StreamInfo::default_matrix(720), Matrix::Bt709);
        assert_eq!(StreamInfo::default_matrix(480), Matrix::Bt601);
    }

    #[test]
    fn the_synthetic_hw_decoder_refuses_overholding() {
        let mut d = SyntheticDecoder::with_dmabuf(4, 2, 10, 25, 2, 0).unwrap();
        let a = d.next_dmabuf().unwrap().unwrap();
        let b = d.next_dmabuf().unwrap().unwrap();
        assert_eq!(a.desc.planes.len(), 2);
        assert_eq!(a.desc.planes[1].offset, 8);
        assert_ne!(a.key, b.key);
        assert_eq!(d.held(), 2);
        assert!(
            d.next_dmabuf().is_err(),
            "a third frame with both surfaces held"
        );
        d.release(a.key);
        let c = d.next_dmabuf().unwrap().unwrap();
        assert_eq!(c.key, a.key);
        assert_eq!(c.pts_us, 200_000);
    }

    #[test]
    fn the_synthetic_hw_decoder_scales_into_a_new_pool() {
        let mut d = SyntheticDecoder::with_dmabuf(64, 36, 10, 25, 3, 0).unwrap();
        let a = d.next_dmabuf().unwrap().unwrap();
        assert_eq!((a.desc.width, a.desc.height), (64, 36));
        d.set_scale(Some((32, 18))).unwrap();
        let b = d.next_dmabuf().unwrap().unwrap();
        assert_eq!((b.desc.width, b.desc.height), (32, 18));
        assert_ne!(a.key >> 24, b.key >> 24, "a new generation");
        d.release(a.key); // the old pool's: ignored
        d.set_scale(None).unwrap();
        assert_eq!(d.scale_state(), Some(((64, 36), 2)));
    }
}
