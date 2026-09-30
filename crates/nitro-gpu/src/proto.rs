//! The server ↔ helper protocol: message types and their codec.
//!
//! Frames are `nitro-wire` frames (8-byte header, payload, descriptors on
//! the header's `sendmsg`), so the framing, fd passing and hostile-input
//! handling are the ones the client protocol already tests. The op codes
//! are this protocol's own namespace: `0x01..` server → helper,
//! `0x81..` helper → server. Helper traffic always has a socket of its
//! own, so the two namespaces never meet on one stream.
//!
//! A message is a plain value ([`ToHelper`] / [`FromHelper`], `Clone +
//! PartialEq`) plus the descriptors that ride with it, passed alongside as
//! a `Vec<OwnedFd>`. Each message states how many descriptors it carries;
//! encode and decode both check the count, so a sender cannot put a frame
//! on the wire that the receiver would reject for its fds, and at most
//! [`MAX_FDS`] ride on one frame.
//!
//! Every length is bounded before anything is allocated. Decoding is
//! total: a malformed payload is a [`DecodeError`], never a panic.

use std::os::fd::OwnedFd;

use nitro_core::IRect;
use nitro_wire::{DecodeError, EncodeError, Frame, MAX_FDS, Reader, Writer};

/// Protocol version; [`ToHelper::Hello`] and [`FromHelper::HelloReply`]
/// carry it and a mismatch is refused.
pub const PROTO_VERSION: u32 = 3;

/// Most layers in one [`Composite`] or [`ToHelper::Capture`].
pub const MAX_LAYERS: usize = 16;
/// Most damage rects in one message.
pub const MAX_RECTS: usize = 64;
/// Most slots in an output ring.
pub const MAX_RING: usize = 4;
/// Fewest slots in a capture ring (v3): one with the consumer, one being
/// drawn, one spare, so a slow consumer drops frames instead of stalling.
pub const MIN_CAPTURE_RING: usize = 3;
/// Most capture rings alive at once (v3). Bounds what a server bug can
/// pin in GPU memory; one per recorded output is the expected use.
pub const MAX_CAPTURE_RINGS: usize = 4;
/// Most planes of an imported dma-buf.
pub const MAX_PLANES: usize = 4;
/// Most modifiers in a ring request.
pub const MAX_MODIFIERS: usize = 32;
/// Most format/modifier pairs in a [`DeviceInfo`] list.
pub const MAX_FORMATS: usize = 512;
/// Longest string (device name, error text) in bytes.
pub const MAX_STR: usize = 1024;
/// Largest texture or output edge, in pixels.
pub const MAX_EDGE: u32 = 16384;

/// A DRM fourcc from its four characters.
#[must_use]
pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// `DRM_FORMAT_XRGB8888`: B, G, R, X bytes in memory.
pub const XR24: u32 = fourcc(b'X', b'R', b'2', b'4');
/// `DRM_FORMAT_ARGB8888`: B, G, R, A bytes in memory, **premultiplied**.
pub const AR24: u32 = fourcc(b'A', b'R', b'2', b'4');
/// `DRM_FORMAT_NV12`: Y plane, then interleaved `CbCr` at half resolution.
pub const NV12: u32 = fourcc(b'N', b'V', b'1', b'2');

/// `DRM_FORMAT_MOD_LINEAR`.
pub const MOD_LINEAR: u64 = 0;
/// `I915_FORMAT_MOD_X_TILED`.
pub const MOD_I915_X_TILED: u64 = (1 << 56) | 1;
/// `I915_FORMAT_MOD_Y_TILED`.
pub const MOD_I915_Y_TILED: u64 = (1 << 56) | 2;
/// `DRM_FORMAT_MOD_INVALID`: "implicit modifier", never accepted here.
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Number of memory planes a supported fourcc has, or `None` if the
/// helper does not know the format.
#[must_use]
pub fn plane_count(fourcc: u32) -> Option<usize> {
    match fourcc {
        XR24 | AR24 => Some(1),
        NV12 => Some(2),
        _ => None,
    }
}

/// YCbCr → RGB matrix of a YUV texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorEncoding {
    /// ITU-R BT.601.
    Bt601,
    /// ITU-R BT.709 (the HD default).
    #[default]
    Bt709,
    /// ITU-R BT.2020.
    Bt2020,
}

/// Quantisation range of a YUV texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorRange {
    /// Limited ("narrow", "TV"): Y 16–235, C 16–240.
    #[default]
    Limited,
    /// Full ("PC"): 0–255.
    Full,
}

/// How a layer combines with what is under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Blend {
    /// Replace: the layer's pixels are written as they are (alpha forced
    /// to 1 for XR24 and YUV).
    #[default]
    Opaque,
    /// Premultiplied source-over: `dst = src + (1 - src.a) * dst`.
    PremulOver,
}

/// Which way the helper gets the shadow buffer to the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ShadowPath {
    /// No shadow imported yet / not reported.
    #[default]
    Unknown,
    /// memfd → `/dev/udmabuf` → dma-buf import: zero copy.
    Udmabuf,
    /// Damage rects copied through a staging buffer each frame.
    Staging,
}

/// One plane of an imported dma-buf. The fd rides with the message, one
/// per plane, in plane order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaneDesc {
    /// Byte offset of the plane in its fd.
    pub offset: u32,
    /// Row pitch in bytes.
    pub pitch: u32,
}

/// A client dma-buf to import as a texture.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DmabufDesc {
    /// Texture id, chosen by the server.
    pub id: u32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// DRM fourcc.
    pub fourcc: u32,
    /// Explicit DRM format modifier.
    pub modifier: u64,
    /// Planes; one fd each.
    pub planes: Vec<PlaneDesc>,
    /// YUV matrix (ignored for RGB).
    pub encoding: ColorEncoding,
    /// YUV range (ignored for RGB).
    pub range: ColorRange,
}

/// The shadow buffer (a sealed memfd) to import as a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShadowDesc {
    /// Texture id, chosen by the server.
    pub id: u32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// [`XR24`] or [`AR24`] (premultiplied).
    pub fourcc: u32,
}

/// A format + modifier pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FormatMod {
    /// DRM fourcc.
    pub fourcc: u32,
    /// DRM format modifier.
    pub modifier: u64,
}

/// One layer of a [`Composite`], bottom first.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Layer {
    /// Texture id.
    pub tex: u32,
    /// Source rect in texels: x, y, w, h (fractional allowed).
    pub src: [f32; 4],
    /// Destination rect in output pixels.
    pub dst: IRect,
    /// Blend mode.
    pub blend: Blend,
}

/// One frame: draw `layers` into ring slot `out_idx`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Composite {
    /// Server-chosen frame serial, echoed in [`FromHelper::Composited`].
    pub serial: u64,
    /// Output ring slot.
    pub out_idx: u32,
    /// What changed since the **previous frame** (not since this slot was
    /// last drawn: the helper adds the buffer-age damage itself).
    pub damage: Vec<IRect>,
    /// Layers, bottom first.
    pub layers: Vec<Layer>,
    /// Bit `i` set: layer `i` has an acquire `sync_file`. The fds ride
    /// with the message in layer order, one per set bit.
    pub fence_mask: u32,
}

impl Composite {
    /// Number of acquire fences, i.e. fds this message carries.
    #[must_use]
    pub fn fence_count(&self) -> usize {
        self.fence_mask.count_ones() as usize
    }
}

/// One capture frame (v3): draw `frame.layers` into slot `frame.out_idx`
/// of capture ring `ring_id`. Everything else is as for a [`Composite`]
/// (the damage is since the previous frame **of this ring**; the helper
/// adds buffer age per ring), and it is answered the same way:
/// `Composited{serial}` + a `sync_file`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaptureComposite {
    /// Capture ring, from [`ToHelper::AllocCaptureRing`].
    pub ring_id: u32,
    /// The frame; `out_idx` is the capture ring slot.
    pub frame: Composite,
}

/// What the device can do; the answer to [`ToHelper::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceInfo {
    /// Vulkan device name (or the fake's).
    pub device: String,
    /// Driver name (`anv`, `hasvk`, …).
    pub driver: String,
    /// Formats + modifiers importable as sampled textures.
    pub sampleable: Vec<FormatMod>,
    /// Formats + modifiers usable for the output ring.
    pub render: Vec<FormatMod>,
}

/// Layout of one ring slot; the dma-buf fd rides with the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SlotLayout {
    /// Byte offset of the image in the fd.
    pub offset: u32,
    /// Row pitch in bytes.
    pub pitch: u32,
    /// Size of the dma-buf in bytes.
    pub size: u64,
}

/// Helper counters, the answer to [`ToHelper::GetStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Frames submitted.
    pub frames: u64,
    /// Textures imported (cumulative).
    pub imports: u64,
    /// Error replies sent.
    pub errors: u64,
    /// Textures alive now (released ones pending completion included).
    pub textures_live: u32,
    /// Frames submitted and not yet signalled.
    pub in_flight: u32,
    /// Mean CPU time of a `composite` call (record + submit), µs.
    pub submit_us_avg: u32,
    /// Worst CPU time of a `composite` call, µs.
    pub submit_us_max: u32,
    /// How the shadow reaches the GPU.
    pub shadow_path: ShadowPath,
    /// Sum of `drm-total-*` over the helper's DRM fds (fdinfo), bytes.
    pub drm_total: u64,
    /// Sum of `drm-resident-*` over the helper's DRM fds, bytes.
    pub drm_resident: u64,
    /// The helper's own `VmRSS`, bytes.
    pub rss: u64,
    /// The helper's own proportional set size (`smaps_rollup` `Pss`),
    /// bytes. Reported by the helper because it is non-dumpable: no other
    /// process may read its `/proc` files.
    pub pss: u64,
}

/// Error codes of [`FromHelper::Error`]. Every error is **non-fatal**:
/// the helper refuses the one request and carries on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ErrorCode {
    /// The payload did not decode.
    Protocol = 1,
    /// Version mismatch in `Hello`.
    Version = 2,
    /// Unknown (or already released) texture id.
    BadId = 3,
    /// Texture id already in use.
    DuplicateId = 4,
    /// A rect is empty, out of bounds or not finite.
    BadRect = 5,
    /// Format, modifier or plane layout not supported.
    BadFormat = 6,
    /// Fence count does not match the fence mask.
    Fences = 7,
    /// Too many layers, rects, slots or planes.
    TooMany = 8,
    /// No output ring allocated, or the slot index is out of range.
    NoRing = 9,
    /// The slot's previous frame has not finished on the GPU.
    Busy = 10,
    /// The backend (driver) refused.
    Backend = 11,
    /// A shadow memfd lacks the required seals or is too small.
    BadBuffer = 12,
    /// Unknown capture ring id (v3); `what` is the ring id, or the serial
    /// for a `CaptureComposite`.
    BadRing = 13,
}

impl ErrorCode {
    fn from_u16(v: u16) -> Result<Self, DecodeError> {
        Ok(match v {
            1 => Self::Protocol,
            2 => Self::Version,
            3 => Self::BadId,
            4 => Self::DuplicateId,
            5 => Self::BadRect,
            6 => Self::BadFormat,
            7 => Self::Fences,
            8 => Self::TooMany,
            9 => Self::NoRing,
            10 => Self::Busy,
            11 => Self::Backend,
            12 => Self::BadBuffer,
            13 => Self::BadRing,
            _ => return Err(DecodeError::BadValue),
        })
    }
}

/// Server → helper.
#[derive(Debug, Clone, PartialEq)]
pub enum ToHelper {
    /// First message. Answered by [`FromHelper::HelloReply`].
    Hello {
        /// [`PROTO_VERSION`].
        version: u32,
    },
    /// Import a dma-buf (one fd per plane). Answered by `Imported` or `Error`.
    ImportDmabuf(DmabufDesc),
    /// Import the shadow buffer (one sealed memfd). Answered by
    /// `Imported` or `Error`.
    ImportShadow(ShadowDesc),
    /// The shadow's pixels changed in `rects` (needed on the staging path,
    /// a no-op on the udmabuf path). No reply unless it fails.
    UploadDamage {
        /// Texture id.
        id: u32,
        /// Changed rects, texel coordinates.
        rects: Vec<IRect>,
    },
    /// (Re)allocate the output ring. Answered by `OutputRing` (n fds).
    AllocOutputRing {
        /// Slot count, 1..=[`MAX_RING`].
        n: u32,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Fourcc (XR24 / AR24).
        fourcc: u32,
        /// Acceptable modifiers (the target plane's `IN_FORMATS`).
        modifiers: Vec<u64>,
    },
    /// Draw one frame. Answered by `Composited` (+ `sync_file`) right after
    /// the GPU submit, before the GPU has done anything.
    Composite(Composite),
    /// Drop a texture. Answered by `Released` once every submitted frame
    /// that samples it has signalled.
    Release {
        /// Texture id.
        id: u32,
    },
    /// Debug/test: copy a slot to a linear BGRA memfd. Blocks the helper
    /// until the GPU is done with the slot — never use per frame.
    ReadBack {
        /// Slot.
        out_idx: u32,
    },
    /// Answered by `Stats`.
    GetStats,
    /// Exit cleanly (closing the socket does the same).
    Shutdown,
    /// Draw `layers` (bottom first) into a **temporary** `w`×`h` target
    /// and answer with its pixels (`Captured` + a sealed memfd), for a
    /// screenshot (#3962). Blocks the helper until the GPU is done, like
    /// `ReadBack`: never per frame. Pixels no layer covers are
    /// undefined. Nothing it allocates outlives the reply.
    Capture {
        /// Server-chosen serial, echoed in `Captured` (and `Error.what`).
        serial: u64,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Layers, bottom first.
        layers: Vec<Layer>,
    },
    /// v3: allocate a capture ring for screen recording, beside (and
    /// independent of) the output ring. Answered by `CaptureRing` (n
    /// fds) or `Error{what: ring_id}`. The modifier is picked LINEAR
    /// first (consumers mmap it or import it anywhere), then in the
    /// request's order; an empty list means "anything the helper renders".
    AllocCaptureRing {
        /// Server-chosen id; must not be alive (or still being freed).
        ring_id: u32,
        /// Slot count, [`MIN_CAPTURE_RING`]..=[`MAX_RING`].
        n: u32,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Fourcc: [`XR24`] only (NV12 is a follow-up).
        fourcc: u32,
        /// Acceptable modifiers; may be empty.
        modifiers: Vec<u64>,
    },
    /// v3: draw a capture frame. Answered by `Composited` (+ `sync_file`),
    /// like `Composite`; `Error.what` is the serial.
    CaptureComposite(CaptureComposite),
    /// v3: drop a capture ring. The id is dead at once (frames for it are
    /// refused); its memory is freed once every submitted frame into it
    /// has signalled. No reply unless the id is unknown.
    FreeCaptureRing {
        /// Ring id.
        ring_id: u32,
    },
}

/// Helper → server.
#[derive(Debug, Clone, PartialEq)]
pub enum FromHelper {
    /// Answer to `Hello`.
    HelloReply {
        /// [`PROTO_VERSION`].
        version: u32,
        /// Device capabilities.
        info: DeviceInfo,
    },
    /// A texture import succeeded.
    Imported {
        /// Texture id.
        id: u32,
    },
    /// A request was refused; the helper carries on.
    Error {
        /// Op code of the refused request (0 if it did not decode).
        op: u16,
        /// The texture id or frame serial it concerned (0 if none).
        what: u64,
        /// Why.
        code: ErrorCode,
        /// Human-readable detail; never interpreted.
        msg: String,
    },
    /// The ring: one dma-buf fd per slot rides with it.
    OutputRing {
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Fourcc.
        fourcc: u32,
        /// The modifier the driver picked from the request's list.
        modifier: u64,
        /// Per-slot layout.
        slots: Vec<SlotLayout>,
    },
    /// A frame was submitted; its completion `sync_file` rides with it.
    Composited {
        /// The request's serial.
        serial: u64,
    },
    /// A texture is gone and its buffers may be reused.
    Released {
        /// Texture id.
        id: u32,
    },
    /// Answer to `ReadBack`; a sealed memfd of `stride * h` bytes rides
    /// with it.
    ReadBackReply {
        /// Slot.
        out_idx: u32,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Row stride in bytes.
        stride: u32,
    },
    /// Answer to `GetStats`.
    Stats(Stats),
    /// Answer to `Capture`; a sealed memfd of `stride * h` bytes of
    /// linear BGRX rides with it.
    Captured {
        /// The request's serial.
        serial: u64,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Row stride in bytes.
        stride: u32,
    },
    /// v3: answer to `AllocCaptureRing`; one dma-buf fd per slot rides
    /// with it.
    CaptureRing {
        /// The request's ring id.
        ring_id: u32,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
        /// Fourcc.
        fourcc: u32,
        /// The modifier every slot has.
        modifier: u64,
        /// Per-slot layout.
        slots: Vec<SlotLayout>,
    },
}

/// Op codes.
pub mod op {
    /// `Hello`.
    pub const HELLO: u16 = 0x01;
    /// `ImportDmabuf`.
    pub const IMPORT_DMABUF: u16 = 0x02;
    /// `ImportShadow`.
    pub const IMPORT_SHADOW: u16 = 0x03;
    /// `UploadDamage`.
    pub const UPLOAD_DAMAGE: u16 = 0x04;
    /// `AllocOutputRing`.
    pub const ALLOC_OUTPUT_RING: u16 = 0x05;
    /// `Composite`.
    pub const COMPOSITE: u16 = 0x06;
    /// `Release`.
    pub const RELEASE: u16 = 0x07;
    /// `ReadBack`.
    pub const READ_BACK: u16 = 0x08;
    /// `GetStats`.
    pub const GET_STATS: u16 = 0x09;
    /// `Shutdown`.
    pub const SHUTDOWN: u16 = 0x0a;
    /// `Capture`.
    pub const CAPTURE: u16 = 0x0b;
    /// `AllocCaptureRing` (v3).
    pub const ALLOC_CAPTURE_RING: u16 = 0x0c;
    /// `CaptureComposite` (v3).
    pub const CAPTURE_COMPOSITE: u16 = 0x0d;
    /// `FreeCaptureRing` (v3).
    pub const FREE_CAPTURE_RING: u16 = 0x0e;

    /// `HelloReply`.
    pub const HELLO_REPLY: u16 = 0x81;
    /// `Imported`.
    pub const IMPORTED: u16 = 0x82;
    /// `Error`.
    pub const ERROR: u16 = 0x83;
    /// `OutputRing`.
    pub const OUTPUT_RING: u16 = 0x84;
    /// `Composited`.
    pub const COMPOSITED: u16 = 0x85;
    /// `Released`.
    pub const RELEASED: u16 = 0x86;
    /// `ReadBackReply`.
    pub const READ_BACK_REPLY: u16 = 0x87;
    /// `Stats`.
    pub const STATS: u16 = 0x88;
    /// `Captured`.
    pub const CAPTURED: u16 = 0x89;
    /// `CaptureRing` (v3).
    pub const CAPTURE_RING: u16 = 0x8a;
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
    /// [`EncodeError::TooLarge`] if a list exceeds its protocol maximum.
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
    /// [`DecodeError::UnexpectedFd`] when the fd count is wrong for the
    /// message.
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

    /// Refuse lists over their protocol maximum.
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

fn put_str(w: &mut Writer, s: &str) {
    let mut end = s.len().min(MAX_STR);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    w.put_str(&s[..end]);
}

fn get_str(r: &mut Reader<'_>) -> Result<String, DecodeError> {
    let s = r.get_str()?;
    if s.len() > MAX_STR {
        return Err(DecodeError::TooLarge);
    }
    Ok(s)
}

fn put_rect(w: &mut Writer, r: IRect) {
    w.put_i32(r.x);
    w.put_i32(r.y);
    w.put_i32(r.w);
    w.put_i32(r.h);
}

fn get_rect(r: &mut Reader<'_>) -> Result<IRect, DecodeError> {
    Ok(IRect::new(
        r.get_i32()?,
        r.get_i32()?,
        r.get_i32()?,
        r.get_i32()?,
    ))
}

fn put_rects(w: &mut Writer, rects: &[IRect]) {
    w.put_u32(rects.len() as u32);
    for r in rects {
        put_rect(w, *r);
    }
}

fn get_rects(r: &mut Reader<'_>) -> Result<Vec<IRect>, DecodeError> {
    let n = get_count(r, MAX_RECTS, 16)?;
    (0..n).map(|_| get_rect(r)).collect()
}

fn put_layers(w: &mut Writer, layers: &[Layer]) {
    w.put_u32(layers.len() as u32);
    for l in layers {
        w.put_u32(l.tex);
        for v in l.src {
            w.put_f32(v);
        }
        put_rect(w, l.dst);
        w.put_u8(l.blend.to_u8());
    }
}

fn get_layers(r: &mut Reader<'_>) -> Result<Vec<Layer>, DecodeError> {
    let n = get_count(r, MAX_LAYERS, 37)?;
    let mut layers = Vec::with_capacity(n);
    for _ in 0..n {
        let tex = r.get_u32()?;
        let src = [r.get_f32()?, r.get_f32()?, r.get_f32()?, r.get_f32()?];
        let dst = get_rect(r)?;
        let blend = Blend::from_u8(r.get_u8()?)?;
        layers.push(Layer {
            tex,
            src,
            dst,
            blend,
        });
    }
    Ok(layers)
}

fn composite_ok(c: &Composite) -> bool {
    c.damage.len() <= MAX_RECTS
        && c.layers.len() <= MAX_LAYERS
        && c.fence_mask >> c.layers.len() == 0
}

/// `fence_mask`, damage, layers: the tail both frame messages share.
fn put_frame_tail(w: &mut Writer, c: &Composite) {
    w.put_u32(c.fence_mask);
    put_rects(w, &c.damage);
    put_layers(w, &c.layers);
}

fn get_frame_tail(r: &mut Reader<'_>, serial: u64, out_idx: u32) -> Result<Composite, DecodeError> {
    let fence_mask = r.get_u32()?;
    let damage = get_rects(r)?;
    let layers = get_layers(r)?;
    if fence_mask >> layers.len() != 0 {
        return Err(DecodeError::BadValue);
    }
    Ok(Composite {
        serial,
        out_idx,
        damage,
        layers,
        fence_mask,
    })
}

fn put_modifiers(w: &mut Writer, modifiers: &[u64]) {
    w.put_u32(modifiers.len() as u32);
    for m in modifiers {
        w.put_u64(*m);
    }
}

fn get_modifiers(r: &mut Reader<'_>) -> Result<Vec<u64>, DecodeError> {
    let count = get_count(r, MAX_MODIFIERS, 8)?;
    (0..count).map(|_| r.get_u64()).collect()
}

fn put_slots(w: &mut Writer, slots: &[SlotLayout]) {
    w.put_u32(slots.len() as u32);
    for s in slots {
        w.put_u32(s.offset);
        w.put_u32(s.pitch);
        w.put_u64(s.size);
    }
}

fn get_slots(r: &mut Reader<'_>) -> Result<Vec<SlotLayout>, DecodeError> {
    let n = get_count(r, MAX_RING, 16)?;
    (0..n)
        .map(|_| {
            Ok(SlotLayout {
                offset: r.get_u32()?,
                pitch: r.get_u32()?,
                size: r.get_u64()?,
            })
        })
        .collect()
}

fn put_formats(w: &mut Writer, f: &[FormatMod]) {
    w.put_u32(f.len() as u32);
    for f in f {
        w.put_u32(f.fourcc);
        w.put_u64(f.modifier);
    }
}

fn get_formats(r: &mut Reader<'_>) -> Result<Vec<FormatMod>, DecodeError> {
    let n = get_count(r, MAX_FORMATS, 12)?;
    (0..n)
        .map(|_| {
            Ok(FormatMod {
                fourcc: r.get_u32()?,
                modifier: r.get_u64()?,
            })
        })
        .collect()
}

fn enc_u8(v: u8, max: u8) -> Result<u8, DecodeError> {
    if v <= max {
        Ok(v)
    } else {
        Err(DecodeError::BadValue)
    }
}

impl ColorEncoding {
    fn to_u8(self) -> u8 {
        match self {
            Self::Bt601 => 0,
            Self::Bt709 => 1,
            Self::Bt2020 => 2,
        }
    }
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok([Self::Bt601, Self::Bt709, Self::Bt2020][enc_u8(v, 2)? as usize])
    }
}

impl ColorRange {
    fn to_u8(self) -> u8 {
        match self {
            Self::Limited => 0,
            Self::Full => 1,
        }
    }
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok([Self::Limited, Self::Full][enc_u8(v, 1)? as usize])
    }
}

impl Blend {
    fn to_u8(self) -> u8 {
        match self {
            Self::Opaque => 0,
            Self::PremulOver => 1,
        }
    }
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok([Self::Opaque, Self::PremulOver][enc_u8(v, 1)? as usize])
    }
}

impl ShadowPath {
    fn to_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Udmabuf => 1,
            Self::Staging => 2,
        }
    }
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok([Self::Unknown, Self::Udmabuf, Self::Staging][enc_u8(v, 2)? as usize])
    }
}

impl Message for ToHelper {
    fn op(&self) -> u16 {
        match self {
            Self::Hello { .. } => op::HELLO,
            Self::ImportDmabuf(_) => op::IMPORT_DMABUF,
            Self::ImportShadow(_) => op::IMPORT_SHADOW,
            Self::UploadDamage { .. } => op::UPLOAD_DAMAGE,
            Self::AllocOutputRing { .. } => op::ALLOC_OUTPUT_RING,
            Self::Composite(_) => op::COMPOSITE,
            Self::Release { .. } => op::RELEASE,
            Self::ReadBack { .. } => op::READ_BACK,
            Self::GetStats => op::GET_STATS,
            Self::Shutdown => op::SHUTDOWN,
            Self::Capture { .. } => op::CAPTURE,
            Self::AllocCaptureRing { .. } => op::ALLOC_CAPTURE_RING,
            Self::CaptureComposite(_) => op::CAPTURE_COMPOSITE,
            Self::FreeCaptureRing { .. } => op::FREE_CAPTURE_RING,
        }
    }

    fn fd_count(&self) -> usize {
        match self {
            Self::ImportDmabuf(d) => d.planes.len(),
            Self::ImportShadow(_) => 1,
            Self::Composite(c) => c.fence_count(),
            Self::CaptureComposite(c) => c.frame.fence_count(),
            _ => 0,
        }
    }

    fn check_limits(&self) -> Result<(), EncodeError> {
        match self {
            Self::ImportDmabuf(d) => too_large(d.planes.len() <= MAX_PLANES),
            Self::UploadDamage { rects, .. } => too_large(rects.len() <= MAX_RECTS),
            Self::AllocOutputRing { modifiers, .. } | Self::AllocCaptureRing { modifiers, .. } => {
                too_large(modifiers.len() <= MAX_MODIFIERS)
            }
            Self::Composite(c) => too_large(composite_ok(c)),
            Self::CaptureComposite(c) => too_large(composite_ok(&c.frame)),
            Self::Capture { layers, .. } => too_large(layers.len() <= MAX_LAYERS),
            _ => Ok(()),
        }
    }

    fn encode_body(&self, w: &mut Writer) {
        match self {
            Self::Hello { version } => w.put_u32(*version),
            Self::ImportDmabuf(d) => {
                w.put_u32(d.id);
                w.put_u32(d.w);
                w.put_u32(d.h);
                w.put_u32(d.fourcc);
                w.put_u64(d.modifier);
                w.put_u8(d.encoding.to_u8());
                w.put_u8(d.range.to_u8());
                w.put_u32(d.planes.len() as u32);
                for p in &d.planes {
                    w.put_u32(p.offset);
                    w.put_u32(p.pitch);
                }
            }
            Self::ImportShadow(s) => {
                w.put_u32(s.id);
                w.put_u32(s.w);
                w.put_u32(s.h);
                w.put_u32(s.stride);
                w.put_u32(s.fourcc);
            }
            Self::UploadDamage { id, rects } => {
                w.put_u32(*id);
                put_rects(w, rects);
            }
            Self::AllocOutputRing {
                n,
                w: width,
                h,
                fourcc,
                modifiers,
            } => {
                w.put_u32(*n);
                w.put_u32(*width);
                w.put_u32(*h);
                w.put_u32(*fourcc);
                put_modifiers(w, modifiers);
            }
            Self::Composite(c) => {
                w.put_u64(c.serial);
                w.put_u32(c.out_idx);
                put_frame_tail(w, c);
            }
            Self::AllocCaptureRing {
                ring_id,
                n,
                w: width,
                h,
                fourcc,
                modifiers,
            } => {
                w.put_u32(*ring_id);
                w.put_u32(*n);
                w.put_u32(*width);
                w.put_u32(*h);
                w.put_u32(*fourcc);
                put_modifiers(w, modifiers);
            }
            Self::CaptureComposite(c) => {
                w.put_u64(c.frame.serial);
                w.put_u32(c.ring_id);
                w.put_u32(c.frame.out_idx);
                put_frame_tail(w, &c.frame);
            }
            Self::FreeCaptureRing { ring_id } => w.put_u32(*ring_id),
            Self::Release { id } => w.put_u32(*id),
            Self::ReadBack { out_idx } => w.put_u32(*out_idx),
            Self::GetStats | Self::Shutdown => {}
            Self::Capture {
                serial,
                w: width,
                h,
                layers,
            } => {
                w.put_u64(*serial);
                w.put_u32(*width);
                w.put_u32(*h);
                put_layers(w, layers);
            }
        }
    }

    fn decode_body(op: u16, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match op {
            op::HELLO => Self::Hello {
                version: r.get_u32()?,
            },
            op::IMPORT_DMABUF => {
                let id = r.get_u32()?;
                let w = r.get_u32()?;
                let h = r.get_u32()?;
                let fourcc = r.get_u32()?;
                let modifier = r.get_u64()?;
                let encoding = ColorEncoding::from_u8(r.get_u8()?)?;
                let range = ColorRange::from_u8(r.get_u8()?)?;
                let n = get_count(r, MAX_PLANES, 8)?;
                let planes = (0..n)
                    .map(|_| {
                        Ok(PlaneDesc {
                            offset: r.get_u32()?,
                            pitch: r.get_u32()?,
                        })
                    })
                    .collect::<Result<_, DecodeError>>()?;
                Self::ImportDmabuf(DmabufDesc {
                    id,
                    w,
                    h,
                    fourcc,
                    modifier,
                    planes,
                    encoding,
                    range,
                })
            }
            op::IMPORT_SHADOW => Self::ImportShadow(ShadowDesc {
                id: r.get_u32()?,
                w: r.get_u32()?,
                h: r.get_u32()?,
                stride: r.get_u32()?,
                fourcc: r.get_u32()?,
            }),
            op::UPLOAD_DAMAGE => Self::UploadDamage {
                id: r.get_u32()?,
                rects: get_rects(r)?,
            },
            op::ALLOC_OUTPUT_RING => {
                let n = r.get_u32()?;
                let w = r.get_u32()?;
                let h = r.get_u32()?;
                let fourcc = r.get_u32()?;
                let modifiers = get_modifiers(r)?;
                Self::AllocOutputRing {
                    n,
                    w,
                    h,
                    fourcc,
                    modifiers,
                }
            }
            op::COMPOSITE => {
                let serial = r.get_u64()?;
                let out_idx = r.get_u32()?;
                Self::Composite(get_frame_tail(r, serial, out_idx)?)
            }
            op::ALLOC_CAPTURE_RING => Self::AllocCaptureRing {
                ring_id: r.get_u32()?,
                n: r.get_u32()?,
                w: r.get_u32()?,
                h: r.get_u32()?,
                fourcc: r.get_u32()?,
                modifiers: get_modifiers(r)?,
            },
            op::CAPTURE_COMPOSITE => {
                let serial = r.get_u64()?;
                let ring_id = r.get_u32()?;
                let slot = r.get_u32()?;
                Self::CaptureComposite(CaptureComposite {
                    ring_id,
                    frame: get_frame_tail(r, serial, slot)?,
                })
            }
            op::FREE_CAPTURE_RING => Self::FreeCaptureRing {
                ring_id: r.get_u32()?,
            },
            op::RELEASE => Self::Release { id: r.get_u32()? },
            op::READ_BACK => Self::ReadBack {
                out_idx: r.get_u32()?,
            },
            op::GET_STATS => Self::GetStats,
            op::SHUTDOWN => Self::Shutdown,
            op::CAPTURE => Self::Capture {
                serial: r.get_u64()?,
                w: r.get_u32()?,
                h: r.get_u32()?,
                layers: get_layers(r)?,
            },
            other => return Err(DecodeError::UnknownOp(other)),
        })
    }
}

impl Message for FromHelper {
    fn op(&self) -> u16 {
        match self {
            Self::HelloReply { .. } => op::HELLO_REPLY,
            Self::Imported { .. } => op::IMPORTED,
            Self::Error { .. } => op::ERROR,
            Self::OutputRing { .. } => op::OUTPUT_RING,
            Self::Composited { .. } => op::COMPOSITED,
            Self::Released { .. } => op::RELEASED,
            Self::ReadBackReply { .. } => op::READ_BACK_REPLY,
            Self::Stats(_) => op::STATS,
            Self::Captured { .. } => op::CAPTURED,
            Self::CaptureRing { .. } => op::CAPTURE_RING,
        }
    }

    fn fd_count(&self) -> usize {
        match self {
            Self::OutputRing { slots, .. } | Self::CaptureRing { slots, .. } => slots.len(),
            Self::Composited { .. } | Self::ReadBackReply { .. } | Self::Captured { .. } => 1,
            _ => 0,
        }
    }

    fn check_limits(&self) -> Result<(), EncodeError> {
        match self {
            Self::HelloReply { info, .. } => {
                too_large(info.sampleable.len() <= MAX_FORMATS && info.render.len() <= MAX_FORMATS)
            }
            Self::OutputRing { slots, .. } | Self::CaptureRing { slots, .. } => {
                too_large(slots.len() <= MAX_RING)
            }
            _ => Ok(()),
        }
    }

    fn encode_body(&self, w: &mut Writer) {
        match self {
            Self::HelloReply { version, info } => {
                w.put_u32(*version);
                put_str(w, &info.device);
                put_str(w, &info.driver);
                put_formats(w, &info.sampleable);
                put_formats(w, &info.render);
            }
            Self::Imported { id } | Self::Released { id } => w.put_u32(*id),
            Self::Error {
                op,
                what,
                code,
                msg,
            } => {
                w.put_u16(*op);
                w.put_u64(*what);
                w.put_u16(*code as u16);
                put_str(w, msg);
            }
            Self::OutputRing {
                w: width,
                h,
                fourcc,
                modifier,
                slots,
            } => {
                w.put_u32(*width);
                w.put_u32(*h);
                w.put_u32(*fourcc);
                w.put_u64(*modifier);
                put_slots(w, slots);
            }
            Self::CaptureRing {
                ring_id,
                w: width,
                h,
                fourcc,
                modifier,
                slots,
            } => {
                w.put_u32(*ring_id);
                w.put_u32(*width);
                w.put_u32(*h);
                w.put_u32(*fourcc);
                w.put_u64(*modifier);
                put_slots(w, slots);
            }
            Self::Composited { serial } => w.put_u64(*serial),
            Self::ReadBackReply {
                out_idx,
                w: width,
                h,
                stride,
            } => {
                w.put_u32(*out_idx);
                w.put_u32(*width);
                w.put_u32(*h);
                w.put_u32(*stride);
            }
            Self::Captured {
                serial,
                w: width,
                h,
                stride,
            } => {
                w.put_u64(*serial);
                w.put_u32(*width);
                w.put_u32(*h);
                w.put_u32(*stride);
            }
            Self::Stats(s) => {
                w.put_u64(s.frames);
                w.put_u64(s.imports);
                w.put_u64(s.errors);
                w.put_u32(s.textures_live);
                w.put_u32(s.in_flight);
                w.put_u32(s.submit_us_avg);
                w.put_u32(s.submit_us_max);
                w.put_u8(s.shadow_path.to_u8());
                w.put_u64(s.drm_total);
                w.put_u64(s.drm_resident);
                w.put_u64(s.rss);
                w.put_u64(s.pss);
            }
        }
    }

    fn decode_body(op: u16, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match op {
            op::HELLO_REPLY => Self::HelloReply {
                version: r.get_u32()?,
                info: DeviceInfo {
                    device: get_str(r)?,
                    driver: get_str(r)?,
                    sampleable: get_formats(r)?,
                    render: get_formats(r)?,
                },
            },
            op::IMPORTED => Self::Imported { id: r.get_u32()? },
            op::RELEASED => Self::Released { id: r.get_u32()? },
            op::ERROR => Self::Error {
                op: r.get_u16()?,
                what: r.get_u64()?,
                code: ErrorCode::from_u16(r.get_u16()?)?,
                msg: get_str(r)?,
            },
            op::OUTPUT_RING => {
                let w = r.get_u32()?;
                let h = r.get_u32()?;
                let fourcc = r.get_u32()?;
                let modifier = r.get_u64()?;
                let slots = get_slots(r)?;
                Self::OutputRing {
                    w,
                    h,
                    fourcc,
                    modifier,
                    slots,
                }
            }
            op::COMPOSITED => Self::Composited {
                serial: r.get_u64()?,
            },
            op::READ_BACK_REPLY => Self::ReadBackReply {
                out_idx: r.get_u32()?,
                w: r.get_u32()?,
                h: r.get_u32()?,
                stride: r.get_u32()?,
            },
            op::CAPTURED => Self::Captured {
                serial: r.get_u64()?,
                w: r.get_u32()?,
                h: r.get_u32()?,
                stride: r.get_u32()?,
            },
            op::CAPTURE_RING => Self::CaptureRing {
                ring_id: r.get_u32()?,
                w: r.get_u32()?,
                h: r.get_u32()?,
                fourcc: r.get_u32()?,
                modifier: r.get_u64()?,
                slots: get_slots(r)?,
            },
            op::STATS => Self::Stats(Stats {
                frames: r.get_u64()?,
                imports: r.get_u64()?,
                errors: r.get_u64()?,
                textures_live: r.get_u32()?,
                in_flight: r.get_u32()?,
                submit_us_avg: r.get_u32()?,
                submit_us_max: r.get_u32()?,
                shadow_path: ShadowPath::from_u8(r.get_u8()?)?,
                drm_total: r.get_u64()?,
                drm_resident: r.get_u64()?,
                rss: r.get_u64()?,
                pss: r.get_u64()?,
            }),
            other => return Err(DecodeError::UnknownOp(other)),
        })
    }
}
