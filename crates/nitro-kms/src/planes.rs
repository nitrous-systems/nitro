//! Plane discovery and `TEST_ONLY` layouts: the vocabulary.
//!
//! What a display engine can compose per output — which planes exist,
//! which formats and modifiers each takes, how they stack, whether they
//! scale — and a way to ask "would this layout work?" without touching
//! the screen ([`Backend::test_layout`](crate::Backend::test_layout)).
//!
//! No DRM type leaks through here: the server's `planes` module codes
//! against these types and the fake backend's rule-based acceptor, and
//! the DRM backend maps them onto plane properties and an atomic commit
//! with `DRM_MODE_ATOMIC_TEST_ONLY`.
//!
//! **Advertised is not usable.** A plane that lists NV12 may still refuse
//! it at a given size, scale or position, and two planes that each accept
//! a buffer alone may not accept both at once (shared scalers, memory
//! bandwidth, watermarks). Discovery narrows the search; only
//! `test_layout` answers it.

use std::fmt;
use std::io;
use std::os::fd::BorrowedFd;

use crate::Rect;

/// A plane: the DRM object id on the real backend, a backend-unique
/// number on the fake one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaneId(pub u32);

impl fmt::Display for PlaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plane#{}", self.0)
    }
}

/// A scanout buffer allocated with
/// [`Backend::alloc_buffer`](crate::Backend::alloc_buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BufferId(pub u32);

impl fmt::Display for BufferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "buffer#{}", self.0)
    }
}

/// A DRM fourcc pixel format code (`drm_fourcc.h`), little-endian ASCII.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fourcc(pub u32);

impl Fourcc {
    const fn code(s: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(s))
    }

    /// `XR24`: 32-bit `0x00RRGGBB`, the format of every output buffer.
    pub const XRGB8888: Fourcc = Fourcc::code(*b"XR24");
    /// `AR24`: 32-bit `0xAARRGGBB`, premultiplied as far as KMS cares.
    pub const ARGB8888: Fourcc = Fourcc::code(*b"AR24");
    /// `XB24`: 32-bit `0x00BBGGRR`.
    pub const XBGR8888: Fourcc = Fourcc::code(*b"XB24");
    /// `NV12`: 8-bit Y plane plus an interleaved 2×2-subsampled `CbCr` plane.
    pub const NV12: Fourcc = Fourcc::code(*b"NV12");
    /// `YUYV`: packed 4:2:2, `Y0 Cb Y1 Cr` bytes.
    pub const YUYV: Fourcc = Fourcc::code(*b"YUYV");
    /// `UYVY`: packed 4:2:2, `Cb Y0 Cr Y1` bytes.
    pub const UYVY: Fourcc = Fourcc::code(*b"UYVY");

    /// Whether this is a YCbCr format, i.e. one for which
    /// `COLOR_ENCODING` / `COLOR_RANGE` mean something.
    #[must_use]
    pub fn is_yuv(self) -> bool {
        matches!(self, Fourcc::NV12 | Fourcc::YUYV | Fourcc::UYVY)
            || matches!(&self.0.to_le_bytes(), [b'N' | b'Y' | b'U' | b'V', ..])
    }
}

impl fmt::Display for Fourcc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.to_le_bytes() {
            let c = if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '?'
            };
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fourcc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fourcc({self})")
    }
}

/// `DRM_FORMAT_MOD_LINEAR`: plain row-major memory, what a dumb buffer is.
pub const MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: "no explicit modifier".
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A short human name for a format modifier: the common ones by name,
/// anything else as hex.
#[must_use]
pub fn modifier_name(m: u64) -> String {
    const INTEL: u64 = 0x01 << 56;
    match m {
        MOD_LINEAR => "LINEAR".into(),
        MOD_INVALID => "INVALID".into(),
        x if x == INTEL | 1 => "I915_X_TILED".into(),
        x if x == INTEL | 2 => "I915_Y_TILED".into(),
        x if x == INTEL | 3 => "I915_Yf_TILED".into(),
        x if x == INTEL | 4 => "I915_Y_TILED_CCS".into(),
        x if x == INTEL | 5 => "I915_Yf_TILED_CCS".into(),
        x if x == INTEL | 9 => "I915_4_TILED".into(),
        x => format!("{x:#018x}"),
    }
}

/// The `rotation` property's bits (`DRM_MODE_ROTATE_*` / `REFLECT_*`).
pub mod rotation {
    /// No rotation.
    pub const ROTATE_0: u32 = 1 << 0;
    /// 90° counter-clockwise.
    pub const ROTATE_90: u32 = 1 << 1;
    /// 180°.
    pub const ROTATE_180: u32 = 1 << 2;
    /// 270° counter-clockwise.
    pub const ROTATE_270: u32 = 1 << 3;
    /// Mirror along the x axis.
    pub const REFLECT_X: u32 = 1 << 4;
    /// Mirror along the y axis.
    pub const REFLECT_Y: u32 = 1 << 5;

    /// `"rotate-0|reflect-x"`-style text for a mask, `"-"` for none.
    #[must_use]
    pub fn describe(mask: u32) -> String {
        const NAMES: [&str; 6] = [
            "rotate-0",
            "rotate-90",
            "rotate-180",
            "rotate-270",
            "reflect-x",
            "reflect-y",
        ];
        let v: Vec<&str> = NAMES
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, n)| *n)
            .collect();
        if v.is_empty() {
            "-".into()
        } else {
            v.join("|")
        }
    }
}

/// What kind of plane the kernel says this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaneKind {
    /// The CRTC's main plane; what [`Backend::commit`](crate::Backend::commit) flips.
    Primary,
    /// An overlay ("sprite") plane.
    Overlay,
    /// A cursor plane: usually small, fixed sizes, no scaling.
    Cursor,
}

impl fmt::Display for PlaneKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PlaneKind::Primary => "primary",
            PlaneKind::Overlay => "overlay",
            PlaneKind::Cursor => "cursor",
        })
    }
}

/// A plane's `zpos` property: where it stacks. Higher is nearer the
/// viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Zpos {
    /// The current value.
    pub current: u64,
    /// Smallest allowed value.
    pub min: u64,
    /// Largest allowed value.
    pub max: u64,
    /// The driver fixes the order (`DRM_MODE_PROP_IMMUTABLE`, as on
    /// Intel): the property reports it and cannot be set. Underlay there
    /// is done by *assignment* — the video on a lower plane — not by
    /// reordering.
    pub immutable: bool,
}

/// `COLOR_ENCODING`: the YCbCr → RGB matrix a plane applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorEncoding {
    /// ITU-R BT.601 (SD video, JPEG).
    Bt601,
    /// ITU-R BT.709 (HD video).
    Bt709,
    /// ITU-R BT.2020.
    Bt2020,
}

impl ColorEncoding {
    /// The enum name as the kernel spells it.
    #[must_use]
    pub const fn kernel_name(self) -> &'static str {
        match self {
            ColorEncoding::Bt601 => "ITU-R BT.601 YCbCr",
            ColorEncoding::Bt709 => "ITU-R BT.709 YCbCr",
            ColorEncoding::Bt2020 => "ITU-R BT.2020 YCbCr",
        }
    }
}

/// `COLOR_RANGE`: whether YCbCr samples use the full 0–255 range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorRange {
    /// 16–235 luma, 16–240 chroma (video).
    Limited,
    /// 0–255 (JPEG).
    Full,
}

impl ColorRange {
    /// The enum name as the kernel spells it.
    #[must_use]
    pub const fn kernel_name(self) -> &'static str {
        match self {
            ColorRange::Limited => "YCbCr limited range",
            ColorRange::Full => "YCbCr full range",
        }
    }
}

/// Everything discovery learns about one plane. Read once per
/// [`Backend::rescan`](crate::Backend::rescan), not per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneInfo {
    /// Its id.
    pub id: PlaneId,
    /// Primary, overlay or cursor.
    pub kind: PlaneKind,
    /// `possible_crtcs`: bit *i* set when the plane can go on CRTC index
    /// *i*. On the fake backend each output is its own CRTC with bit
    /// `(id - 1) % 32`.
    pub crtc_mask: u32,
    /// Supported formats, each with its supported modifiers (from the
    /// `IN_FORMATS` blob; `[MOD_LINEAR]` when the plane has none).
    pub formats: Vec<(Fourcc, Vec<u64>)>,
    /// Stacking, when the plane has a `zpos` property.
    pub zpos: Option<Zpos>,
    /// Supported `rotation` bits ([`rotation`]); 0 when there is no
    /// property (then only `ROTATE_0`).
    pub rotations: u32,
    /// `COLOR_ENCODING` enum names, kernel spelling; empty when absent.
    pub color_encodings: Vec<String>,
    /// `COLOR_RANGE` enum names, kernel spelling; empty when absent.
    pub color_ranges: Vec<String>,
    /// `pixel blend mode` enum names; empty when absent.
    pub blend_modes: Vec<String>,
    /// Has a plane-wide `alpha` property.
    pub alpha: bool,
    /// Has `FB_DAMAGE_CLIPS`.
    pub damage_clips: bool,
    /// Has `IN_FENCE_FD` (can wait on a `sync_file` before scanning out).
    pub in_fence: bool,
    /// Whether the plane scales. `None` when unknown: no KMS property
    /// says so, only a `TEST_ONLY` commit can tell (the DRM backend
    /// reports `None`; the fake reports what it was configured with).
    pub scaling: Option<bool>,
}

impl PlaneInfo {
    /// Whether the plane lists `format` with `modifier`.
    #[must_use]
    pub fn supports(&self, format: Fourcc, modifier: u64) -> bool {
        self.formats
            .iter()
            .any(|(f, mods)| *f == format && mods.contains(&modifier))
    }

    /// Whether `COLOR_ENCODING` offers `e`.
    #[must_use]
    pub fn has_encoding(&self, e: ColorEncoding) -> bool {
        self.color_encodings.iter().any(|n| n == e.kernel_name())
    }

    /// Whether `COLOR_RANGE` offers `r`.
    #[must_use]
    pub fn has_range(&self, r: ColorRange) -> bool {
        self.color_ranges.iter().any(|n| n == r.kernel_name())
    }
}

/// A source rectangle in the buffer, **16.16 fixed point** like the
/// `SRC_*` plane properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SrcRect {
    /// Left, 16.16.
    pub x: u32,
    /// Top, 16.16.
    pub y: u32,
    /// Width, 16.16.
    pub w: u32,
    /// Height, 16.16.
    pub h: u32,
}

impl SrcRect {
    /// A rectangle in whole pixels.
    #[must_use]
    pub const fn pixels(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self {
            x: x << 16,
            y: y << 16,
            w: w << 16,
            h: h << 16,
        }
    }

    /// The whole of a `w × h` buffer.
    #[must_use]
    pub const fn whole(w: u32, h: u32) -> Self {
        Self::pixels(0, 0, w, h)
    }
}

/// What a plane scans out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaneSource {
    /// The output's current front buffer (`XRGB8888`, output-sized): what
    /// is on its primary plane now.
    OutputFront,
    /// A buffer from [`Backend::alloc_buffer`](crate::Backend::alloc_buffer).
    Buffer(BufferId),
}

/// One plane's part in a candidate layout.
#[derive(Debug, Clone, Copy)]
pub struct PlaneAssignment<'a> {
    /// Which plane.
    pub plane: PlaneId,
    /// What it shows.
    pub source: PlaneSource,
    /// Which part of the source (16.16).
    pub src: SrcRect,
    /// Where on the output (`CRTC_*`), in output pixels; may extend past
    /// the edges.
    pub dst: Rect,
    /// `zpos` to set. `None` leaves the plane's current value.
    pub zpos: Option<u64>,
    /// `rotation` bits to set. `None` leaves it (normally `ROTATE_0`).
    pub rotation: Option<u32>,
    /// `COLOR_ENCODING` to set (YCbCr sources).
    pub color_encoding: Option<ColorEncoding>,
    /// `COLOR_RANGE` to set (YCbCr sources).
    pub color_range: Option<ColorRange>,
    /// A `sync_file` fd the plane waits on (`IN_FENCE_FD`). Borrowed: it is
    /// only read for the duration of the call.
    pub in_fence: Option<BorrowedFd<'a>>,
}

impl PlaneAssignment<'_> {
    /// `source`'s `src` shown at `dst` on `plane`, nothing else set.
    #[must_use]
    pub fn new(plane: PlaneId, source: PlaneSource, src: SrcRect, dst: Rect) -> Self {
        Self {
            plane,
            source,
            src,
            dst,
            zpos: None,
            rotation: None,
            color_encoding: None,
            color_range: None,
            in_fence: None,
        }
    }

    /// Set `zpos`.
    #[must_use]
    pub fn with_zpos(mut self, z: u64) -> Self {
        self.zpos = Some(z);
        self
    }

    /// Whether the source and destination sizes differ (the plane must
    /// scale).
    #[must_use]
    pub fn scales(&self) -> bool {
        u64::from(self.src.w) != u64::from(self.dst.w) << 16
            || u64::from(self.src.h) != u64::from(self.dst.h) << 16
    }
}

/// The kernel's answer to a `TEST_ONLY` layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// The layout would work as the next commit.
    Accepted,
    /// Refused, with the errno (`EINVAL`, `ERANGE`, `ENOSPC`, …).
    Rejected(i32),
}

impl Verdict {
    /// `Rejected(EINVAL)`, the generic "no".
    #[must_use]
    pub fn einval() -> Self {
        Verdict::Rejected(rustix::io::Errno::INVAL.raw_os_error())
    }

    /// Whether the layout was accepted.
    #[must_use]
    pub fn accepted(self) -> bool {
        self == Verdict::Accepted
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Accepted => f.write_str("ACCEPT"),
            Verdict::Rejected(e) => {
                write!(f, "REJECT {}", errno_name(*e))
            }
        }
    }
}

/// `EINVAL`-style name for the errnos a commit returns, the number
/// otherwise.
#[must_use]
pub fn errno_name(e: i32) -> String {
    use rustix::io::Errno;
    let known = [
        (Errno::INVAL, "EINVAL"),
        (Errno::RANGE, "ERANGE"),
        (Errno::NOSPC, "ENOSPC"),
        (Errno::NOMEM, "ENOMEM"),
        (Errno::BUSY, "EBUSY"),
        (Errno::NOENT, "ENOENT"),
        (Errno::ACCESS, "EACCES"),
        (Errno::PERM, "EPERM"),
        (Errno::OPNOTSUPP, "EOPNOTSUPP"),
        (Errno::NODEV, "ENODEV"),
    ];
    known
        .iter()
        .find(|(k, _)| k.raw_os_error() == e)
        .map_or_else(
            || io::Error::from_raw_os_error(e).to_string(),
            |(_, n)| (*n).to_owned(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_codes_and_names() {
        assert_eq!(Fourcc::XRGB8888.0, 875_713_112);
        assert_eq!(Fourcc::NV12.to_string(), "NV12");
        assert_eq!(Fourcc::XRGB8888.to_string(), "XR24");
        assert!(Fourcc::NV12.is_yuv());
        assert!(!Fourcc::ARGB8888.is_yuv());
    }

    #[test]
    fn modifier_and_rotation_text() {
        assert_eq!(modifier_name(MOD_LINEAR), "LINEAR");
        assert_eq!(modifier_name((1 << 56) | 1), "I915_X_TILED");
        assert_eq!(modifier_name(0x0300_0000_0000_0001), "0x0300000000000001");
        assert_eq!(
            rotation::describe(rotation::ROTATE_0 | rotation::ROTATE_180),
            "rotate-0|rotate-180"
        );
        assert_eq!(rotation::describe(0), "-");
    }

    #[test]
    fn assignment_scaling_is_in_16_16() {
        let a = PlaneAssignment::new(
            PlaneId(1),
            PlaneSource::OutputFront,
            SrcRect::whole(1280, 720),
            Rect::new(0, 0, 1280, 720),
        );
        assert!(!a.scales());
        let b = PlaneAssignment {
            dst: Rect::new(0, 0, 1920, 1080),
            ..a
        };
        assert!(b.scales());
    }

    #[test]
    fn verdict_text() {
        assert_eq!(Verdict::Accepted.to_string(), "ACCEPT");
        assert_eq!(Verdict::einval().to_string(), "REJECT EINVAL");
    }
}
