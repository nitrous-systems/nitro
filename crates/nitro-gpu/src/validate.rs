//! Layer-list and request validation.
//!
//! The helper trusts nothing the socket says. Every request is checked
//! here before a backend sees it, and every failure is a typed
//! [`BackendError`] that becomes an `Error` reply; nothing in this module
//! panics on any input.

use nitro_core::IRect;

use crate::backend::{BackendError, RingRequest};
use crate::proto::{
    AR24, Composite, DeviceInfo, DmabufDesc, ErrorCode, Layer, MAX_EDGE, MAX_LAYERS, MAX_RING,
    MOD_INVALID, ShadowDesc, XR24, plane_count,
};

/// What kind of buffer a texture came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexKind {
    /// A client dma-buf.
    Dmabuf,
    /// The shadow memfd.
    Shadow,
}

/// What validation needs to know about a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TexInfo {
    /// Kind.
    pub kind: TexKind,
    /// Width in texels.
    pub w: u32,
    /// Height in texels.
    pub h: u32,
    /// Fourcc.
    pub fourcc: u32,
}

/// What validation needs to know about the output ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutInfo {
    /// Slot count.
    pub n: usize,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

fn err(code: ErrorCode, msg: impl Into<String>) -> BackendError {
    BackendError::with_code(code, msg)
}

fn edge_ok(v: u32) -> bool {
    (1..=MAX_EDGE).contains(&v)
}

/// A dma-buf import.
///
/// # Errors
/// Bad size, unknown fourcc, plane count mismatch, a zero pitch, or a
/// fourcc/modifier pair the device did not advertise as sampleable.
pub fn dmabuf(d: &DmabufDesc, info: &DeviceInfo) -> Result<TexInfo, BackendError> {
    if !edge_ok(d.w) || !edge_ok(d.h) {
        return Err(err(ErrorCode::BadRect, format!("size {}x{}", d.w, d.h)));
    }
    let Some(planes) = plane_count(d.fourcc) else {
        return Err(err(
            ErrorCode::BadFormat,
            format!("fourcc {:#010x}", d.fourcc),
        ));
    };
    if d.planes.len() != planes {
        return Err(err(
            ErrorCode::BadFormat,
            format!("{} planes, format has {planes}", d.planes.len()),
        ));
    }
    if d.planes.iter().any(|p| p.pitch == 0) {
        return Err(err(ErrorCode::BadFormat, "zero pitch"));
    }
    if d.modifier == MOD_INVALID
        || !info
            .sampleable
            .iter()
            .any(|f| f.fourcc == d.fourcc && f.modifier == d.modifier)
    {
        return Err(err(
            ErrorCode::BadFormat,
            format!("{:#010x}/{:#x} not sampleable", d.fourcc, d.modifier),
        ));
    }
    Ok(TexInfo {
        kind: TexKind::Dmabuf,
        w: d.w,
        h: d.h,
        fourcc: d.fourcc,
    })
}

/// A shadow import; `file_len` is the memfd's (sealed) size.
///
/// # Errors
/// Bad size, format or stride, or a file too small for `stride * h`.
pub fn shadow(d: &ShadowDesc, file_len: u64) -> Result<TexInfo, BackendError> {
    if !edge_ok(d.w) || !edge_ok(d.h) {
        return Err(err(ErrorCode::BadRect, format!("size {}x{}", d.w, d.h)));
    }
    if d.fourcc != XR24 && d.fourcc != AR24 {
        return Err(err(
            ErrorCode::BadFormat,
            format!("shadow fourcc {:#010x}", d.fourcc),
        ));
    }
    if u64::from(d.stride) < u64::from(d.w) * 4 || !d.stride.is_multiple_of(4) {
        return Err(err(ErrorCode::BadFormat, format!("stride {}", d.stride)));
    }
    if u64::from(d.stride) * u64::from(d.h) > file_len {
        return Err(err(
            ErrorCode::BadBuffer,
            format!("memfd {file_len} bytes < {}x{}", d.stride, d.h),
        ));
    }
    Ok(TexInfo {
        kind: TexKind::Shadow,
        w: d.w,
        h: d.h,
        fourcc: d.fourcc,
    })
}

/// Damage rects for a shadow upload: kept if non-empty, clipped to the
/// texture.
///
/// # Errors
/// The texture is not a shadow.
pub fn upload(tex: TexInfo, rects: &[IRect]) -> Result<Vec<IRect>, BackendError> {
    if tex.kind != TexKind::Shadow {
        return Err(err(ErrorCode::BadId, "not a shadow texture"));
    }
    let bounds = IRect::new(0, 0, crate::px(tex.w), crate::px(tex.h));
    Ok(rects
        .iter()
        .map(|r| r.intersect(&bounds))
        .filter(|r| !r.is_empty())
        .collect())
}

/// A ring allocation: the modifier list is filtered to what the device
/// can render to, in the request's order.
///
/// # Errors
/// Bad slot count, size or format, or no usable modifier.
pub fn ring(
    n: u32,
    w: u32,
    h: u32,
    fourcc: u32,
    modifiers: &[u64],
    info: &DeviceInfo,
) -> Result<RingRequest, BackendError> {
    if n == 0 || n as usize > MAX_RING {
        return Err(err(ErrorCode::TooMany, format!("{n} slots")));
    }
    if !edge_ok(w) || !edge_ok(h) {
        return Err(err(ErrorCode::BadRect, format!("size {w}x{h}")));
    }
    if fourcc != XR24 && fourcc != AR24 {
        return Err(err(
            ErrorCode::BadFormat,
            format!("ring fourcc {fourcc:#010x}"),
        ));
    }
    let usable: Vec<u64> = modifiers
        .iter()
        .copied()
        .filter(|m| {
            info.render
                .iter()
                .any(|f| f.fourcc == fourcc && f.modifier == *m)
        })
        .collect();
    if usable.is_empty() {
        return Err(err(
            ErrorCode::BadFormat,
            "no renderable modifier in the list",
        ));
    }
    Ok(RingRequest {
        n: n as usize,
        w,
        h,
        fourcc,
        modifiers: usable,
    })
}

/// A frame. `tex` looks up a live texture id.
#[allow(clippy::many_single_char_names)] // x, y, w, h of a rect
///
/// # Errors
/// No ring or bad slot, an unknown texture, an empty or out-of-bounds
/// destination, a source rect outside its texture or not finite.
pub fn composite(
    c: &Composite,
    tex: impl Fn(u32) -> Option<TexInfo>,
    out: Option<OutInfo>,
) -> Result<(), BackendError> {
    let Some(out) = out else {
        return Err(err(ErrorCode::NoRing, "no output ring"));
    };
    if c.out_idx as usize >= out.n {
        return Err(err(
            ErrorCode::NoRing,
            format!("slot {} of {}", c.out_idx, out.n),
        ));
    }
    if c.fence_mask >> c.layers.len() != 0 {
        return Err(err(ErrorCode::Fences, "fence bit without a layer"));
    }
    layers(&c.layers, &tex, out.w, out.h)
}

/// A capture (#3962): a `w`×`h` temporary target and its layers, checked
/// as a frame's (no ring involved).
///
/// # Errors
/// A bad size, too many layers, an unknown texture, an empty or
/// out-of-bounds destination, a source rect outside its texture.
pub fn capture(
    w: u32,
    h: u32,
    ls: &[Layer],
    tex: impl Fn(u32) -> Option<TexInfo>,
) -> Result<(), BackendError> {
    if !edge_ok(w) || !edge_ok(h) {
        return Err(err(ErrorCode::BadRect, format!("size {w}x{h}")));
    }
    if ls.len() > MAX_LAYERS {
        return Err(err(ErrorCode::TooMany, format!("{} layers", ls.len())));
    }
    layers(ls, &tex, w, h)
}

#[allow(clippy::many_single_char_names)] // x, y, w, h of a rect
fn layers(
    ls: &[Layer],
    tex: &impl Fn(u32) -> Option<TexInfo>,
    out_w: u32,
    out_h: u32,
) -> Result<(), BackendError> {
    let bounds = IRect::new(0, 0, crate::px(out_w), crate::px(out_h));
    for (i, l) in ls.iter().enumerate() {
        let Some(t) = tex(l.tex) else {
            return Err(err(
                ErrorCode::BadId,
                format!("layer {i}: texture {}", l.tex),
            ));
        };
        if l.dst.is_empty() || !bounds.contains_rect(&l.dst) {
            return Err(err(
                ErrorCode::BadRect,
                format!("layer {i}: dst {:?}", l.dst),
            ));
        }
        let [x, y, w, h] = l.src;
        let fits = [x, y, w, h].iter().all(|v| v.is_finite())
            && x >= 0.0
            && y >= 0.0
            && w > 0.0
            && h > 0.0
            && x + w <= t.w as f32
            && y + h <= t.h as f32;
        if !fits {
            return Err(err(
                ErrorCode::BadRect,
                format!("layer {i}: src {:?}", l.src),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{FormatMod, NV12, PlaneDesc};

    fn info() -> DeviceInfo {
        DeviceInfo {
            sampleable: vec![
                FormatMod {
                    fourcc: NV12,
                    modifier: 0,
                },
                FormatMod {
                    fourcc: XR24,
                    modifier: 0,
                },
            ],
            render: vec![FormatMod {
                fourcc: XR24,
                modifier: 0,
            }],
            ..DeviceInfo::default()
        }
    }

    fn nv12() -> DmabufDesc {
        DmabufDesc {
            id: 1,
            w: 64,
            h: 64,
            fourcc: NV12,
            modifier: 0,
            planes: vec![
                PlaneDesc {
                    offset: 0,
                    pitch: 64,
                },
                PlaneDesc {
                    offset: 4096,
                    pitch: 64,
                },
            ],
            ..DmabufDesc::default()
        }
    }

    #[test]
    fn dmabuf_checks() {
        assert!(dmabuf(&nv12(), &info()).is_ok());
        let mut d = nv12();
        d.planes.pop();
        assert_eq!(dmabuf(&d, &info()).unwrap_err().code, ErrorCode::BadFormat);
        let mut d = nv12();
        d.modifier = 7;
        assert_eq!(dmabuf(&d, &info()).unwrap_err().code, ErrorCode::BadFormat);
        let mut d = nv12();
        d.w = 0;
        assert_eq!(dmabuf(&d, &info()).unwrap_err().code, ErrorCode::BadRect);
        let mut d = nv12();
        d.fourcc = 0x1234;
        assert_eq!(dmabuf(&d, &info()).unwrap_err().code, ErrorCode::BadFormat);
    }

    #[test]
    fn shadow_checks() {
        let d = ShadowDesc {
            id: 1,
            w: 10,
            h: 10,
            stride: 40,
            fourcc: XR24,
        };
        assert!(shadow(&d, 400).is_ok());
        assert_eq!(shadow(&d, 399).unwrap_err().code, ErrorCode::BadBuffer);
        assert_eq!(
            shadow(&ShadowDesc { stride: 39, ..d }, 4000)
                .unwrap_err()
                .code,
            ErrorCode::BadFormat
        );
        assert_eq!(
            shadow(&ShadowDesc { fourcc: NV12, ..d }, 4000)
                .unwrap_err()
                .code,
            ErrorCode::BadFormat
        );
    }

    #[test]
    fn ring_filters_modifiers() {
        let r = ring(2, 100, 100, XR24, &[5, 0], &info()).unwrap();
        assert_eq!(r.modifiers, vec![0]);
        assert_eq!(
            ring(2, 100, 100, XR24, &[5], &info()).unwrap_err().code,
            ErrorCode::BadFormat
        );
        assert_eq!(
            ring(0, 100, 100, XR24, &[0], &info()).unwrap_err().code,
            ErrorCode::TooMany
        );
        assert_eq!(
            ring(5, 100, 100, XR24, &[0], &info()).unwrap_err().code,
            ErrorCode::TooMany
        );
    }

    #[test]
    fn composite_checks() {
        let t = |id| {
            (id == 1).then_some(TexInfo {
                kind: TexKind::Shadow,
                w: 10,
                h: 10,
                fourcc: XR24,
            })
        };
        let out = Some(OutInfo { n: 2, w: 20, h: 20 });
        let layer = Layer {
            tex: 1,
            src: [0.0, 0.0, 10.0, 10.0],
            dst: IRect::new(0, 0, 20, 20),
            ..Layer::default()
        };
        let c = Composite {
            layers: vec![layer],
            ..Composite::default()
        };
        assert!(composite(&c, t, out).is_ok());
        assert_eq!(composite(&c, t, None).unwrap_err().code, ErrorCode::NoRing);
        let bad = |l: Layer| Composite {
            layers: vec![l],
            ..Composite::default()
        };
        assert_eq!(
            composite(&bad(Layer { tex: 2, ..layer }), t, out)
                .unwrap_err()
                .code,
            ErrorCode::BadId
        );
        assert_eq!(
            composite(
                &bad(Layer {
                    dst: IRect::new(1, 0, 20, 20),
                    ..layer
                }),
                t,
                out
            )
            .unwrap_err()
            .code,
            ErrorCode::BadRect
        );
        assert_eq!(
            composite(
                &bad(Layer {
                    src: [0.5, 0.0, 10.0, 10.0],
                    ..layer
                }),
                t,
                out
            )
            .unwrap_err()
            .code,
            ErrorCode::BadRect
        );
        assert_eq!(
            composite(
                &bad(Layer {
                    src: [0.0, 0.0, f32::NAN, 10.0],
                    ..layer
                }),
                t,
                out
            )
            .unwrap_err()
            .code,
            ErrorCode::BadRect
        );
        let c = Composite {
            out_idx: 2,
            layers: vec![layer],
            ..Composite::default()
        };
        assert_eq!(composite(&c, t, out).unwrap_err().code, ErrorCode::NoRing);
    }

    #[test]
    fn capture_checks() {
        let t = |id| {
            (id == 1).then_some(TexInfo {
                kind: TexKind::Dmabuf,
                w: 10,
                h: 10,
                fourcc: NV12,
            })
        };
        let layer = Layer {
            tex: 1,
            src: [0.0, 0.0, 10.0, 10.0],
            dst: IRect::new(5, 5, 20, 20),
            ..Layer::default()
        };
        assert!(capture(40, 30, &[layer], t).is_ok());
        assert!(capture(40, 30, &[], t).is_ok());
        assert_eq!(
            capture(0, 30, &[layer], t).unwrap_err().code,
            ErrorCode::BadRect
        );
        assert_eq!(
            capture(20, 20, &[layer], t).unwrap_err().code,
            ErrorCode::BadRect
        );
        assert_eq!(
            capture(40, 30, &[Layer { tex: 9, ..layer }], t)
                .unwrap_err()
                .code,
            ErrorCode::BadId
        );
        let many = vec![layer; MAX_LAYERS + 1];
        assert_eq!(
            capture(40, 30, &many, t).unwrap_err().code,
            ErrorCode::TooMany
        );
    }
}
