//! Client dma-buf Surfaces (#3918): import validation, the CPU-path
//! mapping, acquire fences and format/modifier feedback.
//!
//! A client registers a dma-buf it allocated (VA-API, a GPU, udmabuf)
//! with `CreateDmabufBuffer`, one fd per plane. What happens next depends
//! on the layout:
//!
//! - **Linear, a CPU-convertible format, one inode**: the server maps it
//!   read-only (`nitro_shm::Mapping::map_dmabuf`) and paints it like any
//!   shm Surface buffer (`blit_nv12`/`blit_yuyv`/XRGB).
//! - **Anything else** (tiled, compressed, split across buffers): it
//!   validates and is kept, but its store is not CPU-readable and paints
//!   as a documented placeholder (`frame::HOLE_PLACEHOLDER` grey,
//!   counted in `dmabuf_placeholder_paints`) unless the planes module
//!   (#3899) scans it out. When the output backend has planes, every
//!   dma-buf is also imported as a KMS framebuffer at commit
//!   (`Backend::import_buffer`, `AddFB2` with the modifier), kept in
//!   `HeldBuffer::scanout` for the buffer's life, and is a plane
//!   candidate like a server-allocated one; the planner pre-filters on
//!   each plane's `IN_FORMATS`. [`direct_scanout`] says whether any plane
//!   could take one, which sets `DIRECT_SCANOUT` (#3938).
//!
//! **Fences.** A frame is never latched — sampled or scanned out — before
//! its acquire fence signals, and the server never blocks on one: the
//! fence is `poll`ed once at receipt and otherwise waits in the epoll set
//! ([`FenceSet`]). The fence is the `sync_file` from
//! `PresentSurfaceFenced` (explicit sync), or a snapshot of the buffer's
//! write fences taken with `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` at
//! `PresentSurface` (implicit sync). On a kernel without that ioctl the
//! dma-buf fd itself is polled (a dma-buf is readable once its writers are
//! done), counted in `implicit_fence_fallbacks`. A frame of a Surface
//! placed on a plane with `IN_FENCE_FD` latches early instead (#3938):
//! its fence leaves the set ([`FenceSet::take`]) and goes to the display
//! with `Backend::set_plane_fence`, so the kernel waits, not the server.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use nitro_kms::{Fourcc, ImportDesc, PlaneInfo, PlaneKind};
use nitro_scene::{BufferDesc, PixelStore};
use nitro_shm::Mapping;
use nitro_wire::msg::CreateDmabufBuffer;
use nitro_wire::types::{DmabufFormat, ErrorCode, dmabuf_flags, format, modifier};
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::clients::{
    ApplyError, BufferBudget, SurfaceGeometry, check_budget, surface_map_len,
    validate_surface_geometry,
};

/// Largest width or height of a client dma-buf.
pub const MAX_DMABUF_DIM: u32 = 16_384;

/// Most acquire fences one client may have pending (`Limit` beyond).
pub const MAX_FENCES_PER_CLIENT: usize = 64;

/// The formats the CPU path converts, all linear only.
pub const CPU_FORMATS: [u32; 5] = [
    format::NV12,
    format::YUYV,
    format::UYVY,
    format::XR24,
    format::AR24,
];

/// A client dma-buf's pixels as the scene sees them: a read-only mapping
/// on the CPU path, nothing otherwise, and the first plane's fd for the
/// implicit fence either way.
#[derive(Debug)]
pub struct DmabufPixels {
    map: Option<Mapping>,
    /// Plane 0's descriptor, kept for the buffer's life: the implicit
    /// acquire fence is exported from it at every `PresentSurface`.
    fd: OwnedFd,
}

impl DmabufPixels {
    /// Whether the CPU path maps it.
    #[must_use]
    pub fn is_mapped(&self) -> bool {
        self.map.is_some()
    }
}

impl PixelStore for DmabufPixels {
    fn bytes(&self) -> &[u8] {
        self.map.as_ref().map_or(&[], Mapping::as_bytes)
    }

    fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        None
    }

    fn cpu_readable(&self) -> bool {
        self.map.is_some()
    }

    fn fence_fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.fd.as_fd())
    }

    /// A dma-buf's `AR24` is premultiplied, the Wayland/GPU convention
    /// (#3921): Chromium's GPU process presents its render output as is.
    fn premultiplied(&self) -> bool {
        true
    }
}

/// What the server imports into KMS at commit, when the output backend
/// has planes (the #3899 hook): the layout and a dup of each plane's fd.
#[derive(Debug)]
pub struct ImportRequest {
    /// The framebuffer layout.
    pub desc: ImportDesc,
    /// One fd per plane.
    pub fds: Vec<OwnedFd>,
}

/// A validated `CreateDmabufBuffer`, ready for `Pending::Dmabuf`.
#[derive(Debug)]
pub struct Validated {
    /// The scene's description (the real geometry either way).
    pub desc: BufferDesc,
    /// The store.
    pub pixels: DmabufPixels,
    /// The KMS import to try at commit.
    pub import: ImportRequest,
}

/// Planes a format needs at least. Unknown formats need one (and may
/// carry more: CCS auxiliary planes).
fn min_planes(fourcc: u32) -> usize {
    if fourcc == format::NV12 { 2 } else { 1 }
}

fn bad<T>(detail: String) -> Result<T, ApplyError> {
    Err(ApplyError::new(ErrorCode::BadBuffer, detail))
}

/// Check a `CreateDmabufBuffer` against `importable` (the default
/// feedback) and the buffer caps, and map it if it takes the CPU path.
///
/// # Errors
/// [`ErrorCode::BadBuffer`] (fatal) for every malformed request: plane
/// count, zero or huge sizes, a zero stride, `MOD_INVALID` or a pair not
/// listed as `IMPORT`, an fd that is neither a dma-buf nor a sealed memfd,
/// a plane outside its fd. [`ErrorCode::Limit`] past the buffer caps.
#[allow(clippy::too_many_lines)] // one linear list of checks
pub fn validate(
    m: CreateDmabufBuffer,
    importable: &[DmabufFormat],
    held: BufferBudget,
) -> Result<Validated, ApplyError> {
    let n = m.planes.len();
    if !(1..=4).contains(&n) {
        return bad(format!("CreateDmabufBuffer: {n} planes, not 1..=4"));
    }
    if n < min_planes(m.format) {
        return bad(format!(
            "CreateDmabufBuffer: format {:#010x} needs {} planes, got {n}",
            m.format,
            min_planes(m.format)
        ));
    }
    if m.width == 0 || m.height == 0 || m.width > MAX_DMABUF_DIM || m.height > MAX_DMABUF_DIM {
        return bad(format!(
            "CreateDmabufBuffer: size {}x{} is not 1..={MAX_DMABUF_DIM}",
            m.width, m.height
        ));
    }
    if m.planes.iter().any(|p| p.stride == 0) {
        return bad("CreateDmabufBuffer: a stride is 0".to_owned());
    }
    if m.modifier == modifier::INVALID {
        return bad("CreateDmabufBuffer: DRM_FORMAT_MOD_INVALID (implicit layout)".to_owned());
    }
    if !importable
        .iter()
        .any(|f| f.format == m.format && f.modifier == m.modifier)
    {
        return bad(format!(
            "CreateDmabufBuffer: format {:#010x} with modifier {} is not importable",
            m.format,
            nitro_kms::modifier_name(m.modifier)
        ));
    }
    let linear = m.modifier == modifier::LINEAR;
    let mut inodes = Vec::with_capacity(n);
    for (i, p) in m.planes.iter().enumerate() {
        if !nitro_shm::is_dmabuf(&p.fd) && nitro_shm::check_seals(p.fd.as_fd()).is_err() {
            return bad(format!(
                "CreateDmabufBuffer: plane {i}'s fd is neither a dma-buf nor a sealed memfd"
            ));
        }
        let st = rustix::fs::fstat(&p.fd)
            .map_err(|e| ApplyError::new(ErrorCode::BadBuffer, format!("fstat: {e}")))?;
        let size = u64::try_from(st.st_size).unwrap_or(0);
        let (off, stride) = (u64::from(p.offset), u64::from(p.stride));
        let fits = if linear {
            let (rows, row) = linear_plane(m.format, i, m.width, m.height);
            row <= stride && off + stride * (rows - 1) + row <= size
        } else {
            off < size
        };
        if !fits {
            return bad(format!(
                "CreateDmabufBuffer: plane {i} (offset {off}, stride {stride}) leaves its \
                 {size}-byte buffer"
            ));
        }
        inodes.push((st.st_dev, st.st_ino, size));
    }
    let one_inode = inodes
        .windows(2)
        .all(|w| w[0].0 == w[1].0 && w[0].1 == w[1].1);
    let cpu = linear && CPU_FORMATS.contains(&m.format) && one_inode;
    let p0 = &m.planes[0];
    let p1 = m.planes.get(1);
    let (desc, map_len) = if cpu {
        let geo = SurfaceGeometry {
            width: m.width,
            height: m.height,
            format: m.format,
            size: u32::try_from(inodes[0].2).unwrap_or(u32::MAX),
            offset0: p0.offset,
            stride0: p0.stride,
            offset1: if m.format == format::NV12 {
                p1.map_or(0, |p| p.offset)
            } else {
                0
            },
            stride1: if m.format == format::NV12 {
                p1.map_or(0, |p| p.stride)
            } else {
                0
            },
        };
        let desc = validate_surface_geometry(&geo)?;
        // The scene's length check is `stride × rows` (`byte_len`), which
        // with a padded pitch (GBM's, #3921) runs past the last row's
        // payload: map that much when the buffer has it, as
        // `AllocSurfaceBuffers` does with its export.
        let size = usize::try_from(inodes[0].2).unwrap_or(usize::MAX);
        (desc, surface_map_len(&geo).max(desc.byte_len().min(size)))
    } else {
        // Geometry only: nothing reads these bytes.
        let plane1 = (m.format == format::NV12)
            .then(|| p1.map(|p| (p.offset, p.stride.max(m.width), m.height.div_ceil(2))))
            .flatten();
        let desc = BufferDesc::new(m.width, m.height, p0.stride.max(m.width), m.format)
            .with_planes(p0.offset, plane1)
            .with_opaque(m.format != format::AR24);
        (desc, 0)
    };
    check_budget(map_len as u64, 1, held).map_err(|e| ApplyError::new(ErrorCode::Limit, e))?;
    let dup = |fd: &OwnedFd| {
        rustix::io::dup(fd).map_err(|e| ApplyError::new(ErrorCode::Limit, format!("dup: {e}")))
    };
    let map = if cpu {
        match Mapping::map_dmabuf(dup(&p0.fd)?, map_len) {
            Ok(m) => Some(m),
            Err(e) => {
                crate::warn!("dma-buf CPU mapping failed, showing a placeholder: {e}");
                None
            }
        }
    } else {
        None
    };
    let mut offsets = [0; 4];
    let mut pitches = [0; 4];
    for (i, p) in m.planes.iter().enumerate() {
        offsets[i] = p.offset;
        pitches[i] = p.stride;
    }
    let import = ImportRequest {
        desc: ImportDesc {
            format: Fourcc(m.format),
            width: m.width,
            height: m.height,
            modifier: m.modifier,
            #[allow(clippy::cast_possible_truncation)] // 1..=4
            planes: n as u8,
            offsets,
            pitches,
        },
        fds: m.planes.into_iter().map(|p| p.fd).collect(),
    };
    let fd = dup(&import.fds[0])?;
    Ok(Validated {
        desc,
        pixels: DmabufPixels { map, fd },
        import,
    })
}

/// `(rows, row payload bytes)` of plane `i` of a linear buffer. Unknown
/// formats are checked as one byte per pixel, the loosest bound.
fn linear_plane(fourcc: u32, i: usize, w: u32, h: u32) -> (u64, u64) {
    let (w, h) = (u64::from(w), u64::from(h));
    match (fourcc, i) {
        (format::NV12, 0) => (h, w),
        (format::NV12, _) => (h.div_ceil(2), 2 * w.div_ceil(2)),
        (format::YUYV | format::UYVY, _) => (h, 2 * w),
        (format::XR24 | format::AR24, _) => (h, 4 * w),
        _ => (h, w),
    }
}

/// Format/modifier feedback for one set of planes: every pair a
/// non-cursor plane lists (`SCANOUT`), plus the CPU formats linear
/// (`CPU`), all `IMPORT`. Sorted, one entry per pair.
#[must_use]
pub fn feedback(planes: &[PlaneInfo]) -> Vec<DmabufFormat> {
    let mut out: Vec<DmabufFormat> = CPU_FORMATS
        .iter()
        .map(|&f| DmabufFormat {
            format: f,
            modifier: modifier::LINEAR,
            flags: dmabuf_flags::CPU | dmabuf_flags::IMPORT,
        })
        .collect();
    for p in planes.iter().filter(|p| p.kind != PlaneKind::Cursor) {
        for (f, mods) in &p.formats {
            for &m in mods.iter().filter(|&&m| m != modifier::INVALID) {
                out.push(DmabufFormat {
                    format: f.0,
                    modifier: m,
                    flags: dmabuf_flags::SCANOUT | dmabuf_flags::IMPORT,
                });
            }
        }
    }
    merge(out)
}

/// Sort by (format, modifier) and OR the flags of equal pairs.
#[must_use]
pub fn merge(mut v: Vec<DmabufFormat>) -> Vec<DmabufFormat> {
    v.sort_by_key(|f| (f.format, f.modifier));
    let mut out: Vec<DmabufFormat> = Vec::with_capacity(v.len());
    for f in v {
        match out.last_mut() {
            Some(l) if l.format == f.format && l.modifier == f.modifier => l.flags |= f.flags,
            _ => out.push(f),
        }
    }
    out
}

/// Whether the planes module (#3899) can place a client dma-buf on these
/// planes at all (#3938): some non-cursor plane lists a real format and
/// modifier. Exactly the pairs [`feedback`] flags `SCANOUT`, so the
/// `DIRECT_SCANOUT` capability bit and the feedback agree.
#[must_use]
pub fn direct_scanout(planes: &[PlaneInfo]) -> bool {
    planes.iter().any(|p| {
        p.kind != PlaneKind::Cursor
            && p.formats
                .iter()
                .any(|(_, mods)| mods.iter().any(|&m| m != modifier::INVALID))
    })
}

/// A pending acquire fence's key; its epoll token is `base + key`.
pub type FenceKey = crate::surface::FenceKey;

/// Whether `fd` is readable now (a `sync_file` that has signalled),
/// without blocking. An error counts as ready: a broken fence must not
/// stall the surface forever, and there is nothing better to wait on.
#[must_use]
pub fn signalled(fd: BorrowedFd<'_>) -> bool {
    let mut p = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    match rustix::event::poll(&mut p, Some(&rustix::time::Timespec::default())) {
        Ok(0) => false,
        Ok(_) | Err(_) => true,
    }
}

/// The pending fences, each in the server's epoll under `base + key`.
#[derive(Debug)]
pub struct FenceSet {
    base: u64,
    next: FenceKey,
    pending: HashMap<FenceKey, (u64, OwnedFd)>,
}

impl FenceSet {
    /// An empty set whose epoll tokens start at `base`.
    #[must_use]
    pub fn new(base: u64) -> Self {
        Self {
            base,
            next: 0,
            pending: HashMap::new(),
        }
    }

    /// How many fences `token` has pending.
    #[must_use]
    pub fn count_for(&self, token: u64) -> usize {
        self.pending.values().filter(|(t, _)| *t == token).count()
    }

    /// How many are pending in all.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether none is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Register `fd` for connection `token`.
    ///
    /// # Errors
    /// The `epoll_ctl` errno.
    pub fn add(
        &mut self,
        epoll: &OwnedFd,
        token: u64,
        fd: OwnedFd,
    ) -> Result<FenceKey, rustix::io::Errno> {
        let key = self.next;
        self.next += 1;
        epoll::add(
            epoll,
            &fd,
            EventData::new_u64(self.base + key),
            EventFlags::IN,
        )?;
        self.pending.insert(key, (token, fd));
        Ok(key)
    }

    /// The key an epoll token names, if it is one of ours.
    #[must_use]
    pub fn key_of(&self, epoll_token: u64) -> Option<FenceKey> {
        epoll_token.checked_sub(self.base)
    }

    /// Deregister and close a fence (signalled, or its frame dropped).
    /// Unknown keys are ignored.
    pub fn remove(&mut self, epoll: &OwnedFd, key: FenceKey) {
        if let Some((_, fd)) = self.pending.remove(&key) {
            let _ = epoll::delete(epoll, &fd);
        }
    }

    /// Deregister a fence and hand its fd over instead of closing it: its
    /// frame latched early onto a plane and the display waits on it as
    /// `IN_FENCE_FD` (#3938). `None` for an unknown key.
    pub fn take(&mut self, epoll: &OwnedFd, key: FenceKey) -> Option<OwnedFd> {
        let (_, fd) = self.pending.remove(&key)?;
        let _ = epoll::delete(epoll, &fd);
        Some(fd)
    }

    /// Drop every fence `token` registered.
    pub fn forget_client(&mut self, epoll: &OwnedFd, token: u64) {
        let keys: Vec<FenceKey> = self
            .pending
            .iter()
            .filter(|(_, (t, _))| *t == token)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.remove(epoll, k);
        }
    }
}

/// Which output's feedback each tracked Surface node was last sent,
/// keyed by node and connection like `surface::Hints` (#3918).
#[derive(Debug, Default)]
pub struct FeedbackTracker {
    nodes: HashMap<(nitro_scene::NodeKey, u64), (nitro_wire::types::NodeId, Option<u64>)>,
}

/// A per-output feedback: `(output, width, height, formats)`.
pub type OutputFeedback = (nitro_scene::OutputId, u32, u32, Vec<DmabufFormat>);

impl FeedbackTracker {
    /// Track a Surface node for connection `token`, named `id` there.
    pub fn track(&mut self, node: nitro_scene::NodeKey, token: u64, id: nitro_wire::types::NodeId) {
        self.nodes.entry((node, token)).or_insert((id, None));
    }

    /// Stop tracking `node` for `token`.
    pub fn untrack(&mut self, node: nitro_scene::NodeKey, token: u64) {
        self.nodes.remove(&(node, token));
    }

    /// Forget a client's nodes.
    pub fn forget_client(&mut self, token: u64) {
        self.nodes.retain(|(_, t), _| *t != token);
    }

    /// Whether nothing is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Forget what was sent, so every node is sent its feedback again at
    /// the next [`changed`](Self::changed) (outputs changed).
    pub fn invalidate(&mut self) {
        for v in self.nodes.values_mut() {
            v.1 = None;
        }
    }

    /// The `(token, id, output)` of every node whose output, or whose
    /// output's feedback, differs from what it was last sent. Dead nodes
    /// are dropped; a node on no output sends nothing.
    pub fn changed(
        &mut self,
        scene: &nitro_scene::Scene,
        outputs: &[OutputFeedback],
    ) -> Vec<(u64, nitro_wire::types::NodeId, nitro_scene::OutputId)> {
        let mut out = Vec::new();
        self.nodes.retain(|(key, token), (id, sent)| {
            let Ok(node) = scene.node(*key) else {
                return false;
            };
            let Some(o) = scene
                .window_info(node.window())
                .ok()
                .and_then(nitro_scene::Window::output)
            else {
                return true;
            };
            let Some(fb) = outputs.iter().find(|f| f.0 == o) else {
                return true;
            };
            let now = Some(stamp(fb));
            if *sent != now {
                *sent = now;
                out.push((*token, *id, o));
            }
            true
        });
        out
    }
}

/// A cheap fingerprint of one output's feedback, to notice a change.
fn stamp(fb: &OutputFeedback) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fb.0.0.hash(&mut h);
    fb.1.hash(&mut h);
    fb.2.hash(&mut h);
    fb.3.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_kms::PlaneId;
    use nitro_wire::msg::DmabufPlane;
    use nitro_wire::types::BufferId;

    fn plane(kind: PlaneKind, formats: Vec<(Fourcc, Vec<u64>)>) -> PlaneInfo {
        PlaneInfo {
            id: PlaneId(1),
            kind,
            crtc_mask: 1,
            formats,
            zpos: None,
            rotations: 1,
            color_encodings: vec![],
            color_ranges: vec![],
            blend_modes: vec![],
            alpha: false,
            damage_clips: false,
            in_fence: false,
            scaling: None,
        }
    }

    fn planes() -> Vec<PlaneInfo> {
        vec![
            plane(
                PlaneKind::Primary,
                vec![
                    (
                        Fourcc::XRGB8888,
                        vec![modifier::LINEAR, modifier::I915_X_TILED],
                    ),
                    (Fourcc::ARGB8888, vec![modifier::LINEAR]),
                ],
            ),
            plane(
                PlaneKind::Overlay,
                vec![(Fourcc::NV12, vec![modifier::LINEAR, modifier::I915_Y_TILED])],
            ),
            plane(
                PlaneKind::Cursor,
                vec![(Fourcc(format::fourcc(b"AB24")), vec![modifier::LINEAR])],
            ),
        ]
    }

    #[test]
    fn feedback_merges_scanout_and_cpu() {
        use dmabuf_flags::{CPU, IMPORT, SCANOUT};
        let f = feedback(&planes());
        let get = |fmt: u32, m: u64| f.iter().find(|e| e.format == fmt && e.modifier == m);
        assert_eq!(get(format::NV12, 0).unwrap().flags, CPU | IMPORT | SCANOUT);
        assert_eq!(
            get(format::NV12, modifier::I915_Y_TILED).unwrap().flags,
            IMPORT | SCANOUT
        );
        assert_eq!(get(format::YUYV, 0).unwrap().flags, CPU | IMPORT);
        assert_eq!(
            get(format::XR24, modifier::I915_X_TILED).unwrap().flags,
            IMPORT | SCANOUT
        );
        assert!(get(format::fourcc(b"AB24"), 0).is_none(), "cursor planes");
        assert_eq!(f.len(), 7);
        let mut sorted = f.clone();
        sorted.sort();
        assert_eq!(
            f.iter().map(|e| (e.format, e.modifier)).collect::<Vec<_>>(),
            sorted
                .iter()
                .map(|e| (e.format, e.modifier))
                .collect::<Vec<_>>()
        );
        // No planes: the CPU formats alone.
        assert_eq!(feedback(&[]).len(), CPU_FORMATS.len());
    }

    fn msg(fmt: u32, m: u64, planes: Vec<(OwnedFd, u32, u32)>) -> CreateDmabufBuffer {
        CreateDmabufBuffer {
            id: BufferId(1),
            width: 16,
            height: 16,
            format: fmt,
            modifier: m,
            planes: planes
                .into_iter()
                .map(|(fd, offset, stride)| DmabufPlane { fd, offset, stride })
                .collect(),
        }
    }

    fn sealed(len: u64) -> OwnedFd {
        nitro_shm::create_sealed("t", len).unwrap()
    }

    fn dup(fd: &OwnedFd) -> OwnedFd {
        rustix::io::dup(fd).unwrap()
    }

    #[test]
    fn a_linear_nv12_memfd_maps_for_the_cpu_path() {
        let fd = sealed(16 * 24);
        let m = msg(format::NV12, 0, vec![(dup(&fd), 0, 16), (fd, 256, 16)]);
        let v = validate(m, &feedback(&planes()), BufferBudget::default()).unwrap();
        assert!(v.pixels.is_mapped());
        assert_eq!(v.import.fds.len(), 2);
        assert_eq!(v.desc.plane1, Some((256, 16, 8)));
    }

    #[test]
    fn a_tiled_buffer_validates_but_is_not_cpu_readable() {
        let fd = sealed(16 * 24);
        let m = msg(
            format::NV12,
            modifier::I915_Y_TILED,
            vec![(dup(&fd), 0, 128), (fd, 256, 128)],
        );
        let v = validate(m, &feedback(&planes()), BufferBudget::default()).unwrap();
        assert!(!v.pixels.cpu_readable());
        assert!(v.pixels.fence_fd().is_some());
    }

    #[test]
    fn malformed_imports_are_bad_buffer() {
        let fb = feedback(&planes());
        let refuse = |m: CreateDmabufBuffer| {
            let e = validate(m, &fb, BufferBudget::default()).unwrap_err();
            assert_eq!(e.code, ErrorCode::BadBuffer, "{}", e.detail);
        };
        // NV12 with one plane.
        refuse(msg(format::NV12, 0, vec![(sealed(4096), 0, 16)]));
        // No planes.
        refuse(msg(format::XR24, 0, vec![]));
        // Not importable: Y-tiled XR24 is listed by no plane.
        refuse(msg(
            format::XR24,
            modifier::I915_Y_TILED,
            vec![(sealed(4096), 0, 64)],
        ));
        refuse(msg(
            format::XR24,
            modifier::INVALID,
            vec![(sealed(4096), 0, 64)],
        ));
        // Unknown format.
        refuse(msg(format::fourcc(b"ZZZZ"), 0, vec![(sealed(4096), 0, 64)]));
        // Outside the fd.
        refuse(msg(format::XR24, 0, vec![(sealed(64 * 15), 0, 64)]));
        // Zero stride.
        refuse(msg(format::XR24, 0, vec![(sealed(4096), 0, 0)]));
        // A pipe, and an unsealed memfd.
        let (r, _w) = rustix::pipe::pipe().unwrap();
        refuse(msg(format::XR24, 0, vec![(r, 0, 64)]));
        let unsealed = rustix::fs::memfd_create("u", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&unsealed, 4096).unwrap();
        refuse(msg(format::XR24, 0, vec![(unsealed, 0, 64)]));
        // No pixels.
        let mut m = msg(format::XR24, 0, vec![(sealed(4096), 0, 64)]);
        m.width = 0;
        refuse(m);
    }

    #[test]
    fn direct_scanout_follows_the_planes() {
        assert!(!direct_scanout(&[]));
        let cursor = plane(
            PlaneKind::Cursor,
            vec![(Fourcc::ARGB8888, vec![modifier::LINEAR])],
        );
        assert!(!direct_scanout(std::slice::from_ref(&cursor)));
        let invalid = plane(
            PlaneKind::Overlay,
            vec![(Fourcc::NV12, vec![modifier::INVALID])],
        );
        assert!(!direct_scanout(&[cursor.clone(), invalid]));
        assert!(direct_scanout(&planes()));
    }

    #[test]
    fn a_taken_fence_leaves_the_set_open() {
        let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).unwrap();
        let mut set = FenceSet::new(100);
        let (r, w) = rustix::pipe::pipe().unwrap();
        let k = set.add(&epoll, 7, r).unwrap();
        assert_eq!(set.count_for(7), 1);
        let fd = set.take(&epoll, k).unwrap();
        assert!(set.is_empty());
        assert!(set.take(&epoll, k).is_none());
        // Still the same pipe, still open.
        rustix::io::write(&w, b"x").unwrap();
        assert!(signalled(fd.as_fd()));
    }

    #[test]
    fn fences_are_polled_without_blocking() {
        let (r, w) = rustix::pipe::pipe().unwrap();
        assert!(!signalled(r.as_fd()));
        rustix::io::write(&w, b"x").unwrap();
        assert!(signalled(r.as_fd()));
    }
}
