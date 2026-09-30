//! The app ↔ decode-helper protocol: message types and their codec.
//!
//! Frames are `nitro-wire` frames (8-byte header, payload, descriptors on
//! the header's `sendmsg`), the same framing `nitro-gpu/src/proto.rs`
//! uses. The op codes are this protocol's own namespace: `0x01..` app →
//! helper, `0x81..` helper → app; a helper always has a socket of its own.
//!
//! A message is a plain value ([`ToHelper`] / [`FromHelper`]) plus the
//! descriptors that ride with it, passed alongside as a `Vec<OwnedFd>`.
//! Each message states how many descriptors it carries; encode and decode
//! both check the count. Every length is bounded before anything is
//! allocated, and decoding is total: a malformed payload is a
//! [`DecodeError`], never a panic. What a well-formed reply *means* is
//! checked separately, as hostile input, by [`crate::validate`].
//!
//! The table in `docs/media.md` ("Protocol") is the design; phase 1
//! defines the messages, phase 2 speaks them.

use std::os::fd::OwnedFd;

use nitro_wire::{DecodeError, EncodeError, Frame, MAX_FDS, Reader, Writer};

use crate::frame::{FrameBuf, HwDec, HwInfo, Matrix, Nv12Layout, StreamInfo};
use crate::node::{Choice, Micros, Pixel, VideoFormat};

/// Protocol version, carried by [`ToHelper::Warm`] and
/// [`FromHelper::Warmed`]; a mismatch is refused.
pub const PROTO_VERSION: u32 = 1;

/// Most tracks in one file.
pub const MAX_TRACKS: usize = 16;
/// Longest string (codec name, error text) in bytes.
pub const MAX_STR: usize = 1024;
/// Most planes of an exported surface.
pub const MAX_PLANES: usize = 4;
/// Most slots in a software video ring.
pub const MAX_SLOTS: u32 = 16;
/// Most equaliser bands.
pub const MAX_BANDS: usize = 16;
/// Largest video edge, in pixels.
pub const MAX_EDGE: u32 = 16384;

/// Op codes.
mod op {
    pub const WARM: u16 = 0x01;
    pub const OPEN: u16 = 0x02;
    pub const SELECT_VIDEO: u16 = 0x03;
    pub const POOL: u16 = 0x04;
    pub const NEXT: u16 = 0x05;
    pub const RELEASE: u16 = 0x06;
    pub const PLAY_AUDIO: u16 = 0x07;
    pub const PROPS: u16 = 0x08;
    pub const TAP: u16 = 0x09;
    pub const PLAY: u16 = 0x0a;
    pub const PAUSE: u16 = 0x0b;
    pub const SEEK: u16 = 0x0c;
    pub const SET_SCALE: u16 = 0x0d;
    pub const CLOSE: u16 = 0x0e;

    pub const WARMED: u16 = 0x81;
    pub const INFO: u16 = 0x82;
    pub const SELECTED: u16 = 0x83;
    pub const ERROR: u16 = 0x84;
    pub const BUFFER: u16 = 0x85;
    pub const END: u16 = 0x86;
    pub const PLAYING: u16 = 0x87;
    pub const SEEKED: u16 = 0x88;
    pub const SCALED: u16 = 0x89;
    pub const SCALE_ERROR: u16 = 0x8a;
    pub const CLOSED: u16 = 0x8b;
}

/// Where a helper's audio stream should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    /// Wherever the session manager links new streams.
    #[default]
    Default,
    /// A `PipeWire` node, by global id.
    Node(u32),
}

/// One track of an opened file.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackInfo {
    /// A video track.
    Video(StreamInfo),
    /// An audio track.
    Audio {
        /// The decoder's name.
        codec: String,
        /// Length in seconds; 0 when unknown.
        duration: f64,
        /// Frames per second.
        rate: u32,
        /// Channel count.
        channels: u8,
    },
}

/// The fd-less wire form of a [`crate::DmabufDesc`]: an exported NV12
/// surface's layout. Its fds (one per plane, in order) ride with the
/// [`FromHelper::Buffer`] that first names the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceDesc {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// DRM format modifier.
    pub modifier: u64,
    /// `(offset, stride)` per plane: luma, then interleaved chroma.
    pub planes: Vec<(u32, u32)>,
}

/// A decoded frame, in a ring slot or a helper surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    /// The track it belongs to.
    pub track: u32,
    /// Where its pixels are.
    pub buf: FrameBuf,
    /// Presentation time.
    pub pts_us: Micros,
    /// The [`ToHelper::Seek`] generation it was decoded after.
    pub generation: u32,
    /// For the first use of a dma-buf key: the surface to register.
    pub surface: Option<SurfaceDesc>,
}

/// App → helper.
#[derive(Debug, Clone, PartialEq)]
pub enum ToHelper {
    /// Get ready for a file; fds: the render node if `render`, then the
    /// connected `PipeWire` socket if `pipewire`.
    Warm {
        /// [`PROTO_VERSION`].
        version: u32,
        /// A render-node fd rides along (VA-API wanted).
        render: bool,
        /// A `PipeWire` socket fd rides along.
        pipewire: bool,
    },
    /// Open the file whose fd rides along.
    Open,
    /// Decode video track `track`, negotiated from `format`.
    SelectVideo {
        /// Track index.
        track: u32,
        /// What the app takes (wildcards allowed).
        format: VideoFormat,
    },
    /// The software video ring: one sealed memfd of `slots` frames rides
    /// along.
    Pool {
        /// Track index.
        track: u32,
        /// Frames in the ring.
        slots: u32,
        /// Each frame's layout.
        layout: Nv12Layout,
    },
    /// Decode the next frame of `track`.
    Next {
        /// Track index.
        track: u32,
    },
    /// The app is done with a frame's buffer.
    Release {
        /// Track index.
        track: u32,
        /// Slot or key.
        buf: FrameBuf,
    },
    /// Play audio track `track` into `PipeWire`.
    PlayAudio {
        /// Track index.
        track: u32,
        /// Where to.
        target: Target,
        /// Equaliser band gains, dB (empty: off).
        eq: Vec<f32>,
    },
    /// Stream properties.
    Props {
        /// Linear volume.
        volume: f32,
        /// Muted.
        mute: bool,
        /// Equaliser band gains, dB.
        eq: Vec<f32>,
    },
    /// The visualiser's tap ring: a memfd rides along.
    Tap,
    /// Start or resume.
    Play,
    /// Pause.
    Pause,
    /// Continue from the keyframe at or before `t_us`.
    Seek {
        /// Target time.
        t_us: Micros,
        /// Tags every later frame; strictly increasing.
        generation: u32,
    },
    /// Scale later surfaces to `size`, or native (`None`).
    SetScale {
        /// Target size (even).
        size: Option<(u32, u32)>,
    },
    /// Release everything; the helper answers [`FromHelper::Closed`].
    Close,
}

/// Helper → app.
#[derive(Debug, Clone, PartialEq)]
pub enum FromHelper {
    /// Warm, speaking `version`.
    Warmed {
        /// [`PROTO_VERSION`].
        version: u32,
    },
    /// The opened file's tracks.
    Info {
        /// In file order.
        tracks: Vec<TrackInfo>,
    },
    /// The video format a [`ToHelper::SelectVideo`] settled on.
    Selected {
        /// Track index.
        track: u32,
        /// Fixed (no wildcard left).
        format: VideoFormat,
        /// Hardware decode facts; `None` for software.
        hw: Option<HwInfo>,
    },
    /// Request `op` failed (a refusal by name).
    Error {
        /// The op code refused.
        op: u16,
        /// Why, for the user.
        text: String,
    },
    /// A decoded frame; fds: the surface's planes when `surface` is set.
    Buffer(Buffer),
    /// End of the stream.
    End {
        /// Track index.
        track: u32,
        /// The seek generation it ended in.
        generation: u32,
    },
    /// Audio is playing through `PipeWire` node `node`; the clock page
    /// (a memfd) rides along.
    Playing {
        /// `PipeWire` global id of the stream node.
        node: u32,
    },
    /// A seek landed.
    Seeked {
        /// Where, at the keyframe.
        landed_us: Micros,
        /// The [`ToHelper::Seek`]'s generation.
        generation: u32,
    },
    /// Scaling took effect.
    Scaled,
    /// Scaling failed or stopped; frames are native.
    ScaleError {
        /// Why.
        text: String,
    },
    /// Everything released; the helper may be reused.
    Closed,
}

/// A message of this protocol: encode with its fds, decode from a frame.
pub trait Message: Sized {
    /// Op code of this message.
    fn op(&self) -> u16;

    /// How many fds this message carries.
    fn fd_count(&self) -> usize;

    /// Append one frame to `w`.
    ///
    /// # Errors
    /// [`EncodeError::TooManyFds`] if `fds.len()` is not
    /// [`Message::fd_count`] or exceeds [`MAX_FDS`];
    /// [`EncodeError::TooLarge`] if a list or string exceeds its maximum.
    /// Nothing is written on error.
    fn encode(&self, w: &mut Writer, fds: Vec<OwnedFd>) -> Result<(), EncodeError> {
        if fds.len() != self.fd_count() || fds.len() > MAX_FDS {
            return Err(EncodeError::TooManyFds);
        }
        self.check_limits()?;
        w.frame(self.op(), |w| {
            self.encode_body(w);
            for fd in fds {
                w.put_fd(fd);
            }
            Ok(())
        })
    }

    /// Decode one frame.
    ///
    /// # Errors
    /// Any [`DecodeError`]; [`DecodeError::MissingFd`] /
    /// [`DecodeError::UnexpectedFd`] when the fd count is wrong.
    fn decode(frame: Frame) -> Result<(Self, Vec<OwnedFd>), DecodeError> {
        let mut r = Reader::new(&frame.payload);
        let msg = Self::decode_body(frame.op, &mut r)?;
        r.finish()?;
        match frame.fds.len().cmp(&msg.fd_count()) {
            std::cmp::Ordering::Less => Err(DecodeError::MissingFd),
            std::cmp::Ordering::Greater => Err(DecodeError::UnexpectedFd),
            std::cmp::Ordering::Equal => Ok((msg, frame.fds)),
        }
    }

    /// Refuse lists and strings over their protocol maximum.
    ///
    /// # Errors
    /// [`EncodeError::TooLarge`].
    fn check_limits(&self) -> Result<(), EncodeError>;

    /// Write the payload.
    fn encode_body(&self, w: &mut Writer);

    /// Read the payload of op `op`.
    ///
    /// # Errors
    /// Any [`DecodeError`].
    fn decode_body(op: u16, r: &mut Reader<'_>) -> Result<Self, DecodeError>;
}

// ---------------------------------------------------------------------------
// Field helpers

fn too_large(ok: bool) -> Result<(), EncodeError> {
    if ok {
        Ok(())
    } else {
        Err(EncodeError::TooLarge)
    }
}

/// Read a `u32` count, refusing more than `max` items or more items (of at
/// least `item` bytes each) than the payload has room for.
fn get_count(r: &mut Reader<'_>, max: usize, item: usize) -> Result<usize, DecodeError> {
    let n = r.get_u32()? as usize;
    if n > max {
        return Err(DecodeError::TooLarge);
    }
    if n.saturating_mul(item) > r.remaining() {
        return Err(DecodeError::Truncated);
    }
    Ok(n)
}

fn get_str(r: &mut Reader<'_>) -> Result<String, DecodeError> {
    let s = r.get_str()?;
    if s.len() > MAX_STR {
        return Err(DecodeError::TooLarge);
    }
    Ok(s)
}

fn put_bool(w: &mut Writer, b: bool) {
    w.put_u8(u8::from(b));
}

fn get_bool(r: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match r.get_u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::BadValue),
    }
}

fn put_i64(w: &mut Writer, v: i64) {
    w.put_u64(u64::from_le_bytes(v.to_le_bytes()));
}

fn get_i64(r: &mut Reader<'_>) -> Result<i64, DecodeError> {
    Ok(i64::from_le_bytes(r.get_u64()?.to_le_bytes()))
}

fn put_f64(w: &mut Writer, v: f64) {
    w.put_u64(v.to_bits());
}

fn get_f64(r: &mut Reader<'_>) -> Result<f64, DecodeError> {
    Ok(f64::from_bits(r.get_u64()?))
}

fn put_bands(w: &mut Writer, eq: &[f32]) {
    w.put_u32(eq.len() as u32);
    for v in eq {
        w.put_f32(*v);
    }
}

fn get_bands(r: &mut Reader<'_>) -> Result<Vec<f32>, DecodeError> {
    let n = get_count(r, MAX_BANDS, 4)?;
    (0..n).map(|_| r.get_f32()).collect()
}

/// An enum from its wire index into `all`.
fn pick<T: Copy>(all: &[T], v: u8) -> Result<T, DecodeError> {
    all.get(usize::from(v))
        .copied()
        .ok_or(DecodeError::BadValue)
}

fn put_size(w: &mut Writer, size: Option<(u32, u32)>) {
    let (sw, sh) = size.unwrap_or((0, 0));
    w.put_u32(sw);
    w.put_u32(sh);
}

/// A size where `0 × 0` means none; a half-zero size is malformed.
fn get_size(r: &mut Reader<'_>) -> Result<Option<(u32, u32)>, DecodeError> {
    match (r.get_u32()?, r.get_u32()?) {
        (0, 0) => Ok(None),
        (0, _) | (_, 0) => Err(DecodeError::BadValue),
        s => Ok(Some(s)),
    }
}

const HWDECS: [HwDec; 4] = [HwDec::Auto, HwDec::DmaBuf, HwDec::Download, HwDec::Off];
const MATRICES: [Matrix; 3] = [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020];

fn index_of<T: PartialEq>(all: &[T], v: &T) -> u8 {
    all.iter().position(|x| x == v).unwrap_or(0) as u8
}

fn put_format(w: &mut Writer, f: &VideoFormat) {
    w.put_u8(match f.pixel {
        Choice::Any => 0,
        Choice::Is(Pixel::Nv12) => 1,
    });
    put_size(w, f.size.fixed());
    w.put_u8(index_of(&HWDECS, &f.hw));
}

fn get_format(r: &mut Reader<'_>) -> Result<VideoFormat, DecodeError> {
    let pixel = pick(&[Choice::Any, Choice::Is(Pixel::Nv12)], r.get_u8()?)?;
    let size = get_size(r)?.map_or(Choice::Any, Choice::Is);
    let hw = pick(&HWDECS, r.get_u8()?)?;
    Ok(VideoFormat { pixel, size, hw })
}

fn put_buf(w: &mut Writer, b: FrameBuf) {
    match b {
        FrameBuf::Shm(slot) => {
            w.put_u8(0);
            w.put_u32(slot as u32);
        }
        FrameBuf::DmaBuf(key) => {
            w.put_u8(1);
            w.put_u32(key);
        }
    }
}

fn get_buf(r: &mut Reader<'_>) -> Result<FrameBuf, DecodeError> {
    match r.get_u8()? {
        0 => Ok(FrameBuf::Shm(r.get_u32()? as usize)),
        1 => Ok(FrameBuf::DmaBuf(r.get_u32()?)),
        _ => Err(DecodeError::BadValue),
    }
}

fn put_track(w: &mut Writer, t: &TrackInfo) {
    match t {
        TrackInfo::Video(s) => {
            w.put_u8(0);
            w.put_u32(s.width);
            w.put_u32(s.height);
            put_f64(w, s.duration);
            w.put_u8(index_of(&MATRICES, &s.matrix));
            put_bool(w, s.full_range);
            w.put_str(&s.codec);
        }
        TrackInfo::Audio {
            codec,
            duration,
            rate,
            channels,
        } => {
            w.put_u8(1);
            w.put_str(codec);
            put_f64(w, *duration);
            w.put_u32(*rate);
            w.put_u8(*channels);
        }
    }
}

fn get_track(r: &mut Reader<'_>) -> Result<TrackInfo, DecodeError> {
    match r.get_u8()? {
        0 => Ok(TrackInfo::Video(StreamInfo {
            width: r.get_u32()?,
            height: r.get_u32()?,
            duration: get_f64(r)?,
            matrix: pick(&MATRICES, r.get_u8()?)?,
            full_range: get_bool(r)?,
            codec: get_str(r)?,
        })),
        1 => Ok(TrackInfo::Audio {
            codec: get_str(r)?,
            duration: get_f64(r)?,
            rate: r.get_u32()?,
            channels: r.get_u8()?,
        }),
        _ => Err(DecodeError::BadValue),
    }
}

fn track_codec(t: &TrackInfo) -> &str {
    match t {
        TrackInfo::Video(s) => &s.codec,
        TrackInfo::Audio { codec, .. } => codec,
    }
}

impl Message for ToHelper {
    fn op(&self) -> u16 {
        match self {
            Self::Warm { .. } => op::WARM,
            Self::Open => op::OPEN,
            Self::SelectVideo { .. } => op::SELECT_VIDEO,
            Self::Pool { .. } => op::POOL,
            Self::Next { .. } => op::NEXT,
            Self::Release { .. } => op::RELEASE,
            Self::PlayAudio { .. } => op::PLAY_AUDIO,
            Self::Props { .. } => op::PROPS,
            Self::Tap => op::TAP,
            Self::Play => op::PLAY,
            Self::Pause => op::PAUSE,
            Self::Seek { .. } => op::SEEK,
            Self::SetScale { .. } => op::SET_SCALE,
            Self::Close => op::CLOSE,
        }
    }

    fn fd_count(&self) -> usize {
        match self {
            Self::Warm {
                render, pipewire, ..
            } => usize::from(*render) + usize::from(*pipewire),
            Self::Open | Self::Pool { .. } | Self::Tap => 1,
            _ => 0,
        }
    }

    fn check_limits(&self) -> Result<(), EncodeError> {
        match self {
            Self::PlayAudio { eq, .. } | Self::Props { eq, .. } => too_large(eq.len() <= MAX_BANDS),
            Self::Pool { slots, .. } => too_large(*slots <= MAX_SLOTS),
            _ => Ok(()),
        }
    }

    fn encode_body(&self, w: &mut Writer) {
        match self {
            Self::Warm {
                version,
                render,
                pipewire,
            } => {
                w.put_u32(*version);
                put_bool(w, *render);
                put_bool(w, *pipewire);
            }
            Self::Open | Self::Tap | Self::Play | Self::Pause | Self::Close => {}
            Self::SelectVideo { track, format } => {
                w.put_u32(*track);
                put_format(w, format);
            }
            Self::Pool {
                track,
                slots,
                layout,
            } => {
                w.put_u32(*track);
                w.put_u32(*slots);
                w.put_u32(layout.width);
                w.put_u32(layout.height);
            }
            Self::Next { track } => w.put_u32(*track),
            Self::Release { track, buf } => {
                w.put_u32(*track);
                put_buf(w, *buf);
            }
            Self::PlayAudio { track, target, eq } => {
                w.put_u32(*track);
                match target {
                    Target::Default => {
                        w.put_u8(0);
                        w.put_u32(0);
                    }
                    Target::Node(id) => {
                        w.put_u8(1);
                        w.put_u32(*id);
                    }
                }
                put_bands(w, eq);
            }
            Self::Props { volume, mute, eq } => {
                w.put_f32(*volume);
                put_bool(w, *mute);
                put_bands(w, eq);
            }
            Self::Seek { t_us, generation } => {
                put_i64(w, *t_us);
                w.put_u32(*generation);
            }
            Self::SetScale { size } => put_size(w, *size),
        }
    }

    fn decode_body(op: u16, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match op {
            op::WARM => Self::Warm {
                version: r.get_u32()?,
                render: get_bool(r)?,
                pipewire: get_bool(r)?,
            },
            op::OPEN => Self::Open,
            op::SELECT_VIDEO => Self::SelectVideo {
                track: r.get_u32()?,
                format: get_format(r)?,
            },
            op::POOL => {
                let track = r.get_u32()?;
                let slots = r.get_u32()?;
                if slots > MAX_SLOTS {
                    return Err(DecodeError::TooLarge);
                }
                let (width, height) = (r.get_u32()?, r.get_u32()?);
                Self::Pool {
                    track,
                    slots,
                    layout: Nv12Layout { width, height },
                }
            }
            op::NEXT => Self::Next {
                track: r.get_u32()?,
            },
            op::RELEASE => Self::Release {
                track: r.get_u32()?,
                buf: get_buf(r)?,
            },
            op::PLAY_AUDIO => {
                let track = r.get_u32()?;
                let target = match (r.get_u8()?, r.get_u32()?) {
                    (0, _) => Target::Default,
                    (1, id) => Target::Node(id),
                    _ => return Err(DecodeError::BadValue),
                };
                Self::PlayAudio {
                    track,
                    target,
                    eq: get_bands(r)?,
                }
            }
            op::PROPS => Self::Props {
                volume: r.get_f32()?,
                mute: get_bool(r)?,
                eq: get_bands(r)?,
            },
            op::TAP => Self::Tap,
            op::PLAY => Self::Play,
            op::PAUSE => Self::Pause,
            op::SEEK => Self::Seek {
                t_us: get_i64(r)?,
                generation: r.get_u32()?,
            },
            op::SET_SCALE => Self::SetScale { size: get_size(r)? },
            op::CLOSE => Self::Close,
            other => return Err(DecodeError::UnknownOp(other)),
        })
    }
}

impl Message for FromHelper {
    fn op(&self) -> u16 {
        match self {
            Self::Warmed { .. } => op::WARMED,
            Self::Info { .. } => op::INFO,
            Self::Selected { .. } => op::SELECTED,
            Self::Error { .. } => op::ERROR,
            Self::Buffer(_) => op::BUFFER,
            Self::End { .. } => op::END,
            Self::Playing { .. } => op::PLAYING,
            Self::Seeked { .. } => op::SEEKED,
            Self::Scaled => op::SCALED,
            Self::ScaleError { .. } => op::SCALE_ERROR,
            Self::Closed => op::CLOSED,
        }
    }

    fn fd_count(&self) -> usize {
        match self {
            Self::Buffer(b) => b.surface.as_ref().map_or(0, |s| s.planes.len()),
            Self::Playing { .. } => 1,
            _ => 0,
        }
    }

    fn check_limits(&self) -> Result<(), EncodeError> {
        match self {
            Self::Info { tracks } => too_large(
                tracks.len() <= MAX_TRACKS
                    && tracks.iter().all(|t| track_codec(t).len() <= MAX_STR),
            ),
            Self::Error { text, .. } | Self::ScaleError { text } => {
                too_large(text.len() <= MAX_STR)
            }
            Self::Buffer(b) => too_large(
                b.surface
                    .as_ref()
                    .is_none_or(|s| s.planes.len() <= MAX_PLANES),
            ),
            _ => Ok(()),
        }
    }

    fn encode_body(&self, w: &mut Writer) {
        match self {
            Self::Warmed { version } => w.put_u32(*version),
            Self::Info { tracks } => {
                w.put_u32(tracks.len() as u32);
                for t in tracks {
                    put_track(w, t);
                }
            }
            Self::Selected { track, format, hw } => {
                w.put_u32(*track);
                put_format(w, format);
                put_bool(w, hw.is_some());
                let hw = hw.unwrap_or(HwInfo {
                    modifier: 0,
                    pool: 0,
                });
                w.put_u64(hw.modifier);
                w.put_u32(hw.pool as u32);
            }
            Self::Error { op, text } => {
                w.put_u16(*op);
                w.put_str(text);
            }
            Self::Buffer(b) => {
                w.put_u32(b.track);
                put_buf(w, b.buf);
                put_i64(w, b.pts_us);
                w.put_u32(b.generation);
                put_bool(w, b.surface.is_some());
                if let Some(s) = &b.surface {
                    w.put_u32(s.width);
                    w.put_u32(s.height);
                    w.put_u64(s.modifier);
                    w.put_u32(s.planes.len() as u32);
                    for (offset, stride) in &s.planes {
                        w.put_u32(*offset);
                        w.put_u32(*stride);
                    }
                }
            }
            Self::End { track, generation } => {
                w.put_u32(*track);
                w.put_u32(*generation);
            }
            Self::Playing { node } => w.put_u32(*node),
            Self::Seeked {
                landed_us,
                generation,
            } => {
                put_i64(w, *landed_us);
                w.put_u32(*generation);
            }
            Self::ScaleError { text } => w.put_str(text),
            Self::Scaled | Self::Closed => {}
        }
    }

    fn decode_body(op: u16, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match op {
            op::WARMED => Self::Warmed {
                version: r.get_u32()?,
            },
            op::INFO => {
                // The smallest track (audio, empty codec) is 18 bytes.
                let n = get_count(r, MAX_TRACKS, 18)?;
                Self::Info {
                    tracks: (0..n).map(|_| get_track(r)).collect::<Result<_, _>>()?,
                }
            }
            op::SELECTED => {
                let track = r.get_u32()?;
                let format = get_format(r)?;
                let some = get_bool(r)?;
                let modifier = r.get_u64()?;
                let pool = r.get_u32()? as usize;
                Self::Selected {
                    track,
                    format,
                    hw: some.then_some(HwInfo { modifier, pool }),
                }
            }
            op::ERROR => Self::Error {
                op: r.get_u16()?,
                text: get_str(r)?,
            },
            op::BUFFER => {
                let track = r.get_u32()?;
                let buf = get_buf(r)?;
                let pts_us = get_i64(r)?;
                let generation = r.get_u32()?;
                let surface = if get_bool(r)? {
                    let width = r.get_u32()?;
                    let height = r.get_u32()?;
                    let modifier = r.get_u64()?;
                    let n = get_count(r, MAX_PLANES, 8)?;
                    let planes = (0..n)
                        .map(|_| Ok((r.get_u32()?, r.get_u32()?)))
                        .collect::<Result<_, DecodeError>>()?;
                    Some(SurfaceDesc {
                        width,
                        height,
                        modifier,
                        planes,
                    })
                } else {
                    None
                };
                Self::Buffer(Buffer {
                    track,
                    buf,
                    pts_us,
                    generation,
                    surface,
                })
            }
            op::END => Self::End {
                track: r.get_u32()?,
                generation: r.get_u32()?,
            },
            op::PLAYING => Self::Playing { node: r.get_u32()? },
            op::SEEKED => Self::Seeked {
                landed_us: get_i64(r)?,
                generation: r.get_u32()?,
            },
            op::SCALED => Self::Scaled,
            op::SCALE_ERROR => Self::ScaleError { text: get_str(r)? },
            op::CLOSED => Self::Closed,
            other => return Err(DecodeError::UnknownOp(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_wire::Framer;

    fn memfd() -> OwnedFd {
        nitro_shm::create_sealed("nitro-media-test", 64).unwrap()
    }

    fn fds(n: usize) -> Vec<OwnedFd> {
        (0..n).map(|_| memfd()).collect()
    }

    /// Encode, frame, decode.
    fn round<M: Message>(m: &M) -> Result<(M, Vec<OwnedFd>), DecodeError> {
        let mut w = Writer::new();
        m.encode(&mut w, fds(m.fd_count())).unwrap();
        let (bytes, f) = w.take();
        let mut framer = Framer::new();
        framer.feed(&bytes, f);
        M::decode(framer.next_frame()?.expect("a frame"))
    }

    fn frame(op: u16, payload: Vec<u8>, nfds: usize) -> Frame {
        Frame {
            op,
            payload,
            fds: fds(nfds),
        }
    }

    fn surface() -> SurfaceDesc {
        SurfaceDesc {
            width: 64,
            height: 36,
            modifier: 0,
            planes: vec![(0, 64), (64 * 36, 64)],
        }
    }

    fn video() -> VideoFormat {
        VideoFormat {
            pixel: Choice::Is(Pixel::Nv12),
            size: Choice::Any,
            hw: HwDec::DmaBuf,
        }
    }

    #[test]
    fn every_request_round_trips() {
        let all = [
            ToHelper::Warm {
                version: PROTO_VERSION,
                render: true,
                pipewire: true,
            },
            ToHelper::Warm {
                version: PROTO_VERSION,
                render: false,
                pipewire: true,
            },
            ToHelper::Open,
            ToHelper::SelectVideo {
                track: 1,
                format: video(),
            },
            ToHelper::Pool {
                track: 0,
                slots: 4,
                layout: Nv12Layout::for_video(640, 360),
            },
            ToHelper::Next { track: 0 },
            ToHelper::Release {
                track: 0,
                buf: FrameBuf::Shm(3),
            },
            ToHelper::Release {
                track: 0,
                buf: FrameBuf::DmaBuf(0x0100_0002),
            },
            ToHelper::PlayAudio {
                track: 1,
                target: Target::Node(42),
                eq: vec![1.5, -3.0],
            },
            ToHelper::PlayAudio {
                track: 1,
                target: Target::Default,
                eq: vec![],
            },
            ToHelper::Props {
                volume: 0.5,
                mute: true,
                eq: vec![0.0; MAX_BANDS],
            },
            ToHelper::Tap,
            ToHelper::Play,
            ToHelper::Pause,
            ToHelper::Seek {
                t_us: -5,
                generation: 7,
            },
            ToHelper::SetScale {
                size: Some((1600, 900)),
            },
            ToHelper::SetScale { size: None },
            ToHelper::Close,
        ];
        for m in all {
            let (got, f) = round(&m).unwrap();
            assert_eq!(got, m);
            assert_eq!(f.len(), m.fd_count());
        }
    }

    #[test]
    fn every_reply_round_trips() {
        let all = [
            FromHelper::Warmed {
                version: PROTO_VERSION,
            },
            FromHelper::Info {
                tracks: vec![
                    TrackInfo::Video(StreamInfo {
                        width: 1920,
                        height: 1080,
                        duration: 12.5,
                        matrix: Matrix::Bt2020,
                        full_range: true,
                        codec: "h264".into(),
                    }),
                    TrackInfo::Audio {
                        codec: "opus".into(),
                        duration: 12.4,
                        rate: 48_000,
                        channels: 2,
                    },
                ],
            },
            FromHelper::Selected {
                track: 0,
                format: VideoFormat {
                    size: Choice::Is((1920, 1080)),
                    ..video()
                },
                hw: Some(HwInfo {
                    modifier: 7,
                    pool: 20,
                }),
            },
            FromHelper::Selected {
                track: 0,
                format: video(),
                hw: None,
            },
            FromHelper::Error {
                op: op::OPEN,
                text: "no such codec".into(),
            },
            FromHelper::Buffer(Buffer {
                track: 0,
                buf: FrameBuf::DmaBuf(9),
                pts_us: 40_000,
                generation: 2,
                surface: Some(surface()),
            }),
            FromHelper::Buffer(Buffer {
                track: 0,
                buf: FrameBuf::Shm(1),
                pts_us: 0,
                generation: 0,
                surface: None,
            }),
            FromHelper::End {
                track: 0,
                generation: 3,
            },
            FromHelper::Playing { node: 77 },
            FromHelper::Seeked {
                landed_us: 1_000_000,
                generation: 4,
            },
            FromHelper::Scaled,
            FromHelper::ScaleError {
                text: "VPP refused".into(),
            },
            FromHelper::Closed,
        ];
        for m in all {
            let (got, f) = round(&m).unwrap();
            assert_eq!(got, m);
            assert_eq!(f.len(), m.fd_count());
        }
    }

    #[test]
    fn fd_counts_are_enforced_both_ways() {
        let mut w = Writer::new();
        assert_eq!(
            ToHelper::Open.encode(&mut w, vec![]),
            Err(EncodeError::TooManyFds)
        );
        assert_eq!(
            ToHelper::Play.encode(&mut w, fds(1)),
            Err(EncodeError::TooManyFds)
        );
        assert!(w.take().0.is_empty(), "nothing written on error");
        assert_eq!(
            ToHelper::decode(frame(op::OPEN, vec![], 0)).err(),
            Some(DecodeError::MissingFd)
        );
        assert_eq!(
            ToHelper::decode(frame(op::PLAY, vec![], 1)).err(),
            Some(DecodeError::UnexpectedFd)
        );
        // A surface of two planes needs exactly two fds.
        let mut w = Writer::new();
        let b = FromHelper::Buffer(Buffer {
            track: 0,
            buf: FrameBuf::DmaBuf(1),
            pts_us: 0,
            generation: 0,
            surface: Some(surface()),
        });
        b.encode(&mut w, fds(2)).unwrap();
        let (bytes, _) = w.take();
        let payload = bytes[nitro_wire::header::SIZE..].to_vec();
        assert_eq!(
            FromHelper::decode(frame(op::BUFFER, payload.clone(), 1)).err(),
            Some(DecodeError::MissingFd)
        );
        assert_eq!(
            FromHelper::decode(frame(op::BUFFER, payload, 3)).err(),
            Some(DecodeError::UnexpectedFd)
        );
    }

    #[test]
    fn oversize_lists_are_refused() {
        let mut w = Writer::new();
        let props = ToHelper::Props {
            volume: 1.0,
            mute: false,
            eq: vec![0.0; MAX_BANDS + 1],
        };
        assert_eq!(props.encode(&mut w, vec![]), Err(EncodeError::TooLarge));
        let err = FromHelper::Error {
            op: 1,
            text: "x".repeat(MAX_STR + 1),
        };
        assert_eq!(err.encode(&mut w, vec![]), Err(EncodeError::TooLarge));
        // A count over the maximum, on the decode side.
        let mut p = Vec::new();
        p.extend_from_slice(&(MAX_TRACKS as u32 + 1).to_le_bytes());
        assert_eq!(
            FromHelper::decode(frame(op::INFO, p, 0)).err(),
            Some(DecodeError::TooLarge)
        );
        // A count the payload cannot hold.
        let p = 3u32.to_le_bytes().to_vec();
        assert_eq!(
            FromHelper::decode(frame(op::INFO, p, 0)).err(),
            Some(DecodeError::Truncated)
        );
        let mut p = 0u32.to_le_bytes().to_vec();
        p.extend_from_slice(&(MAX_SLOTS + 1).to_le_bytes());
        p.extend_from_slice(&[0; 8]);
        assert_eq!(
            ToHelper::decode(frame(op::POOL, p, 1)).err(),
            Some(DecodeError::TooLarge)
        );
    }

    #[test]
    fn malformed_payloads_are_errors() {
        assert_eq!(
            FromHelper::decode(frame(op::WARMED, vec![1, 0], 0)).err(),
            Some(DecodeError::Truncated)
        );
        assert_eq!(
            FromHelper::decode(frame(op::WARMED, vec![1, 0, 0, 0, 9], 0)).err(),
            Some(DecodeError::Trailing)
        );
        assert_eq!(
            FromHelper::decode(frame(0x99, vec![], 0)).err(),
            Some(DecodeError::UnknownOp(0x99))
        );
        assert_eq!(
            ToHelper::decode(frame(op::WARMED, vec![1, 0, 0, 0], 0)).err(),
            Some(DecodeError::UnknownOp(op::WARMED)),
            "a reply op is not a request"
        );
        // A bool other than 0/1.
        assert_eq!(
            ToHelper::decode(frame(op::WARM, vec![1, 0, 0, 0, 2, 0], 0)).err(),
            Some(DecodeError::BadValue)
        );
        // A half-zero size.
        let mut p = 0u32.to_le_bytes().to_vec();
        p.extend_from_slice(&5u32.to_le_bytes());
        assert_eq!(
            ToHelper::decode(frame(op::SET_SCALE, p, 0)).err(),
            Some(DecodeError::BadValue)
        );
        // An unknown buffer tag.
        let mut p = 0u32.to_le_bytes().to_vec();
        p.push(2);
        p.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            ToHelper::decode(frame(op::RELEASE, p, 0)).err(),
            Some(DecodeError::BadValue)
        );
        // Every truncation of a valid Buffer reply fails cleanly.
        let mut w = Writer::new();
        let b = FromHelper::Buffer(Buffer {
            track: 0,
            buf: FrameBuf::DmaBuf(1),
            pts_us: 5,
            generation: 1,
            surface: Some(surface()),
        });
        b.encode(&mut w, fds(2)).unwrap();
        let (bytes, _) = w.take();
        let payload = &bytes[nitro_wire::header::SIZE..];
        for n in 0..payload.len() {
            assert!(FromHelper::decode(frame(op::BUFFER, payload[..n].to_vec(), 2)).is_err());
        }
    }
}
