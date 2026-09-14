//! Atomic KMS backend over an already-open DRM fd.
//!
//! Commit sequence (also in `README.md`):
//!
//! 1. `open`: enable `UNIVERSAL_PLANES` + `ATOMIC` client caps, cache the
//!    property ids of every connector / CRTC / plane, enumerate, then one
//!    blocking atomic commit with `ALLOW_MODESET` that sets **all** state:
//!    our connectors get `CRTC_ID`, our CRTCs get `MODE_ID` + `ACTIVE`,
//!    our primary planes get `FB_ID`/`CRTC_*`/`SRC_*`; every other
//!    connector, CRTC and primary plane is explicitly disabled so nothing
//!    inherited from fbcon or a previous master conflicts.
//! 2. `commit`: `NONBLOCK | PAGE_FLIP_EVENT` with the plane's `FB_ID` and,
//!    when the plane exposes it, an `FB_DAMAGE_CLIPS` blob.
//! 3. `dispatch`: page-flip events off the DRM fd become
//!    `Event::Flipped`; uevents off the netlink socket become
//!    `Event::Hotplug`.

pub mod select;

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use ::drm::buffer::{Buffer as _, DrmFourcc, DrmModifier, PlanarBuffer};
use ::drm::control::atomic::AtomicModeReq;
use ::drm::control::dumbbuffer::{DumbBuffer, DumbMapping};
use ::drm::control::{
    AtomicCommitFlags, Device as ControlDevice, FbCmd2Flags, Mode, ModeFlags, ModeTypeFlags,
    PlaneType, ResourceHandle, ResourceHandles, connector, crtc, framebuffer, plane, property,
};
use ::drm::{ClientCapability, Device as BasicDevice};

use crate::uevent::UeventSocket;
use crate::{BYTES_PER_PIXEL, Backend, BufferMut, Error, Event, Image, OutputId, OutputInfo, Rect};
use select::{Assignment, ConnectorCandidate, ModeCandidate, PlaneCandidate};

// ---------------------------------------------------------------------------
// fd plumbing
// ---------------------------------------------------------------------------

/// The DRM device fd handed to [`DrmBackend::open`].
///
/// `Owned` is closed when the backend drops. `Borrowed` is never closed:
/// use it when the seat (libseat) owns the fd and expects to close it
/// itself after the backend is gone. Either way the backend sets
/// `O_NONBLOCK` on the open file description, which the seat must not
/// mind (libseat never reads from it).
#[derive(Debug)]
pub enum DrmFd<'fd> {
    /// Closed on drop.
    Owned(OwnedFd),
    /// Left open on drop.
    Borrowed(BorrowedFd<'fd>),
}

impl From<OwnedFd> for DrmFd<'static> {
    fn from(fd: OwnedFd) -> Self {
        DrmFd::Owned(fd)
    }
}

impl<'fd> From<BorrowedFd<'fd>> for DrmFd<'fd> {
    fn from(fd: BorrowedFd<'fd>) -> Self {
        DrmFd::Borrowed(fd)
    }
}

impl AsFd for DrmFd<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            DrmFd::Owned(fd) => fd.as_fd(),
            DrmFd::Borrowed(fd) => fd.as_fd(),
        }
    }
}

/// The `drm` crate's device traits are implemented on this thin wrapper.
struct Card<'fd>(DrmFd<'fd>);

impl AsFd for Card<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl BasicDevice for Card<'_> {}
impl ControlDevice for Card<'_> {}

/// Options for [`DrmBackend::open`].
#[derive(Debug, Clone)]
pub struct DrmOptions {
    /// Open a kernel uevent socket for hotplug. Failure to open it is not
    /// fatal (see [`DrmBackend::hotplug_error`]).
    pub hotplug: bool,
}

impl Default for DrmOptions {
    fn default() -> Self {
        Self { hotplug: true }
    }
}

// ---------------------------------------------------------------------------
// cached property ids
// ---------------------------------------------------------------------------

struct ConnProps {
    crtc_id: property::Handle,
}

struct CrtcProps {
    mode_id: property::Handle,
    active: property::Handle,
}

struct PlaneProps {
    primary: bool,
    fb_id: property::Handle,
    crtc_id: property::Handle,
    src_x: property::Handle,
    src_y: property::Handle,
    src_w: property::Handle,
    src_h: property::Handle,
    crtc_x: property::Handle,
    crtc_y: property::Handle,
    crtc_w: property::Handle,
    crtc_h: property::Handle,
    fb_damage_clips: Option<property::Handle>,
}

/// Name → (handle, current raw value) for one KMS object.
type PropMap = HashMap<String, (property::Handle, property::RawValue)>;

fn props_of<H: ResourceHandle>(card: &Card<'_>, handle: H) -> Result<PropMap, Error> {
    let set = card
        .get_properties(handle)
        .map_err(Error::io("get object properties"))?;
    let mut map = HashMap::new();
    for (&id, &val) in set.iter() {
        let info = card.get_property(id).map_err(Error::io("get property"))?;
        map.insert(info.name().to_string_lossy().into_owned(), (id, val));
    }
    Ok(map)
}

fn need(
    map: &PropMap,
    object: &'static str,
    name: &'static str,
) -> Result<property::Handle, Error> {
    map.get(name)
        .map(|(h, _)| *h)
        .ok_or(Error::MissingProperty { object, name })
}

impl PlaneProps {
    fn from_map(map: &PropMap) -> Result<Self, Error> {
        let primary = map
            .get("type")
            .is_some_and(|(_, v)| *v == u64::from(PlaneType::Primary as u32));
        Ok(Self {
            primary,
            fb_id: need(map, "plane", "FB_ID")?,
            crtc_id: need(map, "plane", "CRTC_ID")?,
            src_x: need(map, "plane", "SRC_X")?,
            src_y: need(map, "plane", "SRC_Y")?,
            src_w: need(map, "plane", "SRC_W")?,
            src_h: need(map, "plane", "SRC_H")?,
            crtc_x: need(map, "plane", "CRTC_X")?,
            crtc_y: need(map, "plane", "CRTC_Y")?,
            crtc_w: need(map, "plane", "CRTC_W")?,
            crtc_h: need(map, "plane", "CRTC_H")?,
            fb_damage_clips: map.get("FB_DAMAGE_CLIPS").map(|(h, _)| *h),
        })
    }
}

// ---------------------------------------------------------------------------
// buffers and outputs
// ---------------------------------------------------------------------------

/// `AddFB2` wants a planar description; a dumb `XRGB8888` buffer is one
/// plane with a linear modifier.
struct SinglePlane(DumbBuffer);

impl PlanarBuffer for SinglePlane {
    fn size(&self) -> (u32, u32) {
        self.0.size()
    }
    fn format(&self) -> DrmFourcc {
        self.0.format()
    }
    fn modifier(&self) -> Option<DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.0.pitch(), 0, 0, 0]
    }
    fn handles(&self) -> [Option<::drm::buffer::Handle>; 4] {
        [Some(self.0.handle()), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [0; 4]
    }
}

/// One dumb buffer, its framebuffer object and a persistent mapping.
struct FrameBuf {
    db: DumbBuffer,
    fb: framebuffer::Handle,
    map: DumbMapping<'static>,
}

impl FrameBuf {
    fn create(card: &Card<'_>, width: u32, height: u32) -> Result<Self, Error> {
        let db = card
            .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
            .map_err(Error::io("create dumb buffer"))?;
        let fb = match card.add_planar_framebuffer(&SinglePlane(db), FbCmd2Flags::empty()) {
            Ok(fb) => fb,
            Err(e) => {
                let _ = card.destroy_dumb_buffer(db);
                return Err(Error::Io {
                    op: "add framebuffer",
                    source: e,
                });
            }
        };
        // `map_dumb_buffer` ties the mapping's lifetime to a `&mut
        // DumbBuffer`. We want the mapping to live as long as the output,
        // so we lease it from a leaked copy of the (Copy, 24-byte) handle
        // struct; the real handle is kept in `db` for cleanup. Outputs
        // come and go only on hotplug, so the leak is bounded and tiny.
        let leaked: &'static mut DumbBuffer = Box::leak(Box::new(db));
        let map = match card.map_dumb_buffer(leaked) {
            Ok(m) => m,
            Err(e) => {
                let _ = card.destroy_framebuffer(fb);
                let _ = card.destroy_dumb_buffer(db);
                return Err(Error::Io {
                    op: "map dumb buffer",
                    source: e,
                });
            }
        };
        Ok(Self { db, fb, map })
    }

    fn destroy(self, card: &Card<'_>) {
        let FrameBuf { db, fb, map } = self;
        drop(map);
        let _ = card.destroy_framebuffer(fb);
        let _ = card.destroy_dumb_buffer(db);
    }
}

struct Output {
    id: OutputId,
    info: OutputInfo,
    connector: connector::Handle,
    crtc: crtc::Handle,
    crtc_idx: usize,
    plane: plane::Handle,
    plane_idx: usize,
    mode: Mode,
    mode_blob: u64,
    bufs: [FrameBuf; 2],
    /// Index of the most recently committed buffer.
    front: usize,
    pending: bool,
    /// Template request for `commit`: plane `FB_ID` (+ damage), nothing
    /// else. Cloned per commit because `atomic_commit` takes it by value.
    flip_req: AtomicModeReq,
}

impl Output {
    fn back(&self) -> usize {
        1 - self.front
    }

    fn destroy(self, card: &Card<'_>) {
        let [a, b] = self.bufs;
        a.destroy(card);
        b.destroy(card);
        let _ = card.destroy_property_blob(self.mode_blob);
    }
}

fn mode_candidate(m: &Mode) -> ModeCandidate {
    let (w, h) = m.size();
    let flags = m.flags();
    let interlaced = flags.contains(ModeFlags::INTERLACE);
    ModeCandidate {
        width: u32::from(w),
        height: u32::from(h),
        refresh_mhz: select::refresh_millihertz(
            m.clock(),
            u32::from(m.hsync().2),
            u32::from(m.vsync().2),
            u32::from(m.vscan()),
            interlaced,
            flags.contains(ModeFlags::DBLSCAN),
        ),
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
        interlaced,
    }
}

fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

/// Bitmask of CRTC indices (into `res.crtcs()`) selected by `filter`.
fn crtc_mask(res: &ResourceHandles, filter: ::drm::control::CrtcListFilter) -> u32 {
    let allowed = res.filter_crtcs(filter);
    res.crtcs()
        .iter()
        .enumerate()
        .filter(|(_, c)| allowed.contains(c))
        .fold(0u32, |m, (i, _)| m | (1 << i))
}

/// A connected connector with everything needed to set it up.
struct Probed {
    info: connector::Info,
    name: String,
    mode: Mode,
    candidate: ConnectorCandidate,
}

// ---------------------------------------------------------------------------
// the backend
// ---------------------------------------------------------------------------

/// Atomic KMS backend. See the [module docs](self) for the commit
/// sequence and [`Backend`] for the contract.
pub struct DrmBackend<'fd> {
    card: Card<'fd>,
    res: ResourceHandles,
    planes: Vec<plane::Handle>,
    conn_props: HashMap<u32, ConnProps>,
    crtc_props: HashMap<u32, CrtcProps>,
    plane_props: HashMap<u32, PlaneProps>,
    outputs: Vec<Output>,
    infos: Vec<OutputInfo>,
    next_id: u32,
    paused: bool,
    uevent: Option<UeventSocket>,
    hotplug_error: Option<io::Error>,
    damage_scratch: Vec<i32>,
}

impl<'fd> DrmBackend<'fd> {
    /// Take control of a DRM device: enable atomic + universal planes,
    /// enumerate connected outputs, allocate two dumb buffers each and
    /// perform the initial modeset. The caller must hold DRM master (be
    /// root with no other master, or have the fd from a seat).
    ///
    /// # Errors
    /// [`Error::Unsupported`] when the driver lacks atomic modesetting,
    /// [`Error::MissingProperty`] for a KMS object without the standard
    /// atomic properties, [`Error::Io`] for anything the kernel refuses.
    /// A connected connector that cannot get a CRTC is skipped, not an
    /// error; having zero outputs is not an error either (wait for
    /// hotplug).
    pub fn open(fd: impl Into<DrmFd<'fd>>, opts: &DrmOptions) -> Result<Self, Error> {
        let card = Card(fd.into());
        set_nonblocking(card.as_fd()).map_err(Error::io("set O_NONBLOCK on DRM fd"))?;
        card.set_client_capability(ClientCapability::UniversalPlanes, true)
            .map_err(|_| Error::Unsupported("universal planes"))?;
        card.set_client_capability(ClientCapability::Atomic, true)
            .map_err(|_| Error::Unsupported("atomic modesetting"))?;

        let (uevent, hotplug_error) = if opts.hotplug {
            match UeventSocket::open() {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };

        // `ResourceHandles` has private fields; seed with a real query,
        // `rescan` refreshes it anyway.
        let res = card
            .resource_handles()
            .map_err(Error::io("get resources"))?;
        let mut this = Self {
            card,
            res,
            planes: Vec::new(),
            conn_props: HashMap::new(),
            crtc_props: HashMap::new(),
            plane_props: HashMap::new(),
            outputs: Vec::new(),
            infos: Vec::new(),
            next_id: 1,
            paused: false,
            uevent,
            hotplug_error,
            damage_scratch: Vec::new(),
        };
        this.rescan()?;
        Ok(this)
    }

    /// Why hotplug detection is unavailable, if it is. The backend still
    /// works; call [`Backend::rescan`] from an external trigger instead.
    #[must_use]
    pub fn hotplug_error(&self) -> Option<&io::Error> {
        self.hotplug_error.as_ref()
    }

    /// The DRM fd.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.card.as_fd()
    }

    // -- enumeration --------------------------------------------------------

    /// Refresh the resource lists and the property caches. Cheap enough
    /// to do on every rescan; objects can appear (DP MST).
    fn refresh_resources(&mut self) -> Result<(), Error> {
        self.res = self
            .card
            .resource_handles()
            .map_err(Error::io("get resources"))?;
        self.planes = self
            .card
            .plane_handles()
            .map_err(Error::io("get plane resources"))?;

        self.conn_props.clear();
        for &c in self.res.connectors() {
            let map = props_of(&self.card, c)?;
            self.conn_props.insert(
                c.into(),
                ConnProps {
                    crtc_id: need(&map, "connector", "CRTC_ID")?,
                },
            );
        }
        self.crtc_props.clear();
        for &c in self.res.crtcs() {
            let map = props_of(&self.card, c)?;
            self.crtc_props.insert(
                c.into(),
                CrtcProps {
                    mode_id: need(&map, "crtc", "MODE_ID")?,
                    active: need(&map, "crtc", "ACTIVE")?,
                },
            );
        }
        self.plane_props.clear();
        for &p in &self.planes {
            let map = props_of(&self.card, p)?;
            self.plane_props
                .insert(p.into(), PlaneProps::from_map(&map)?);
        }
        Ok(())
    }

    /// Every connected connector with a usable mode.
    fn probe_connectors(&self) -> Result<Vec<Probed>, Error> {
        let mut out = Vec::new();
        for &handle in self.res.connectors() {
            let info = self
                .card
                .get_connector(handle, true)
                .map_err(Error::io("get connector"))?;
            if info.state() != connector::State::Connected {
                continue;
            }
            let cands: Vec<ModeCandidate> = info.modes().iter().map(mode_candidate).collect();
            let Some(mi) = select::select_mode(&cands) else {
                continue;
            };
            let mut mask = 0u32;
            let mut current = None;
            for &enc in info.encoders() {
                if let Ok(e) = self.card.get_encoder(enc) {
                    mask |= crtc_mask(&self.res, e.possible_crtcs());
                    if Some(enc) == info.current_encoder() {
                        current = e
                            .crtc()
                            .and_then(|c| self.res.crtcs().iter().position(|&x| x == c));
                    }
                }
            }
            out.push(Probed {
                name: connector_name(&info),
                mode: info.modes()[mi],
                candidate: ConnectorCandidate {
                    crtc_mask: mask,
                    current_crtc: current,
                },
                info,
            });
        }
        Ok(out)
    }

    fn plane_candidates(&self) -> Result<Vec<PlaneCandidate>, Error> {
        let mut out = Vec::with_capacity(self.planes.len());
        for &p in &self.planes {
            let info = self.card.get_plane(p).map_err(Error::io("get plane"))?;
            out.push(PlaneCandidate {
                crtc_mask: crtc_mask(&self.res, info.possible_crtcs()),
                primary: self.plane_props.get(&p.into()).is_some_and(|pp| pp.primary),
            });
        }
        Ok(out)
    }

    fn create_output(&mut self, probed: &Probed, a: Assignment) -> Result<Output, Error> {
        let (w, h) = probed.mode.size();
        let (w, h) = (u32::from(w), u32::from(h));
        let crtc = self.res.crtcs()[a.crtc];
        let plane = self.planes[a.plane];
        let buf0 = FrameBuf::create(&self.card, w, h)?;
        let buf1 = match FrameBuf::create(&self.card, w, h) {
            Ok(b) => b,
            Err(e) => {
                buf0.destroy(&self.card);
                return Err(e);
            }
        };
        let mode_blob = match self.card.create_property_blob(&probed.mode) {
            Ok(property::Value::Blob(id)) => id,
            Ok(_) => unreachable!("create_property_blob returns Blob"),
            Err(e) => {
                buf0.destroy(&self.card);
                buf1.destroy(&self.card);
                return Err(Error::Io {
                    op: "create mode blob",
                    source: e,
                });
            }
        };
        let cand = mode_candidate(&probed.mode);
        let id = OutputId(self.next_id);
        self.next_id += 1;
        let mut flip_req = AtomicModeReq::new();
        let pp = &self.plane_props[&plane.into()];
        flip_req.add_property(plane, pp.fb_id, property::Value::Framebuffer(Some(buf0.fb)));
        Ok(Output {
            id,
            info: OutputInfo {
                id,
                name: probed.name.clone(),
                width: w,
                height: h,
                refresh_mhz: cand.refresh_mhz,
                phys_mm: probed.info.size().unwrap_or((0, 0)),
            },
            connector: probed.info.handle(),
            crtc,
            crtc_idx: a.crtc,
            plane,
            plane_idx: a.plane,
            mode: probed.mode,
            mode_blob,
            bufs: [buf0, buf1],
            front: 0,
            pending: false,
            flip_req,
        })
    }

    /// Diff the probed connectors against the current outputs. Returns
    /// whether anything changed.
    fn reconcile(&mut self, probed: &[Probed]) -> Result<bool, Error> {
        let mut changed = false;

        // 1. Drop outputs whose connector is gone or whose mode changed.
        let mut i = 0;
        while i < self.outputs.len() {
            let o = &self.outputs[i];
            let keep = probed.iter().any(|p| {
                p.info.handle() == o.connector
                    && p.mode == o.mode
                    && self.res.crtcs().get(o.crtc_idx) == Some(&o.crtc)
                    && self.planes.get(o.plane_idx) == Some(&o.plane)
            });
            if keep {
                i += 1;
            } else {
                let o = self.outputs.remove(i);
                o.destroy(&self.card);
                changed = true;
            }
        }

        // 2. Assign CRTC + plane to the new connectors around the kept ones.
        let used_crtcs = self.outputs.iter().fold(0u32, |m, o| m | (1 << o.crtc_idx));
        let mut used_planes = vec![false; self.planes.len()];
        for o in &self.outputs {
            used_planes[o.plane_idx] = true;
        }
        let fresh: Vec<&Probed> = probed
            .iter()
            .filter(|p| !self.outputs.iter().any(|o| o.connector == p.info.handle()))
            .collect();
        let conns: Vec<ConnectorCandidate> = fresh.iter().map(|p| p.candidate).collect();
        let planes = self.plane_candidates()?;
        let assigned = select::assign(&conns, &planes, used_crtcs, &used_planes);

        for (p, a) in fresh.iter().zip(assigned) {
            let Some(a) = a else {
                // Documented corner case: more connected outputs than
                // CRTCs. Skipped silently; a later rescan retries.
                continue;
            };
            let out = self.create_output(p, a)?;
            self.outputs.push(out);
            changed = true;
        }

        if changed {
            self.infos = self.outputs.iter().map(|o| o.info.clone()).collect();
        }
        Ok(changed)
    }

    // -- commits ------------------------------------------------------------

    /// One blocking `ALLOW_MODESET` commit describing the complete state.
    fn modeset_all(&mut self) -> Result<(), Error> {
        let mut req = AtomicModeReq::new();
        for &c in self.res.connectors() {
            let cp = &self.conn_props[&c.into()];
            let crtc = self
                .outputs
                .iter()
                .find(|o| o.connector == c)
                .map(|o| o.crtc);
            req.add_property(c, cp.crtc_id, property::Value::CRTC(crtc));
        }
        for &c in self.res.crtcs() {
            let cp = &self.crtc_props[&c.into()];
            let out = self.outputs.iter().find(|o| o.crtc == c);
            req.add_property(
                c,
                cp.mode_id,
                property::Value::Blob(out.map_or(0, |o| o.mode_blob)),
            );
            req.add_property(c, cp.active, property::Value::Boolean(out.is_some()));
        }
        for &p in &self.planes {
            let pp = &self.plane_props[&p.into()];
            if !pp.primary {
                continue;
            }
            if let Some(o) = self.outputs.iter().find(|o| o.plane == p) {
                let (w, h) = (u64::from(o.info.width), u64::from(o.info.height));
                let fb = o.bufs[o.front].fb;
                req.add_property(p, pp.fb_id, property::Value::Framebuffer(Some(fb)));
                req.add_property(p, pp.crtc_id, property::Value::CRTC(Some(o.crtc)));
                req.add_property(p, pp.src_x, property::Value::UnsignedRange(0));
                req.add_property(p, pp.src_y, property::Value::UnsignedRange(0));
                req.add_property(p, pp.src_w, property::Value::UnsignedRange(w << 16));
                req.add_property(p, pp.src_h, property::Value::UnsignedRange(h << 16));
                req.add_property(p, pp.crtc_x, property::Value::SignedRange(0));
                req.add_property(p, pp.crtc_y, property::Value::SignedRange(0));
                req.add_property(p, pp.crtc_w, property::Value::UnsignedRange(w));
                req.add_property(p, pp.crtc_h, property::Value::UnsignedRange(h));
            } else {
                req.add_property(p, pp.fb_id, property::Value::Framebuffer(None));
                req.add_property(p, pp.crtc_id, property::Value::CRTC(None));
            }
        }
        self.card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .map_err(Error::io("atomic modeset"))
    }

    fn output_mut(&mut self, id: OutputId) -> Result<&mut Output, Error> {
        self.outputs
            .iter_mut()
            .find(|o| o.id == id)
            .ok_or(Error::NoSuchOutput(id))
    }

    fn output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.iter().find(|o| o.id == id)
    }

    /// Fill `damage_scratch` with `drm_mode_rect`s (x1, y1, x2, y2) for
    /// the given rects clipped to the output.
    fn build_damage(scratch: &mut Vec<i32>, damage: &[Rect], width: u32, height: u32) {
        scratch.clear();
        for r in damage {
            if let Some(c) = r.clipped_to(width, height) {
                scratch.extend_from_slice(&[
                    c.x,
                    c.y,
                    c.x + c.w.cast_signed(),
                    c.y + c.h.cast_signed(),
                ]);
            }
        }
    }
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

impl Backend for DrmBackend<'_> {
    fn outputs(&self) -> &[OutputInfo] {
        &self.infos
    }

    fn back_buffer(&mut self, output: OutputId) -> Result<BufferMut<'_>, Error> {
        let o = self.output_mut(output)?;
        if o.pending {
            return Err(Error::FlipPending(output));
        }
        let back = o.back();
        let (w, h) = (o.info.width, o.info.height);
        let buf = &mut o.bufs[back];
        Ok(BufferMut {
            width: w,
            height: h,
            stride: buf.db.pitch(),
            data: &mut buf.map,
        })
    }

    fn commit(&mut self, output: OutputId, damage: &[Rect]) -> Result<(), Error> {
        if self.paused {
            return Err(Error::Paused);
        }
        let idx = self
            .outputs
            .iter()
            .position(|o| o.id == output)
            .ok_or(Error::NoSuchOutput(output))?;
        let o = &mut self.outputs[idx];
        if o.pending {
            return Err(Error::FlipPending(output));
        }
        let pp = &self.plane_props[&o.plane.into()];
        let back = o.back();
        o.flip_req.add_property(
            o.plane,
            pp.fb_id,
            property::Value::Framebuffer(Some(o.bufs[back].fb)),
        );
        let mut blob = None;
        if let Some(clips) = pp.fb_damage_clips {
            Self::build_damage(
                &mut self.damage_scratch,
                damage,
                o.info.width,
                o.info.height,
            );
            let value = if self.damage_scratch.is_empty() {
                property::Value::Blob(0)
            } else {
                let v = self
                    .card
                    .create_property_blob(self.damage_scratch.as_slice())
                    .map_err(Error::io("create damage blob"))?;
                if let property::Value::Blob(id) = v {
                    blob = Some(id);
                }
                v
            };
            o.flip_req.add_property(o.plane, clips, value);
        }
        let result = self
            .card
            .atomic_commit(
                AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT,
                o.flip_req.clone(),
            )
            .map_err(Error::io("atomic page flip"));
        if let Some(id) = blob {
            // The kernel holds its own reference while the state is in use.
            let _ = self.card.destroy_property_blob(id);
        }
        result?;
        o.front = back;
        o.pending = true;
        Ok(())
    }

    fn flip_pending(&self, output: OutputId) -> bool {
        self.output(output).is_some_and(|o| o.pending)
    }

    fn poll_fds(&self) -> Vec<BorrowedFd<'_>> {
        let mut v = vec![self.card.as_fd()];
        if let Some(u) = &self.uevent {
            v.push(u.as_fd());
        }
        v
    }

    fn dispatch(&mut self, events: &mut Vec<Event>) -> Result<(), Error> {
        loop {
            let batch = match self.card.receive_events() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    return Err(Error::Io {
                        op: "read DRM events",
                        source: e,
                    });
                }
            };
            let mut any = false;
            for ev in batch {
                any = true;
                if let ::drm::control::Event::PageFlip(pf) = ev
                    && let Some(o) = self.outputs.iter_mut().find(|o| o.crtc == pf.crtc)
                {
                    o.pending = false;
                    events.push(Event::Flipped {
                        output: o.id,
                        sequence: u64::from(pf.frame),
                        time: pf.duration,
                    });
                }
            }
            if !any {
                break;
            }
        }
        if let Some(u) = &mut self.uevent
            && u.drain().map_err(Error::io("read uevent socket"))?
        {
            events.push(Event::Hotplug);
        }
        Ok(())
    }

    fn rescan(&mut self) -> Result<bool, Error> {
        self.refresh_resources()?;
        let probed = self.probe_connectors()?;
        let changed = self.reconcile(&probed)?;
        if changed && !self.paused {
            self.modeset_all()?;
        }
        Ok(changed)
    }

    fn pause(&mut self) {
        self.paused = true;
        // Nothing else is needed here: a flip in flight stays pending and, if
        // the kernel does deliver its completion event while we are away,
        // `dispatch` retires it in the usual way. Should it never arrive,
        // `resume` clears the flag unconditionally, so no state can be
        // stranded by pausing.
    }

    fn resume(&mut self) -> Result<(), Error> {
        self.paused = false;
        for o in &mut self.outputs {
            // Abandon any flip that was in flight when the session went away
            // instead of waiting for its completion event: DRM master was
            // revoked in between, and correctness should not depend on the
            // kernel still delivering page-flip events on the fd afterwards
            // (it does today, but that is not a promise we want to rely on).
            // Clearing the flag without swapping buffers is the right
            // bookkeeping, because `commit` already recorded the flipped-to
            // buffer as `front` optimistically and the modeset below scans
            // out exactly that buffer. The back buffer's contents are
            // therefore unknown from here on, which is harmless: the caller
            // repaints fully after a resume.
            o.pending = false;
        }
        let r = self.modeset_all();
        // The post-condition `Backend::resume` promises its caller, checked
        // rather than merely described: nothing is flip-pending, so the
        // server's next paint pass is never turned away by `FlipPending`
        // for a flip whose completion event may never arrive. A stale
        // event that *does* arrive afterwards finds no matching pending
        // output and retires nothing.
        debug_assert!(
            !self.outputs.iter().any(|o| o.pending),
            "resume must leave no output flip-pending"
        );
        r
    }

    fn read_front(&mut self, output: OutputId) -> Result<Image, Error> {
        let o = self.output(output).ok_or(Error::NoSuchOutput(output))?;
        let (w, h) = (o.info.width, o.info.height);
        let buf = &o.bufs[o.front];
        let pitch = buf.db.pitch() as usize;
        let row_bytes = (w * BYTES_PER_PIXEL) as usize;
        let mut data = Vec::with_capacity(row_bytes * h as usize);
        for y in 0..h as usize {
            data.extend_from_slice(&buf.map[y * pitch..y * pitch + row_bytes]);
        }
        Ok(Image {
            width: w,
            height: h,
            stride: w * BYTES_PER_PIXEL,
            data,
        })
    }
}

impl Drop for DrmBackend<'_> {
    fn drop(&mut self) {
        for o in self.outputs.drain(..) {
            o.destroy(&self.card);
        }
        // CRTC state is left as is: the kernel restores fbcon (or the next
        // master sets its own) when the fd's master status goes away.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_blob_layout_is_x1y1x2y2_clipped() {
        let mut scratch = Vec::new();
        DrmBackend::build_damage(
            &mut scratch,
            &[
                Rect::new(10, 20, 30, 40),
                Rect::new(-5, -5, 10, 10),
                Rect::new(500, 0, 1, 1),
            ],
            100,
            100,
        );
        assert_eq!(scratch, vec![10, 20, 40, 60, 0, 0, 5, 5]);
    }
}
