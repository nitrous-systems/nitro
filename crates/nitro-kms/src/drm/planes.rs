//! Plane discovery, scanout buffers and `TEST_ONLY` layouts for the DRM
//! backend. Read-only with respect to the frame path: nothing here is
//! used by `commit`, `flip` or the modesets.
//!
//! Enum names for `rotation` (a BITMASK property), `COLOR_ENCODING`,
//! `COLOR_RANGE` and `pixel blend mode` come from
//! `drm_ffi::mode::get_property`, because `drm`'s `get_property` drops
//! the names of bitmask properties. Both are safe calls; see
//! DEPENDENCIES.md.

use std::collections::HashMap;

use ::drm::buffer::{DrmFourcc, DrmModifier, PlanarBuffer};
use ::drm::control::dumbbuffer::DumbBuffer;
use ::drm::control::{
    Device as ControlDevice, FbCmd2Flags, ResourceHandles, framebuffer, plane, property,
};
use std::os::fd::AsFd;

use super::{Card, PropMap};
use crate::Error;
use crate::planes::{
    Fourcc, MOD_LINEAR, PlaneAssignment, PlaneId, PlaneInfo, PlaneKind, Zpos, rotation,
};
use ::drm::control::atomic::AtomicModeReq;

/// The optional plane properties a `TEST_ONLY` layout may set, beyond
/// the ones `PlaneProps` caches for the frame path.
#[derive(Debug, Default)]
pub(super) struct ExtraProps {
    pub zpos: Option<property::Handle>,
    pub zpos_immutable: bool,
    pub rotation: Option<property::Handle>,
    pub color_encoding: Option<(property::Handle, HashMap<String, u64>)>,
    pub color_range: Option<(property::Handle, HashMap<String, u64>)>,
    pub in_fence_fd: Option<property::Handle>,
}

/// Everything discovery learned about one plane.
pub(super) struct Discovered {
    pub info: PlaneInfo,
    pub props: ExtraProps,
}

/// A property's flags, range values and enum `(value, name)` pairs.
struct RawProp {
    flags: u32,
    values: Vec<u64>,
    enums: Vec<(u64, String)>,
}

fn raw_prop(card: &Card<'_>, id: property::Handle) -> Option<RawProp> {
    let mut values = Vec::new();
    let mut enums = Vec::new();
    let p =
        drm_ffi::mode::get_property(card.as_fd(), id.into(), Some(&mut values), Some(&mut enums))
            .ok()?;
    let enums = enums
        .iter()
        .map(|e| {
            let bytes: Vec<u8> = e
                .name
                .iter()
                .map(|&c| c.cast_unsigned())
                .take_while(|&b| b != 0)
                .collect();
            (e.value, String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect();
    Some(RawProp {
        flags: p.flags,
        values,
        enums,
    })
}

fn enum_prop(
    card: &Card<'_>,
    map: &PropMap,
    name: &str,
) -> Option<(property::Handle, HashMap<String, u64>)> {
    let (h, _) = map.get(name)?;
    let raw = raw_prop(card, *h)?;
    Some((*h, raw.enums.into_iter().map(|(v, n)| (n, v)).collect()))
}

fn sorted_names(m: &HashMap<String, u64>) -> Vec<String> {
    let mut v: Vec<(&String, &u64)> = m.iter().collect();
    v.sort_by_key(|(_, val)| **val);
    v.into_iter().map(|(n, _)| n.clone()).collect()
}

/// Learn what plane `p` can do. Never fails: a property that cannot be
/// read is reported as absent, so a quirky plane costs detail, not the
/// backend.
pub(super) fn discover(
    card: &Card<'_>,
    res: &ResourceHandles,
    p: plane::Handle,
    map: &PropMap,
) -> Discovered {
    let info = card.get_plane(p).ok();
    let crtc_mask = info
        .as_ref()
        .map_or(0, |i| super::crtc_mask(res, i.possible_crtcs()));
    let kind = match map.get("type").map(|(_, v)| *v) {
        Some(1) => PlaneKind::Primary,
        Some(2) => PlaneKind::Cursor,
        _ => PlaneKind::Overlay,
    };

    let formats = map
        .get("IN_FORMATS")
        .and_then(|(_, blob)| card.get_property_blob(*blob).ok())
        .and_then(|bytes| parse_in_formats(&bytes).ok())
        .unwrap_or_else(|| {
            info.as_ref().map_or_else(Vec::new, |i| {
                i.formats()
                    .iter()
                    .map(|&f| (Fourcc(f), vec![MOD_LINEAR]))
                    .collect()
            })
        });

    let mut props = ExtraProps::default();
    let zpos = map.get("zpos").and_then(|(h, cur)| {
        let raw = raw_prop(card, *h)?;
        props.zpos = Some(*h);
        let immutable = raw.flags & drm_ffi::DRM_MODE_PROP_IMMUTABLE != 0;
        props.zpos_immutable = immutable;
        let (min, max) = match raw.values.as_slice() {
            [a, b, ..] => (*a, *b),
            _ => (*cur, *cur),
        };
        Some(Zpos {
            current: *cur,
            min,
            max,
            immutable,
        })
    });

    let rotations = map
        .get("rotation")
        .and_then(|(h, _)| {
            let raw = raw_prop(card, *h)?;
            props.rotation = Some(*h);
            // A bitmask property's enum values are bit *indices*.
            Some(
                raw.enums
                    .iter()
                    .filter(|(v, _)| *v < 32)
                    .fold(0u32, |m, (v, _)| m | (1 << v)),
            )
        })
        .unwrap_or(0);

    props.color_encoding = enum_prop(card, map, "COLOR_ENCODING");
    props.color_range = enum_prop(card, map, "COLOR_RANGE");
    let blend_modes = enum_prop(card, map, "pixel blend mode")
        .map(|(_, m)| sorted_names(&m))
        .unwrap_or_default();
    props.in_fence_fd = map.get("IN_FENCE_FD").map(|(h, _)| *h);

    Discovered {
        info: PlaneInfo {
            id: PlaneId(p.into()),
            kind,
            crtc_mask,
            formats,
            zpos,
            rotations,
            color_encodings: props
                .color_encoding
                .as_ref()
                .map(|(_, m)| sorted_names(m))
                .unwrap_or_default(),
            color_ranges: props
                .color_range
                .as_ref()
                .map(|(_, m)| sorted_names(m))
                .unwrap_or_default(),
            blend_modes,
            alpha: map.contains_key("alpha"),
            damage_clips: map.contains_key("FB_DAMAGE_CLIPS"),
            in_fence: props.in_fence_fd.is_some(),
            scaling: None,
        },
        props,
    }
}

/// Add `a`'s optional properties (zpos, rotation, `COLOR_*`, in-fence)
/// for plane `p` to `req`. `false` when the plane cannot take one of them
/// (no such property, an unknown enum value, or an immutable zpos asked
/// to move): the kernel would say `EINVAL`, so the caller says it
/// without a round trip.
pub(super) fn add_optional(
    req: &mut AtomicModeReq,
    p: plane::Handle,
    disc: &Discovered,
    a: &PlaneAssignment<'_>,
) -> bool {
    use std::os::fd::AsRawFd as _;
    let props = &disc.props;
    if let Some(z) = a.zpos {
        match (props.zpos, disc.info.zpos) {
            (Some(h), Some(zp)) if !zp.immutable => {
                req.add_property(p, h, property::Value::UnsignedRange(z));
            }
            (Some(_), Some(zp)) if zp.current == z => {}
            _ => return false,
        }
    }
    if let Some(r) = a.rotation {
        match props.rotation {
            Some(h) => req.add_property(p, h, property::Value::Bitmask(r.into())),
            None if r == rotation::ROTATE_0 => {}
            None => return false,
        }
    }
    let lookup = |e: &Option<(property::Handle, HashMap<String, u64>)>, name: &str| {
        e.as_ref().and_then(|(h, m)| Some((*h, *m.get(name)?)))
    };
    if let Some(e) = a.color_encoding {
        let Some((h, v)) = lookup(&props.color_encoding, e.kernel_name()) else {
            return false;
        };
        req.add_property(p, h, property::Value::Unknown(v));
    }
    if let Some(r) = a.color_range {
        let Some((h, v)) = lookup(&props.color_range, r.kernel_name()) else {
            return false;
        };
        req.add_property(p, h, property::Value::Unknown(v));
    }
    if let Some(fd) = a.in_fence {
        let Some(h) = props.in_fence_fd else {
            return false;
        };
        // Only read by the kernel during the caller's ioctl, while the
        // borrow is alive.
        req.add_property(p, h, property::Value::SignedRange(fd.as_raw_fd().into()));
    }
    true
}

fn u32_at(b: &[u8], off: usize) -> Result<u32, &'static str> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or("IN_FORMATS blob truncated")
}

fn u64_at(b: &[u8], off: usize) -> Result<u64, &'static str> {
    Ok(u64::from(u32_at(b, off)?) | (u64::from(u32_at(b, off + 4)?) << 32))
}

/// Parse a `drm_format_modifier_blob` (the `IN_FORMATS` property):
///
/// ```text
/// header: u32 version, flags, count_formats, formats_offset,
///             count_modifiers, modifiers_offset
/// formats_offset:   u32 fourcc × count_formats
/// modifiers_offset: { u64 formats; u32 offset; u32 pad; u64 modifier } × count_modifiers
/// ```
///
/// Format *i* supports a modifier when `offset <= i < offset + 64` and
/// bit `i - offset` of its `formats` mask is set. Every read is
/// bounds-checked; a malformed blob is an error, never a panic.
///
/// # Errors
/// A truncated blob or an offset outside it.
pub(crate) fn parse_in_formats(b: &[u8]) -> Result<Vec<(Fourcc, Vec<u64>)>, &'static str> {
    let version = u32_at(b, 0)?;
    if version != 1 {
        return Err("IN_FORMATS blob: unknown version");
    }
    let count_formats = u32_at(b, 8)? as usize;
    let formats_offset = u32_at(b, 12)? as usize;
    let count_mods = u32_at(b, 16)? as usize;
    let mods_offset = u32_at(b, 20)? as usize;
    // Bound the counts by the blob size before allocating anything.
    if count_formats > b.len() / 4 || count_mods > b.len() / 24 {
        return Err("IN_FORMATS blob: count exceeds blob");
    }
    let mut out: Vec<(Fourcc, Vec<u64>)> = (0..count_formats)
        .map(|i| Ok((Fourcc(u32_at(b, formats_offset + 4 * i)?), Vec::new())))
        .collect::<Result<_, &'static str>>()?;
    for m in 0..count_mods {
        let base = mods_offset + 24 * m;
        let mask = u64_at(b, base)?;
        let offset = u32_at(b, base + 8)? as usize;
        let modifier = u64_at(b, base + 16)?;
        for bit in 0..64 {
            if mask & (1 << bit) != 0
                && let Some((_, mods)) = out.get_mut(offset + bit)
                && !mods.contains(&modifier)
            {
                mods.push(modifier);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// scanout buffers
// ---------------------------------------------------------------------------

/// A linear scanout buffer for `TEST_ONLY` layouts: one dumb buffer,
/// possibly holding several planes (NV12: Y then `CbCr`).
pub(super) struct ScanoutBuf {
    pub db: DumbBuffer,
    pub fb: framebuffer::Handle,
}

/// The `AddFB2` description of a dumb buffer holding `planes` planes at
/// `offsets`, all with the same pitch.
struct DumbPlanar<'a> {
    db: &'a DumbBuffer,
    format: DrmFourcc,
    size: (u32, u32),
    planes: usize,
    offsets: [u32; 4],
}

impl PlanarBuffer for DumbPlanar<'_> {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        let mut p = [0; 4];
        for x in p.iter_mut().take(self.planes) {
            *x = ::drm::buffer::Buffer::pitch(self.db);
        }
        p
    }
    fn handles(&self) -> [Option<::drm::buffer::Handle>; 4] {
        let mut h = [None; 4];
        for x in h.iter_mut().take(self.planes) {
            *x = Some(::drm::buffer::Buffer::handle(self.db));
        }
        h
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

impl ScanoutBuf {
    pub fn create(card: &Card<'_>, format: Fourcc, width: u32, height: u32) -> Result<Self, Error> {
        let drm_format =
            DrmFourcc::try_from(format.0).map_err(|_| Error::Unsupported("that pixel format"))?;
        // (dumb width, dumb height, bpp, planes)
        let (dw, dh, bpp, planes) = match format {
            Fourcc::XRGB8888 | Fourcc::ARGB8888 | Fourcc::XBGR8888 => (width, height, 32, 1),
            Fourcc::YUYV | Fourcc::UYVY => (width, height, 16, 1),
            Fourcc::NV12 => {
                if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
                    return Err(Error::Unsupported("odd NV12 sizes"));
                }
                (width, height + height / 2, 8, 2)
            }
            _ => return Err(Error::Unsupported("that pixel format for scanout buffers")),
        };
        let db = card
            .create_dumb_buffer((dw, dh), drm_format, bpp)
            .map_err(Error::io("create dumb buffer"))?;
        let pitch = ::drm::buffer::Buffer::pitch(&db);
        let offsets = if planes == 2 {
            [0, pitch * height, 0, 0]
        } else {
            [0; 4]
        };
        let desc = DumbPlanar {
            db: &db,
            format: drm_format,
            size: (width, height),
            planes,
            offsets,
        };
        match card.add_planar_framebuffer(&desc, FbCmd2Flags::empty()) {
            Ok(fb) => Ok(Self { db, fb }),
            Err(e) => {
                let _ = card.destroy_dumb_buffer(db);
                Err(Error::Io {
                    op: "add framebuffer",
                    source: e,
                })
            }
        }
    }

    pub fn destroy(self, card: &Card<'_>) {
        let _ = card.destroy_framebuffer(self.fb);
        let _ = card.destroy_dumb_buffer(self.db);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(formats: &[u32], mods: &[(u64, u32, u64)]) -> Vec<u8> {
        let fo = 24u32;
        let mo = fo + 4 * formats.len() as u32;
        let mut b = Vec::new();
        for v in [1, 0, formats.len() as u32, fo, mods.len() as u32, mo] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for f in formats {
            b.extend_from_slice(&f.to_le_bytes());
        }
        for &(mask, off, m) in mods {
            b.extend_from_slice(&mask.to_le_bytes());
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&m.to_le_bytes());
        }
        b
    }

    #[test]
    fn in_formats_maps_modifiers_to_formats() {
        let x_tiled = (1u64 << 56) | 1;
        let b = blob(
            &[Fourcc::XRGB8888.0, Fourcc::ARGB8888.0, Fourcc::NV12.0],
            &[(0b111, 0, MOD_LINEAR), (0b011, 0, x_tiled)],
        );
        let f = parse_in_formats(&b).unwrap();
        assert_eq!(
            f,
            vec![
                (Fourcc::XRGB8888, vec![MOD_LINEAR, x_tiled]),
                (Fourcc::ARGB8888, vec![MOD_LINEAR, x_tiled]),
                (Fourcc::NV12, vec![MOD_LINEAR]),
            ]
        );
    }

    #[test]
    fn in_formats_honours_the_mask_offset() {
        let b = blob(&[1, 2, 3], &[(0b1, 2, 7)]);
        let f = parse_in_formats(&b).unwrap();
        assert_eq!(f[2].1, vec![7]);
        assert!(f[0].1.is_empty() && f[1].1.is_empty());
    }

    #[test]
    fn malformed_in_formats_is_an_error_not_a_panic() {
        let good = blob(&[1, 2], &[(0b11, 0, 0)]);
        for cut in 0..good.len() {
            assert!(parse_in_formats(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut bad = good.clone();
        bad[12..16].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(parse_in_formats(&bad).is_err());
        let mut huge = good.clone();
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_in_formats(&huge).is_err());
        let mut v2 = good;
        v2[0] = 2;
        assert!(parse_in_formats(&v2).is_err());
    }
}
