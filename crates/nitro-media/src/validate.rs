//! Checks on helper replies, which are hostile input.
//!
//! A decode helper parses untrusted media with C code; once compromised it
//! may say anything the codec (`crate::proto`) can represent. Everything
//! here turns a well-formed but wrong reply into a [`Violation`] the app
//! answers by killing the helper, never into a panic, an out-of-range
//! slot, or a dma-buf the server would read past. The model is
//! `nitro-gpu/src/validate.rs`, which checks requests the same way.

use std::collections::HashSet;
use std::fmt;
use std::os::fd::{AsFd, OwnedFd};

use crate::frame::{FrameBuf, Nv12Layout};
use crate::node::{ClockPoint, Micros, VideoFormat};
use crate::proto::{Buffer, MAX_EDGE, MAX_STR, MAX_TRACKS, TrackInfo};

/// Most surfaces a hardware decoder may report in a fixed pool.
pub const MAX_HW_POOL: usize = 64;
/// Largest plausible output delay.
pub const MAX_DELAY_NS: u64 = 10_000_000_000;
/// How far past "now" a clock reading may be stamped (clock skew between
/// the helper's read and ours).
pub const CLOCK_SLACK_NS: u64 = 1_000_000_000;
/// Fastest plausible playback rate.
pub const MAX_RATE: f64 = 8.0;
/// How far past the stream's duration a seek may land.
pub const SEEK_SLACK_US: Micros = 1_000_000;

/// What a reply got wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `Info` with no tracks or more than [`MAX_TRACKS`].
    TrackCount(usize),
    /// A track's field is out of range; names it.
    BadTrack(&'static str),
    /// `Selected` left a wildcard, or changed a field the request fixed.
    Format(&'static str),
    /// A hardware pool over [`MAX_HW_POOL`].
    HwPool(usize),
    /// A shared-memory slot with no ring, or out of the ring's range.
    Slot(usize),
    /// A slot or key already held by the app.
    Outstanding(FrameBuf),
    /// A dma-buf key never described, or described twice.
    Key(u32),
    /// Descriptor count does not match what the reply implies.
    FdCount {
        /// What the reply implies.
        want: usize,
        /// What came.
        got: usize,
    },
    /// A surface's geometry is wrong; says how.
    Surface(&'static str),
    /// A dma-buf is shorter than its planes need (or cannot be sized).
    FdSize {
        /// Bytes needed.
        need: u64,
        /// Bytes the fd has.
        have: u64,
    },
    /// A generation went backwards or ahead of the last `Seek` sent.
    Generation {
        /// The reply's.
        got: u32,
        /// Lowest acceptable.
        min: u32,
        /// Highest acceptable.
        max: u32,
    },
    /// A negative timestamp, or a seek landing out of the stream.
    Time(Micros),
    /// A clock reading out of range; names the field.
    Clock(&'static str),
    /// `Closed` while buffers are still held.
    NotReleased(usize),
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrackCount(n) => write!(f, "{n} tracks (1..={MAX_TRACKS})"),
            Self::BadTrack(w) => write!(f, "bad track: {w}"),
            Self::Format(w) => write!(f, "bad selected format: {w}"),
            Self::HwPool(n) => write!(f, "a {n}-surface pool (at most {MAX_HW_POOL})"),
            Self::Slot(s) => write!(f, "slot {s} is not in the ring"),
            Self::Outstanding(b) => write!(f, "{b:?} is still held"),
            Self::Key(k) => write!(f, "dma-buf key {k:#x} is unknown or described twice"),
            Self::FdCount { want, got } => write!(f, "{got} fds, want {want}"),
            Self::Surface(w) => write!(f, "bad surface: {w}"),
            Self::FdSize { need, have } => {
                write!(f, "dma-buf of {have} bytes, planes need {need}")
            }
            Self::Generation { got, min, max } => {
                write!(f, "generation {got} outside {min}..={max}")
            }
            Self::Time(t) => write!(f, "time {t} µs out of range"),
            Self::Clock(w) => write!(f, "bad clock: {w}"),
            Self::NotReleased(n) => write!(f, "closed with {n} buffers held"),
        }
    }
}

impl std::error::Error for Violation {}

/// The size of a memfd or dma-buf, by seeking to its end.
///
/// # Errors
/// The `lseek` failure.
pub fn fd_len(fd: impl AsFd) -> Result<u64, rustix::io::Errno> {
    rustix::fs::seek(fd, rustix::fs::SeekFrom::End(0))
}

/// Check an `Info` reply.
///
/// # Errors
/// The first [`Violation`] found.
pub fn check_info(tracks: &[TrackInfo]) -> Result<(), Violation> {
    if tracks.is_empty() || tracks.len() > MAX_TRACKS {
        return Err(Violation::TrackCount(tracks.len()));
    }
    let duration_ok = |d: f64| d.is_finite() && d >= 0.0;
    for t in tracks {
        match t {
            TrackInfo::Video(s) => {
                let edge = |v: u32| (1..=MAX_EDGE).contains(&v);
                if !edge(s.width) || !edge(s.height) {
                    return Err(Violation::BadTrack("video size"));
                }
                if !duration_ok(s.duration) {
                    return Err(Violation::BadTrack("duration"));
                }
                if s.codec.len() > MAX_STR {
                    return Err(Violation::BadTrack("codec name"));
                }
            }
            TrackInfo::Audio {
                codec,
                duration,
                rate,
                channels,
            } => {
                if !duration_ok(*duration) {
                    return Err(Violation::BadTrack("duration"));
                }
                if !(1..=768_000).contains(rate) {
                    return Err(Violation::BadTrack("sample rate"));
                }
                if !(1..=64).contains(channels) {
                    return Err(Violation::BadTrack("channels"));
                }
                if codec.len() > MAX_STR {
                    return Err(Violation::BadTrack("codec name"));
                }
            }
        }
    }
    Ok(())
}

/// Check a `Selected` reply against the `requested` format: every
/// wildcard filled, every fixed field kept, a sane size and pool.
///
/// # Errors
/// The first [`Violation`] found.
pub fn check_selected(
    requested: &VideoFormat,
    got: &VideoFormat,
    hw: Option<crate::frame::HwInfo>,
) -> Result<(), Violation> {
    if !got.is_fixed() {
        return Err(Violation::Format("a wildcard is left"));
    }
    if !requested
        .meet(got)
        .as_ref()
        .is_some_and(VideoFormat::is_fixed)
    {
        return Err(Violation::Format("a requested field changed"));
    }
    if let Some((w, h)) = got.size.fixed() {
        let edge = |v: u32| (2..=MAX_EDGE).contains(&v) && v.is_multiple_of(2);
        if !edge(w) || !edge(h) {
            return Err(Violation::Format("size"));
        }
    }
    if let Some(hw) = hw
        && hw.pool > MAX_HW_POOL
    {
        return Err(Violation::HwPool(hw.pool));
    }
    Ok(())
}

/// Check a clock reading: `prev` the last accepted one (same seek
/// generation), `mono_now_ns` our own `CLOCK_MONOTONIC`.
///
/// # Errors
/// The first [`Violation`] found.
pub fn check_clock(
    prev: Option<&ClockPoint>,
    now: &ClockPoint,
    mono_now_ns: u64,
) -> Result<(), Violation> {
    if !now.rate.is_finite() || now.rate < 0.0 || now.rate > MAX_RATE {
        return Err(Violation::Clock("rate"));
    }
    if now.delay_ns > MAX_DELAY_NS {
        return Err(Violation::Clock("delay"));
    }
    if now.mono_ns > mono_now_ns.saturating_add(CLOCK_SLACK_NS) {
        return Err(Violation::Clock("stamped in the future"));
    }
    if now.media_us < 0 {
        return Err(Violation::Clock("negative position"));
    }
    if let Some(p) = prev {
        if now.mono_ns < p.mono_ns {
            return Err(Violation::Clock("monotonic time went backwards"));
        }
        if now.media_us < p.media_us {
            return Err(Violation::Clock("position went backwards"));
        }
    }
    Ok(())
}

/// What the app expects of one video track's replies, updated as they
/// are accepted.
#[derive(Debug, Default)]
pub struct ReplyState {
    /// The software ring the app sent: slots and layout.
    pub pool: Option<(u32, Nv12Layout)>,
    /// The largest surface the helper may export: the stream's size, or
    /// the scale target.
    pub surface_size: (u32, u32),
    /// Dma-buf keys already described (their fds registered).
    known_keys: HashSet<u32>,
    /// The last accepted generation.
    generation: u32,
    /// The last `Seek` generation sent.
    requested_generation: u32,
    /// Slots and keys the app holds.
    outstanding: HashSet<FrameBuf>,
}

impl ReplyState {
    /// Expect surfaces up to `surface_size` (the stream's size).
    #[must_use]
    pub fn new(surface_size: (u32, u32)) -> Self {
        Self {
            surface_size,
            ..Self::default()
        }
    }

    /// A `Seek` with `generation` was sent.
    pub fn seek_sent(&mut self, generation: u32) {
        self.requested_generation = generation;
    }

    /// The app released `buf`.
    pub fn release(&mut self, buf: FrameBuf) {
        self.outstanding.remove(&buf);
    }

    /// A new scaled pool: the helper's keys are fresh, sizes up to `size`.
    pub fn rescaled(&mut self, size: (u32, u32)) {
        self.surface_size = size;
    }

    /// Buffers the app holds.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    fn check_generation(&self, got: u32) -> Result<(), Violation> {
        let (min, max) = (self.generation, self.requested_generation);
        if got < min || got > max {
            return Err(Violation::Generation { got, min, max });
        }
        Ok(())
    }

    /// Check a `Buffer` reply and its fds; on success the buffer is held
    /// until [`ReplyState::release`].
    ///
    /// # Errors
    /// The first [`Violation`] found; the state is unchanged then.
    pub fn check_buffer(&mut self, b: &Buffer, fds: &[OwnedFd]) -> Result<(), Violation> {
        self.check_generation(b.generation)?;
        if b.pts_us < 0 {
            return Err(Violation::Time(b.pts_us));
        }
        if self.outstanding.contains(&b.buf) {
            return Err(Violation::Outstanding(b.buf));
        }
        let want_fds = b.surface.as_ref().map_or(0, |s| s.planes.len());
        if fds.len() != want_fds {
            return Err(Violation::FdCount {
                want: want_fds,
                got: fds.len(),
            });
        }
        match b.buf {
            FrameBuf::Shm(slot) => {
                let Some((slots, _)) = self.pool else {
                    return Err(Violation::Slot(slot));
                };
                if slot >= slots as usize {
                    return Err(Violation::Slot(slot));
                }
                if b.surface.is_some() {
                    return Err(Violation::Surface("a ring slot has no surface"));
                }
            }
            FrameBuf::DmaBuf(key) => match &b.surface {
                None if !self.known_keys.contains(&key) => return Err(Violation::Key(key)),
                None => {}
                Some(_) if self.known_keys.contains(&key) => return Err(Violation::Key(key)),
                Some(s) => self.check_surface(s, fds)?,
            },
        }
        if let (FrameBuf::DmaBuf(key), Some(_)) = (b.buf, &b.surface) {
            self.known_keys.insert(key);
        }
        self.generation = b.generation;
        self.outstanding.insert(b.buf);
        Ok(())
    }

    fn check_surface(
        &self,
        s: &crate::proto::SurfaceDesc,
        fds: &[OwnedFd],
    ) -> Result<(), Violation> {
        if s.planes.len() != 2 {
            return Err(Violation::Surface("NV12 has two planes"));
        }
        let (mw, mh) = self.surface_size;
        if s.width == 0
            || s.height == 0
            || !s.width.is_multiple_of(2)
            || !s.height.is_multiple_of(2)
        {
            return Err(Violation::Surface("size is not even and non-zero"));
        }
        if s.width > mw || s.height > mh {
            return Err(Violation::Surface("larger than the stream or scale target"));
        }
        for (i, ((offset, stride), fd)) in s.planes.iter().zip(fds).enumerate() {
            if *stride < s.width {
                return Err(Violation::Surface("stride shorter than a row"));
            }
            let rows = if i == 0 { s.height } else { s.height / 2 };
            let need = u64::from(*stride)
                .checked_mul(u64::from(rows))
                .and_then(|n| n.checked_add(u64::from(*offset)))
                .ok_or(Violation::Surface("plane size overflows"))?;
            let have = fd_len(fd).map_err(|_| Violation::FdSize { need, have: 0 })?;
            if need > have {
                return Err(Violation::FdSize { need, have });
            }
        }
        Ok(())
    }

    /// Check an `End` reply's generation.
    ///
    /// # Errors
    /// [`Violation::Generation`].
    pub fn check_end(&mut self, generation: u32) -> Result<(), Violation> {
        self.check_generation(generation)?;
        self.generation = generation;
        Ok(())
    }

    /// Check a `Seeked { landed_us, generation }` reply: the generation
    /// of the last `Seek` sent, landing inside the stream (`duration_us`,
    /// 0 when unknown).
    ///
    /// # Errors
    /// The first [`Violation`] found.
    pub fn check_seeked(
        &mut self,
        landed_us: Micros,
        generation: u32,
        duration_us: Micros,
    ) -> Result<(), Violation> {
        if generation != self.requested_generation {
            return Err(Violation::Generation {
                got: generation,
                min: self.requested_generation,
                max: self.requested_generation,
            });
        }
        let past = duration_us > 0 && landed_us > duration_us.saturating_add(SEEK_SLACK_US);
        if landed_us < 0 || past {
            return Err(Violation::Time(landed_us));
        }
        self.generation = generation;
        Ok(())
    }

    /// Check a `Closed` reply: nothing may still be held.
    ///
    /// # Errors
    /// [`Violation::NotReleased`].
    pub fn check_closed(&self) -> Result<(), Violation> {
        match self.outstanding.len() {
            0 => Ok(()),
            n => Err(Violation::NotReleased(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{HwDec, HwInfo, StreamInfo};
    use crate::node::{Choice, Pixel};
    use crate::proto::SurfaceDesc;

    fn memfd(len: u64) -> OwnedFd {
        nitro_shm::create_sealed("nitro-media-validate", len).unwrap()
    }

    fn video(w: u32, h: u32) -> TrackInfo {
        TrackInfo::Video(StreamInfo {
            width: w,
            height: h,
            duration: 10.0,
            matrix: crate::frame::Matrix::Bt709,
            full_range: false,
            codec: "h264".into(),
        })
    }

    fn shm(slot: usize, generation: u32) -> Buffer {
        Buffer {
            track: 0,
            buf: FrameBuf::Shm(slot),
            pts_us: 0,
            generation,
            surface: None,
        }
    }

    fn surface(w: u32, h: u32) -> SurfaceDesc {
        SurfaceDesc {
            width: w,
            height: h,
            modifier: 0,
            planes: vec![(0, w), (w * h, w)],
        }
    }

    fn dma(key: u32, s: Option<SurfaceDesc>) -> Buffer {
        Buffer {
            track: 0,
            buf: FrameBuf::DmaBuf(key),
            pts_us: 1000,
            generation: 0,
            surface: s,
        }
    }

    fn nv12_fds(w: u32, h: u32) -> Vec<OwnedFd> {
        let len = u64::from(w * h * 3 / 2);
        vec![memfd(len), memfd(len)]
    }

    #[test]
    fn info_is_bounded() {
        assert_eq!(check_info(&[]), Err(Violation::TrackCount(0)));
        assert!(check_info(&[video(1920, 1080)]).is_ok());
        assert!(check_info(&[video(0, 1080)]).is_err());
        assert!(check_info(&[video(MAX_EDGE + 1, 2)]).is_err());
        let TrackInfo::Video(mut s) = video(64, 36) else {
            unreachable!()
        };
        s.duration = f64::NAN;
        assert_eq!(
            check_info(&[TrackInfo::Video(s)]),
            Err(Violation::BadTrack("duration"))
        );
        let audio = |rate, channels| TrackInfo::Audio {
            codec: "opus".into(),
            duration: 1.0,
            rate,
            channels,
        };
        assert!(check_info(&[audio(48_000, 2)]).is_ok());
        assert!(check_info(&[audio(0, 2)]).is_err());
        assert!(check_info(&[audio(48_000, 0)]).is_err());
        assert!(check_info(&vec![audio(48_000, 2); MAX_TRACKS + 1]).is_err());
    }

    #[test]
    fn selected_keeps_the_request_and_fills_wildcards() {
        let req = VideoFormat {
            pixel: Choice::Is(Pixel::Nv12),
            size: Choice::Any,
            hw: HwDec::Auto,
        };
        let ok = VideoFormat {
            size: Choice::Is((640, 360)),
            ..req
        };
        assert!(check_selected(&req, &ok, None).is_ok());
        assert!(check_selected(&req, &req, None).is_err(), "wildcard left");
        let fixed = VideoFormat {
            size: Choice::Is((320, 180)),
            ..req
        };
        assert!(check_selected(&fixed, &ok, None).is_err(), "size changed");
        let odd = VideoFormat {
            size: Choice::Is((641, 360)),
            ..req
        };
        assert!(check_selected(&req, &odd, None).is_err());
        let hw = |pool| {
            Some(HwInfo {
                modifier: nitro_wire::types::modifier::INVALID,
                pool,
            })
        };
        assert!(check_selected(&req, &ok, hw(20)).is_ok());
        assert_eq!(
            check_selected(&req, &ok, hw(MAX_HW_POOL + 1)),
            Err(Violation::HwPool(MAX_HW_POOL + 1))
        );
    }

    #[test]
    fn slots_stay_in_the_ring() {
        let mut st = ReplyState::new((64, 36));
        assert_eq!(
            st.check_buffer(&shm(0, 0), &[]),
            Err(Violation::Slot(0)),
            "no ring"
        );
        st.pool = Some((4, Nv12Layout::for_video(64, 36)));
        assert!(st.check_buffer(&shm(3, 0), &[]).is_ok());
        assert_eq!(st.check_buffer(&shm(4, 0), &[]), Err(Violation::Slot(4)));
        assert_eq!(
            st.check_buffer(&shm(3, 0), &[]),
            Err(Violation::Outstanding(FrameBuf::Shm(3)))
        );
        st.release(FrameBuf::Shm(3));
        assert!(st.check_buffer(&shm(3, 0), &[]).is_ok());
        assert_eq!(
            st.check_buffer(&shm(1, 0), &[memfd(8)]),
            Err(Violation::FdCount { want: 0, got: 1 })
        );
        assert_eq!(st.check_closed(), Err(Violation::NotReleased(1)));
        st.release(FrameBuf::Shm(3));
        assert!(st.check_closed().is_ok());
    }

    #[test]
    fn dmabufs_are_described_once_and_sized() {
        let mut st = ReplyState::new((64, 36));
        assert_eq!(st.check_buffer(&dma(1, None), &[]), Err(Violation::Key(1)));
        assert_eq!(
            st.check_buffer(&dma(1, Some(surface(64, 36))), &nv12_fds(64, 36)[..1]),
            Err(Violation::FdCount { want: 2, got: 1 })
        );
        assert!(
            st.check_buffer(&dma(1, Some(surface(64, 36))), &nv12_fds(64, 36))
                .is_ok()
        );
        st.release(FrameBuf::DmaBuf(1));
        assert!(st.check_buffer(&dma(1, None), &[]).is_ok(), "known key");
        st.release(FrameBuf::DmaBuf(1));
        assert_eq!(
            st.check_buffer(&dma(1, Some(surface(64, 36))), &nv12_fds(64, 36)),
            Err(Violation::Key(1)),
            "described twice"
        );
        // Too small an fd for its planes.
        let small = vec![memfd(64 * 36), memfd(64 * 36)];
        assert_eq!(
            st.check_buffer(&dma(2, Some(surface(64, 36))), &small),
            Err(Violation::FdSize {
                need: 64 * 36 + 64 * 18,
                have: 64 * 36
            })
        );
        // Bigger than the stream, odd, short stride, one plane.
        assert!(
            st.check_buffer(&dma(3, Some(surface(128, 72))), &nv12_fds(128, 72))
                .is_err()
        );
        assert!(
            st.check_buffer(&dma(3, Some(surface(63, 36))), &nv12_fds(64, 36))
                .is_err()
        );
        let mut s = surface(64, 36);
        s.planes[1].1 = 32;
        assert_eq!(
            st.check_buffer(&dma(3, Some(s)), &nv12_fds(64, 36)),
            Err(Violation::Surface("stride shorter than a row"))
        );
        let mut s = surface(64, 36);
        s.planes.pop();
        assert!(
            st.check_buffer(&dma(3, Some(s)), &nv12_fds(64, 36)[..1])
                .is_err()
        );
        let mut s = surface(64, 36);
        s.planes[1].0 = u32::MAX;
        assert!(matches!(
            st.check_buffer(&dma(3, Some(s)), &nv12_fds(64, 36)),
            Err(Violation::FdSize { .. })
        ));
        // A rescale allows the new size.
        st.rescaled((32, 18));
        assert!(
            st.check_buffer(&dma(9, Some(surface(32, 18))), &nv12_fds(32, 18))
                .is_ok()
        );
    }

    #[test]
    fn generations_are_monotonic_and_requested() {
        let mut st = ReplyState::new((64, 36));
        st.pool = Some((4, Nv12Layout::for_video(64, 36)));
        assert_eq!(
            st.check_buffer(&shm(0, 1), &[]),
            Err(Violation::Generation {
                got: 1,
                min: 0,
                max: 0
            }),
            "ahead of any Seek"
        );
        st.seek_sent(2);
        assert!(
            st.check_buffer(&shm(0, 0), &[]).is_ok(),
            "old frames may still arrive"
        );
        let bad = (0, 1);
        assert!(st.check_seeked(bad.0, bad.1, 10_000_000).is_err());
        let past = (12_000_000, 2);
        assert_eq!(
            st.check_seeked(past.0, past.1, 10_000_000),
            Err(Violation::Time(12_000_000))
        );
        let ok = (4_000_000, 2);
        assert!(st.check_seeked(ok.0, ok.1, 10_000_000).is_ok());
        assert!(st.check_buffer(&shm(1, 1), &[]).is_err(), "went backwards");
        assert!(st.check_buffer(&shm(1, 2), &[]).is_ok());
        let mut neg = shm(2, 2);
        neg.pts_us = -1;
        assert_eq!(st.check_buffer(&neg, &[]), Err(Violation::Time(-1)));
        assert!(st.check_end(1).is_err());
        assert!(st.check_end(2).is_ok());
    }

    #[test]
    fn clocks_are_sane() {
        let now = 5_000_000_000;
        let c = ClockPoint {
            mono_ns: now,
            media_us: 1_000,
            rate: 1.0,
            delay_ns: 20_000_000,
        };
        assert!(check_clock(None, &c, now).is_ok());
        let paused = ClockPoint { rate: 0.0, ..c };
        assert!(check_clock(Some(&c), &paused, now).is_ok());
        for bad in [
            ClockPoint {
                rate: f64::NAN,
                ..c
            },
            ClockPoint { rate: -1.0, ..c },
            ClockPoint { rate: 9.0, ..c },
            ClockPoint {
                delay_ns: MAX_DELAY_NS + 1,
                ..c
            },
            ClockPoint {
                mono_ns: now + CLOCK_SLACK_NS + 1,
                ..c
            },
            ClockPoint { media_us: -1, ..c },
        ] {
            assert!(check_clock(None, &bad, now).is_err(), "{bad:?}");
        }
        let earlier = ClockPoint {
            mono_ns: now - 1,
            ..c
        };
        assert!(check_clock(Some(&c), &earlier, now).is_err());
        let back = ClockPoint { media_us: 999, ..c };
        assert!(check_clock(Some(&c), &back, now).is_err());
    }
}
