//! Hardware plane assignment (#3899): which visible Surfaces go straight
//! to a KMS plane instead of being composited on the CPU.
//!
//! A pure function of what is on screen and what the display engine
//! offers — no `Server`, no backend, only a `test` callback that asks the
//! kernel (`TEST_ONLY`) whether a layout would work. The server builds the
//! [`Candidate`]s from its paint list, calls [`Planner::decide`] before
//! each paint, and stages the [`Decision`]'s layout.
//!
//! Strategies, per output:
//!
//! 1. **Direct** (mode 3): one unobscured Surface covers the whole output.
//!    It goes on the primary, or on an overlay with the primary off (the
//!    Haswell shape: its primary takes no YUV). The output buffer is not
//!    shown at all.
//! 2. Greedy, topmost Surface first:
//!    - **overlay-above** (mode 1): an *unobscured* Surface on an overlay
//!      stacked above the primary, the UI on the primary. No hole needed:
//!      the Surface paints as a hole under its plane, which nobody sees.
//!    - **underlay by zpos** (mode 1): an overlay whose `zpos` is mutable
//!      moved below the primary; the UI scans out as `ARGB8888` with a
//!      transparent hole where the Surface shows through.
//!    - **underlay by primary swap** (mode 1): the Surface on the primary
//!      and the UI (with its hole) on an overlay above it — the Intel way
//!      with immutable `zpos` (Kaby Lake's measured case (c)).
//!
//!    Obscured Surfaces can only use the underlays.
//! 3. **Composite** (mode 0): the default layout, everything on the CPU.
//!
//! Every candidate plane is pre-filtered by `IN_FORMATS`, by the scaling
//! rule (downscale below [`MIN_SCALE_PCT`] never even reaches the
//! kernel: Kaby Lake refuses 0.75×, Haswell any scaling) and by the
//! plane's `COLOR_ENCODING`/`COLOR_RANGE` values, and only then tested.
//!
//! [`Planner`] adds a cache keyed by the layout's *shape* (not its buffer
//! ids, so every video frame of a steady layout is a hit and costs no
//! ioctl) and hysteresis: fewer planes applies at once, more planes only
//! after the same shape was asked for [`UPGRADE_FRAMES`] times over at
//! least [`UPGRADE_NS`] — so controls that autohide and reappear keep the
//! video composited rather than flapping between modes (each switch is a
//! full repaint).

use std::hash::{DefaultHasher, Hash, Hasher};

use nitro_core::IRect;
use nitro_kms::{
    BufferId, ColorEncoding, ColorRange, Fourcc, MOD_LINEAR, PlaneAssignment, PlaneConfig,
    PlaneId, PlaneInfo, PlaneKind, PlaneSource, Rect as KmsRect, SrcRect, Verdict,
};
use nitro_scene::{ColorMatrix, NodeKey, SurfaceColor};

/// Consecutive decisions a better (more planes) layout must be wanted
/// for before it is taken.
pub const UPGRADE_FRAMES: u32 = 15;
/// ... and the least time those decisions must span, in nanoseconds.
pub const UPGRADE_NS: u64 = 250_000_000;
/// Cached decisions per output.
pub const CACHE_ENTRIES: usize = 8;
/// Smallest `dst / src` ratio tested, in percent (Kaby Lake: 0.94×
/// ACCEPT, 0.75× REJECT).
pub const MIN_SCALE_PCT: u64 = 94;

/// What an output's planes are doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Everything on the CPU; the default layout.
    #[default]
    Composite,
    /// Surfaces on overlays above the UI.
    Overlay,
    /// At least one Surface below the UI, showing through a hole.
    Underlay,
    /// One Surface is the whole screen; the UI is not scanned out.
    Direct,
}

impl Mode {
    /// The number `docs/surfaces.md` gives the mode (0, 1 or 3; 2 is
    /// the GPU helper, not built).
    #[must_use]
    pub const fn number(self) -> u64 {
        match self {
            Mode::Composite => 0,
            Mode::Overlay | Mode::Underlay => 1,
            Mode::Direct => 3,
        }
    }
}

/// A visible Surface that could go on a plane: backed by a KMS
/// framebuffer, of an opaque format, drawn axis-aligned at full opacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The Surface node.
    pub node: NodeKey,
    /// Its current buffer's framebuffer.
    pub buffer: BufferId,
    /// The buffer's format.
    pub format: Fourcc,
    /// The buffer's modifier.
    pub modifier: u64,
    /// The part of the buffer that is visible, 16.16 (see [`crop`]).
    pub src: SrcRect,
    /// Where that part is, output-local device pixels, inside the output.
    pub dst: IRect,
    /// Some nitro content (another window, a popup, the cursor, ...) is
    /// painted above it inside `dst`.
    pub obscured: bool,
    /// Colour metadata.
    pub color: SurfaceColor,
}

/// Everything a decision is a function of, besides the kernel's answers.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The candidates, bottom to top.
    pub candidates: &'a [Candidate],
    /// The output's planes ([`nitro_kms::Backend::planes`]).
    pub planes: &'a [PlaneInfo],
    /// Output size in pixels.
    pub size: (u32, u32),
    /// The output buffer can scan out `ARGB8888`, which every underlay
    /// needs for its hole.
    pub alpha: bool,
}

/// A layout for one output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Decision {
    /// The mode.
    pub mode: Mode,
    /// The whole-CRTC layout to stage; empty is the default (composite).
    pub layout: Vec<PlaneConfig>,
    /// Surfaces on a plane, and which. They paint as holes.
    pub placed: Vec<(NodeKey, PlaneId)>,
    /// The subset of `placed` below the UI: their holes must be
    /// transparent, so the output scans out `ARGB8888`.
    pub underlays: Vec<NodeKey>,
}

impl Decision {
    /// Whether the output buffer must carry alpha.
    #[must_use]
    pub fn need_alpha(&self) -> bool {
        !self.underlays.is_empty()
    }

    /// Whether `node` is on a plane.
    #[must_use]
    pub fn places(&self, node: NodeKey) -> bool {
        self.placed.iter().any(|(n, _)| *n == node)
    }

    /// The framebuffers the layout reads.
    pub fn buffers(&self) -> impl Iterator<Item = BufferId> + '_ {
        self.layout.iter().filter_map(|c| match c.source {
            PlaneSource::Buffer(b) => Some(b),
            PlaneSource::OutputFront => None,
        })
    }

    /// Equal but for which buffers the planes show: switching between
    /// two such decisions is a buffer swap, not a mode switch.
    #[must_use]
    pub fn same_shape(&self, other: &Decision) -> bool {
        let unbuf = |c: &PlaneConfig| PlaneConfig {
            source: match c.source {
                PlaneSource::Buffer(_) => PlaneSource::Buffer(BufferId(0)),
                s @ PlaneSource::OutputFront => s,
            },
            ..*c
        };
        self.mode == other.mode
            && self.placed == other.placed
            && self.underlays == other.underlays
            && self.layout.len() == other.layout.len()
            && self
                .layout
                .iter()
                .zip(&other.layout)
                .all(|(a, b)| unbuf(a) == unbuf(b))
    }

    /// Point every placed Surface's plane at its candidate's current
    /// buffer (a cached decision carries the buffers of when it was made).
    fn rebind(&mut self, candidates: &[Candidate]) {
        for (node, plane) in &self.placed {
            let Some(c) = candidates.iter().find(|c| c.node == *node) else {
                continue;
            };
            if let Some(cfg) = self.layout.iter_mut().find(|l| l.plane == *plane) {
                cfg.source = PlaneSource::Buffer(c.buffer);
            }
        }
    }
}

/// The visible part of a Surface: `src` (buffer pixels) is drawn at `dst`
/// (device pixels) and only `visible` (inside `dst`) shows. Returns the
/// 16.16 source that maps onto `visible`.
#[must_use]
pub fn crop(src: IRect, dst: IRect, visible: IRect) -> SrcRect {
    if dst.is_empty() || src.is_empty() {
        return SrcRect::default();
    }
    // 16.16 per device pixel, per axis.
    let fx = (i64::from(src.w) << 16) / i64::from(dst.w);
    let fy = (i64::from(src.h) << 16) / i64::from(dst.h);
    let map = |off: i32, len: i32, base: i32, f: i64| {
        let a = (i64::from(base) << 16) + i64::from(off) * f;
        let b = (i64::from(base) << 16) + i64::from(off + len) * f;
        (a.max(0) as u32, (b - a).max(0) as u32)
    };
    let (x, w) = map(visible.x - dst.x, visible.w, src.x, fx);
    let (y, h) = map(visible.y - dst.y, visible.h, src.y, fy);
    // A full-width crop is exact, whatever the rounding of `f`.
    let w = if visible.w == dst.w && visible.x == dst.x {
        (src.w as u32) << 16
    } else {
        w
    };
    let h = if visible.h == dst.h && visible.y == dst.y {
        (src.h as u32) << 16
    } else {
        h
    };
    SrcRect { x, y, w, h }
}

/// The format a `SurfaceHint` suggests on an output with these planes:
/// `YUYV` where a plane lists linear YUYV but none lists NV12 (Haswell:
/// its overlay then takes the frames as they are), `NV12` otherwise —
/// the cheapest format for the CPU path too.
#[must_use]
pub fn hint_format(planes: &[PlaneInfo]) -> u32 {
    use nitro_wire::types::format;
    let listed = |f: Fourcc| {
        planes
            .iter()
            .any(|p| p.kind != PlaneKind::Cursor && p.supports(f, MOD_LINEAR))
    };
    if listed(Fourcc::YUYV) && !listed(Fourcc::NV12) {
        format::YUYV
    } else {
        format::NV12
    }
}

/// The format a server-allocated scanout buffer gets when the client asks
/// for "the server's choice": `NV12` if some plane lists linear NV12,
/// else `YUYV` if one lists it, else `XR24` — the most compact format
/// a plane of this output can show.
#[must_use]
pub fn alloc_format(planes: &[PlaneInfo]) -> u32 {
    use nitro_wire::types::format;
    let listed = |f: Fourcc| {
        planes
            .iter()
            .any(|p| p.kind != PlaneKind::Cursor && p.supports(f, MOD_LINEAR))
    };
    if listed(Fourcc::NV12) {
        format::NV12
    } else if listed(Fourcc::YUYV) {
        format::YUYV
    } else {
        format::XR24
    }
}

/// `COLOR_ENCODING` for a Surface's matrix.
#[must_use]
pub const fn encoding(m: ColorMatrix) -> ColorEncoding {
    match m {
        ColorMatrix::Bt601 => ColorEncoding::Bt601,
        ColorMatrix::Bt709 => ColorEncoding::Bt709,
        ColorMatrix::Bt2020 => ColorEncoding::Bt2020,
    }
}

/// `COLOR_RANGE` for a Surface's range.
#[must_use]
pub const fn range(r: nitro_scene::ColorRange) -> ColorRange {
    match r {
        nitro_scene::ColorRange::Limited => ColorRange::Limited,
        nitro_scene::ColorRange::Full => ColorRange::Full,
    }
}

fn kms_rect(r: IRect) -> KmsRect {
    KmsRect::new(r.x, r.y, r.w.max(0).cast_unsigned(), r.h.max(0).cast_unsigned())
}

/// `c` on `p`, if the plane can take it at all: format and modifier
/// listed, scale within the rule, colour properties available. `None`
/// means "not worth a test".
fn config(c: &Candidate, p: &PlaneInfo) -> Option<PlaneConfig> {
    if !p.supports(c.format, c.modifier) {
        return None;
    }
    let dst = kms_rect(c.dst);
    let mut cfg = PlaneConfig::new(p.id, PlaneSource::Buffer(c.buffer), c.src, dst);
    if cfg.assignment(None).scales() {
        if p.scaling == Some(false) || c.src.w == 0 || c.src.h == 0 {
            return None;
        }
        let pct = |d: u32, s: u32| (u64::from(d) << 16) * 100 / u64::from(s);
        if pct(dst.w, c.src.w) < MIN_SCALE_PCT || pct(dst.h, c.src.h) < MIN_SCALE_PCT {
            return None;
        }
    }
    if c.format.is_yuv() {
        let (e, r) = (encoding(c.color.matrix), range(c.color.range));
        // A plane without the property decodes with the kernel's default,
        // BT.601 limited: fine for exactly that and nothing else.
        if p.color_encodings.is_empty() {
            if e != ColorEncoding::Bt601 {
                return None;
            }
        } else if p.has_encoding(e) {
            cfg.color_encoding = Some(e);
        } else {
            return None;
        }
        if p.color_ranges.is_empty() {
            if r != ColorRange::Limited {
                return None;
            }
        } else if p.has_range(r) {
            cfg.color_range = Some(r);
        } else {
            return None;
        }
    }
    Some(cfg)
}

fn zpos(p: &PlaneInfo) -> u64 {
    p.zpos.map_or(0, |z| z.current)
}

/// Whether overlay `p` stacks above the primary. Without `zpos` on either,
/// the convention (and every driver seen) is overlays above the primary.
fn above(p: &PlaneInfo, primary: &PlaneInfo) -> bool {
    match (p.zpos, primary.zpos) {
        (Some(a), Some(b)) => a.current > b.current,
        _ => true,
    }
}

/// The search behind [`Planner::decide`]: the best layout the kernel
/// accepts, with no cache and no hysteresis. `test` counts on its own.
pub fn search(inp: &Inputs<'_>, test: &mut dyn FnMut(&[PlaneConfig]) -> bool) -> Decision {
    let Some(primary) = inp.planes.iter().find(|p| p.kind == PlaneKind::Primary) else {
        return Decision::default();
    };
    let mut overlays: Vec<&PlaneInfo> = inp
        .planes
        .iter()
        .filter(|p| p.kind == PlaneKind::Overlay)
        .collect();
    overlays.sort_by_key(|p| (zpos(p), p.id));
    let (w, h) = inp.size;
    let full = IRect::new(0, 0, w.cast_signed(), h.cast_signed());

    // (i) Direct: the topmost unobscured Surface covering the output.
    if let Some(c) = inp
        .candidates
        .iter()
        .rev()
        .find(|c| !c.obscured && c.dst == full)
    {
        for p in std::iter::once(primary).chain(overlays.iter().copied()) {
            if let Some(cfg) = config(c, p)
                && test(&[cfg])
            {
                return Decision {
                    mode: Mode::Direct,
                    layout: vec![cfg],
                    placed: vec![(c.node, p.id)],
                    underlays: Vec::new(),
                };
            }
        }
    }

    // (ii)–(iv), greedy from the top.
    let front = PlaneConfig::new(
        primary.id,
        PlaneSource::OutputFront,
        SrcRect::whole(w, h),
        kms_rect(full),
    );
    let mut d = Decision {
        mode: Mode::Composite,
        layout: vec![front],
        placed: Vec::new(),
        underlays: Vec::new(),
    };
    let mut free = overlays.clone();
    let mut swapped = false;
    for c in inp.candidates.iter().rev() {
        if swapped {
            break;
        }
        let mut try_with = |d: &Decision, extra: PlaneConfig| {
            let mut l = d.layout.clone();
            l.push(extra);
            test(&l).then_some(l)
        };
        // (iv) overlay-above.
        if !c.obscured
            && let Some((i, l)) = free
                .iter()
                .enumerate()
                .filter(|(_, p)| above(p, primary))
                .find_map(|(i, p)| try_with(&d, config(c, p)?).map(|l| (i, l)))
        {
            d.layout = l;
            d.placed.push((c.node, free[i].id));
            free.remove(i);
            continue;
        }
        if !inp.alpha {
            continue;
        }
        // (ii) underlay by mutable zpos.
        let below = primary.zpos.and_then(|pz| {
            pz.current
                .checked_sub(1 + d.underlays.len() as u64)
                .map(|z| (pz, z))
        });
        if let Some((_, z)) = below
            && let Some((i, l)) = free.iter().enumerate().find_map(|(i, p)| {
                let pz = p.zpos?;
                if pz.immutable || z < pz.min || z > pz.max {
                    return None;
                }
                try_with(&d, config(c, p)?.with_zpos(z)).map(|l| (i, l))
            })
        {
            d.layout = l;
            d.placed.push((c.node, free[i].id));
            d.underlays.push(c.node);
            free.remove(i);
            continue;
        }
        // (iii) underlay by primary swap: the UI moves to an overlay below
        // every overlay already in use.
        let Some(on_primary) = config(c, primary) else {
            continue;
        };
        let lowest_used = d
            .placed
            .iter()
            .filter_map(|(_, id)| overlays.iter().find(|p| p.id == *id))
            .map(|p| zpos(p))
            .min();
        let Some(i) = free.iter().position(|p| {
            p.supports(Fourcc::ARGB8888, MOD_LINEAR)
                && p.supports(Fourcc::XRGB8888, MOD_LINEAR)
                && lowest_used.is_none_or(|z| zpos(p) < z)
        }) else {
            continue;
        };
        let mut l = d.layout.clone();
        l[0] = on_primary;
        l.push(PlaneConfig {
            plane: free[i].id,
            ..front
        });
        if test(&l) {
            d.layout = l;
            d.placed.push((c.node, primary.id));
            d.underlays.push(c.node);
            free.remove(i);
            swapped = true;
        }
    }
    if d.placed.is_empty() {
        return Decision::default();
    }
    d.mode = if d.underlays.is_empty() {
        Mode::Overlay
    } else {
        Mode::Underlay
    };
    d
}

/// The shape of a decision problem: everything but the buffer ids.
fn signature(inp: &Inputs<'_>) -> u64 {
    let mut h = DefaultHasher::new();
    inp.size.hash(&mut h);
    inp.alpha.hash(&mut h);
    for p in inp.planes {
        p.id.hash(&mut h);
    }
    for c in inp.candidates {
        c.node.hash(&mut h);
        c.format.hash(&mut h);
        c.modifier.hash(&mut h);
        c.src.hash(&mut h);
        c.dst.hash(&mut h);
        c.obscured.hash(&mut h);
        c.color.hash(&mut h);
    }
    h.finish()
}

/// Counters for `stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlannerStats {
    /// `TEST_ONLY` commits asked.
    pub tests: u64,
    /// Decisions answered from the cache.
    pub cache_hits: u64,
    /// Staged layouts the kernel refused after all.
    pub fallbacks: u64,
    /// Changes of layout shape (each a full repaint).
    pub switches: u64,
}

/// Per-output decision state: cache, hysteresis, counters.
#[derive(Debug, Default)]
pub struct Planner {
    /// `(signature, decision)`, least recently used first.
    cache: Vec<(u64, Decision)>,
    /// What [`Planner::decide`] last returned.
    current: Decision,
    current_sig: Option<u64>,
    /// An upgrade being waited for: `(signature, decisions, first ns)`.
    pending: Option<(u64, u32, u64)>,
    /// Counters.
    pub stats: PlannerStats,
}

impl Planner {
    /// Decide the layout for this frame. `test` asks the kernel.
    pub fn decide(
        &mut self,
        inp: &Inputs<'_>,
        now_ns: u64,
        test: &mut dyn FnMut(&[PlaneAssignment<'_>]) -> Verdict,
    ) -> Decision {
        if inp.candidates.is_empty() {
            self.pending = None;
            return self.set(None, Decision::default());
        }
        let sig = signature(inp);
        let mut want = if let Some(i) = self.cache.iter().position(|(s, _)| *s == sig) {
            self.stats.cache_hits += 1;
            let hit = self.cache.remove(i);
            let d = hit.1.clone();
            self.cache.push(hit);
            d
        } else {
            let tests = &mut self.stats.tests;
            let d = search(inp, &mut |l: &[PlaneConfig]| {
                *tests += 1;
                let a: Vec<PlaneAssignment<'_>> = l.iter().map(|c| c.assignment(None)).collect();
                test(&a).accepted()
            });
            if self.cache.len() >= CACHE_ENTRIES {
                self.cache.remove(0);
            }
            self.cache.push((sig, d.clone()));
            d
        };
        want.rebind(inp.candidates);
        if want.placed.len() <= self.current.placed.len() || self.current_sig == Some(sig) {
            self.pending = None;
            return self.set(Some(sig), want);
        }
        // An upgrade: wanted long enough?
        let (frames, since) = match self.pending {
            Some((s, n, t)) if s == sig => (n + 1, t),
            _ => (1, now_ns),
        };
        if frames >= UPGRADE_FRAMES && now_ns.saturating_sub(since) >= UPGRADE_NS {
            self.pending = None;
            return self.set(Some(sig), want);
        }
        self.pending = Some((sig, frames, since));
        // Meanwhile: composite. What was applied was for another shape.
        self.set(None, Decision::default())
    }

    fn set(&mut self, sig: Option<u64>, d: Decision) -> Decision {
        if !d.same_shape(&self.current) {
            self.stats.switches += 1;
        }
        self.current = d.clone();
        self.current_sig = sig;
        d
    }

    /// The kernel refused the layout [`Planner::decide`] returned (the
    /// commit, not the test): composite from now on for that shape.
    pub fn fallback(&mut self) {
        self.stats.fallbacks += 1;
        if let Some(sig) = self.current_sig
            && let Some((_, d)) = self.cache.iter_mut().find(|(s, _)| *s == sig)
        {
            *d = Decision::default();
        }
        self.reset();
    }

    /// The backend dropped the layout (a modeset): start again from the
    /// default, with no upgrade under way.
    pub fn reset(&mut self) {
        self.current = Decision::default();
        self.current_sig = None;
        self.pending = None;
    }

    /// Cached decisions (for tests and `stats`).
    #[must_use]
    pub fn cached(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use nitro_kms::{Backend as _, FakeBackend, FakeOutputSpec, FakePlaneSpec, OutputId};

    const W: u32 = 640;
    const H: u32 = 360;
    const FRAME_NS: u64 = 16_700_000;
    const COLORS: (&[ColorEncoding], &[ColorRange]) = (
        &[ColorEncoding::Bt601, ColorEncoding::Bt709],
        &[ColorRange::Limited, ColorRange::Full],
    );

    fn hsw() -> Vec<FakePlaneSpec> {
        vec![
            FakePlaneSpec::default_primary().zpos(0, 0, 0, true),
            FakePlaneSpec::overlay()
                .formats(&[Fourcc::XRGB8888, Fourcc::YUYV, Fourcc::UYVY])
                .zpos(1, 1, 1, true)
                .color(COLORS.0, COLORS.1),
            FakePlaneSpec::cursor()
                .formats(&[Fourcc::ARGB8888])
                .zpos(2, 2, 2, true),
        ]
    }

    fn kbl_plane(p: FakePlaneSpec) -> FakePlaneSpec {
        p.formats(&[
            Fourcc::XRGB8888,
            Fourcc::ARGB8888,
            Fourcc::NV12,
            Fourcc::YUYV,
        ])
        .scale_limits(90, 800)
        .color(COLORS.0, COLORS.1)
    }

    fn kbl() -> Vec<FakePlaneSpec> {
        vec![
            kbl_plane(FakePlaneSpec::primary()).zpos(0, 0, 0, true),
            kbl_plane(FakePlaneSpec::overlay()).zpos(1, 1, 1, true),
        ]
    }

    fn amd() -> Vec<FakePlaneSpec> {
        vec![
            kbl_plane(FakePlaneSpec::primary()).zpos(2, 0, 3, false),
            kbl_plane(FakePlaneSpec::overlay()).zpos(3, 0, 3, false),
        ]
    }

    fn gen11() -> Vec<FakePlaneSpec> {
        vec![
            kbl_plane(FakePlaneSpec::primary()).zpos(0, 0, 0, true),
            kbl_plane(FakePlaneSpec::overlay()).zpos(1, 1, 1, true),
            kbl_plane(FakePlaneSpec::overlay()).zpos(2, 2, 2, true),
            kbl_plane(FakePlaneSpec::overlay()).zpos(3, 3, 3, true),
        ]
    }

    struct Rig {
        be: FakeBackend,
        id: OutputId,
        planes: Vec<PlaneInfo>,
        planner: Planner,
        now: u64,
    }

    impl Rig {
        fn new(planes: Vec<FakePlaneSpec>) -> Self {
            let mut be = FakeBackend::new(&[FakeOutputSpec::new(W, H).planes(planes)]).unwrap();
            let id = be.outputs()[0].id;
            be.commit(id, &[]).unwrap();
            let planes = be.planes(id);
            Self {
                be,
                id,
                planes,
                planner: Planner::default(),
                now: 1_000_000_000,
            }
        }

        fn cand(&mut self, n: u32, fmt: Fourcc, buf: (u32, u32), dst: IRect) -> Candidate {
            let buffer = self.be.alloc_buffer(fmt, buf.0, buf.1).unwrap();
            Candidate {
                node: NodeKey::from_parts(n, 1),
                buffer,
                format: fmt,
                modifier: MOD_LINEAR,
                src: SrcRect::whole(buf.0, buf.1),
                dst,
                obscured: false,
                color: SurfaceColor::default(),
            }
        }

        /// One uncached, un-hysteresised search.
        fn search(&mut self, cands: &[Candidate]) -> Decision {
            let inp = Inputs {
                candidates: cands,
                planes: &self.planes,
                size: (W, H),
                alpha: true,
            };
            let (be, id) = (&mut self.be, self.id);
            search(&inp, &mut |l: &[PlaneConfig]| {
                let a: Vec<_> = l.iter().map(|c| c.assignment(None)).collect();
                be.test_layout(id, &a).unwrap().accepted()
            })
        }

        fn decide(&mut self, cands: &[Candidate]) -> Decision {
            self.now += FRAME_NS;
            let inp = Inputs {
                candidates: cands,
                planes: &self.planes,
                size: (W, H),
                alpha: true,
            };
            let (be, id) = (&mut self.be, self.id);
            self.planner
                .decide(&inp, self.now, &mut |a| be.test_layout(id, a).unwrap())
        }

        fn plane(&self, i: usize) -> PlaneId {
            self.planes[i].id
        }
    }

    fn full() -> IRect {
        IRect::new(0, 0, W as i32, H as i32)
    }

    fn window() -> IRect {
        IRect::new(100, 50, 320, 180)
    }

    #[test]
    fn fullscreen_is_direct_on_the_overlay_on_hsw_and_the_primary_on_kbl() {
        let mut r = Rig::new(hsw());
        let c = r.cand(1, Fourcc::YUYV, (W, H), full());
        let d = r.search(&[c]);
        assert_eq!(d.mode, Mode::Direct);
        assert_eq!(d.placed, vec![(c.node, r.plane(1))]);
        assert_eq!(d.layout.len(), 1, "primary off");
        assert!(!d.need_alpha());

        let mut r = Rig::new(kbl());
        let c = r.cand(1, Fourcc::NV12, (W, H), full());
        let d = r.search(&[c]);
        assert_eq!(d.mode, Mode::Direct);
        assert_eq!(d.placed, vec![(c.node, r.plane(0))]);
        assert_eq!(d.layout[0].color_encoding, Some(ColorEncoding::Bt709));
        assert_eq!(d.layout[0].color_range, Some(ColorRange::Limited));
    }

    #[test]
    fn a_window_unobscured_goes_on_an_overlay_above() {
        for planes in [hsw(), kbl()] {
            let mut r = Rig::new(planes);
            let c = r.cand(1, Fourcc::YUYV, (320, 180), window());
            let d = r.search(&[c]);
            assert_eq!(d.mode, Mode::Overlay);
            assert_eq!(d.placed, vec![(c.node, r.plane(1))]);
            assert_eq!(d.layout[0].source, PlaneSource::OutputFront);
            assert_eq!(d.layout[0].plane, r.plane(0));
            assert!(!d.need_alpha());
        }
    }

    #[test]
    fn an_obscured_window_underlays_where_it_can() {
        // HSW: nothing below the primary; composite.
        let mut r = Rig::new(hsw());
        let mut c = r.cand(1, Fourcc::YUYV, (320, 180), window());
        c.obscured = true;
        assert_eq!(r.search(&[c]), Decision::default());

        // KBL: primary swap, the UI on the overlay.
        let mut r = Rig::new(kbl());
        let mut c = r.cand(1, Fourcc::NV12, (320, 180), window());
        c.obscured = true;
        let d = r.search(&[c]);
        assert_eq!(d.mode, Mode::Underlay);
        assert_eq!(d.placed, vec![(c.node, r.plane(0))]);
        assert_eq!(d.layout[1].plane, r.plane(1));
        assert_eq!(d.layout[1].source, PlaneSource::OutputFront);
        assert!(d.need_alpha());

        // AMD-like: the overlay moves below the primary.
        let mut r = Rig::new(amd());
        let mut c = r.cand(1, Fourcc::NV12, (320, 180), window());
        c.obscured = true;
        let d = r.search(&[c]);
        assert_eq!(d.mode, Mode::Underlay);
        assert_eq!(d.placed, vec![(c.node, r.plane(1))]);
        assert_eq!(d.layout[1].zpos, Some(1));
        assert_eq!(d.underlays, vec![c.node]);
    }

    #[test]
    fn downscaling_and_unlisted_formats_composite_without_a_test() {
        let mut r = Rig::new(kbl());
        // 640×360 into 320×180: 0.5×.
        let c = r.cand(1, Fourcc::NV12, (W, H), window());
        let before = r.be.test_log().len();
        let d = r.search(&[c]);
        assert_eq!(d, Decision::default());
        assert_eq!(r.be.test_log().len(), before, "pre-filtered");
        // Upscale is fine.
        let c = r.cand(2, Fourcc::NV12, (160, 90), window());
        assert_eq!(r.search(&[c]).mode, Mode::Overlay);

        // NV12 on HSW.
        let mut r = Rig::new(hsw());
        let c = r.cand(1, Fourcc::NV12, (320, 180), window());
        assert_eq!(r.search(&[c]), Decision::default());
        assert!(r.be.test_log().is_empty());
        // Scaled YUYV on HSW (no scaler).
        let c = r.cand(2, Fourcc::YUYV, (160, 90), window());
        assert_eq!(r.search(&[c]), Decision::default());
    }

    #[test]
    fn two_videos_get_one_plane_on_kbl_and_two_on_gen11() {
        let other = IRect::new(420, 200, 160, 90);
        let mut r = Rig::new(kbl());
        let a = r.cand(1, Fourcc::NV12, (320, 180), window());
        let b = r.cand(2, Fourcc::NV12, (160, 90), other);
        let d = r.search(&[a, b]);
        assert_eq!(d.placed, vec![(b.node, r.plane(1))], "topmost first");

        let mut r = Rig::new(gen11());
        let a = r.cand(1, Fourcc::NV12, (320, 180), window());
        let b = r.cand(2, Fourcc::NV12, (160, 90), other);
        let d = r.search(&[a, b]);
        assert_eq!(d.placed.len(), 2);
        assert_eq!(d.mode, Mode::Overlay);
    }

    #[test]
    fn colour_metadata_maps_or_rejects() {
        let mut r = Rig::new(hsw());
        let mut c = r.cand(1, Fourcc::YUYV, (320, 180), window());
        c.color = SurfaceColor {
            matrix: ColorMatrix::Bt601,
            range: nitro_scene::ColorRange::Full,
        };
        let d = r.search(&[c]);
        assert_eq!(d.layout[1].color_encoding, Some(ColorEncoding::Bt601));
        assert_eq!(d.layout[1].color_range, Some(ColorRange::Full));
        c.color.matrix = ColorMatrix::Bt2020;
        assert_eq!(r.search(&[c]), Decision::default());
        // RGB carries none.
        let c = r.cand(2, Fourcc::XRGB8888, (320, 180), window());
        let d = r.search(&[c]);
        assert_eq!(d.layout[1].color_encoding, None);
        // A plane without the properties only takes the kernel default.
        let mut r = Rig::new(vec![
            FakePlaneSpec::default_primary(),
            FakePlaneSpec::overlay().formats(&[Fourcc::YUYV]),
        ]);
        let mut c = r.cand(1, Fourcc::YUYV, (320, 180), window());
        assert_eq!(r.search(&[c]), Decision::default());
        c.color.matrix = ColorMatrix::Bt601;
        assert_eq!(r.search(&[c]).mode, Mode::Overlay);
    }

    #[test]
    fn steady_state_is_cached_and_tests_nothing() {
        let mut r = Rig::new(hsw());
        let c = r.cand(1, Fourcc::YUYV, (320, 180), window());
        for _ in 0..UPGRADE_FRAMES {
            r.decide(&[c]);
        }
        let d = r.decide(&[c]);
        assert_eq!(d.mode, Mode::Overlay);
        let tests = r.be.test_log().len();
        // New buffers each frame, same shape: no test, buffer rebound.
        for n in 0..10 {
            let next = r.cand(1, Fourcc::YUYV, (320, 180), window());
            let d = r.decide(&[next]);
            assert_eq!(d.layout[1].source, PlaneSource::Buffer(next.buffer), "{n}");
        }
        assert_eq!(r.be.test_log().len(), tests);
        assert_eq!(r.planner.stats.tests as usize, tests);
        assert!(r.planner.stats.cache_hits >= 10);
        assert_eq!(r.planner.cached(), 1);
    }

    #[test]
    fn toggling_controls_stays_composite_and_stability_upgrades() {
        let mut r = Rig::new(hsw());
        let clear = r.cand(1, Fourcc::YUYV, (W, H), full());
        let covered = Candidate {
            obscured: true,
            ..clear
        };
        // Controls shown and hidden every 5 frames: never upgrades.
        for i in 0..60 {
            let c = if (i / 5) % 2 == 0 { clear } else { covered };
            assert_eq!(r.decide(&[c]).mode, Mode::Composite, "frame {i}");
        }
        // Hidden for good: composite until 15 frames / 250 ms, then direct.
        let mut modes = Vec::new();
        for _ in 0..UPGRADE_FRAMES + 2 {
            modes.push(r.decide(&[clear]).mode);
        }
        let first = modes.iter().position(|m| *m == Mode::Direct).unwrap();
        // 15 frames at 60 Hz span 234 ms: the 16th is the first past 250.
        assert_eq!(first, UPGRADE_FRAMES as usize);
        // Shown again: back to composite at once.
        assert_eq!(r.decide(&[covered]).mode, Mode::Composite);
        let switches = r.planner.stats.switches;
        assert_eq!(switches, 2);
    }

    #[test]
    fn a_fallback_pins_the_shape_to_composite() {
        let mut r = Rig::new(hsw());
        let c = r.cand(1, Fourcc::YUYV, (320, 180), window());
        for _ in 0..UPGRADE_FRAMES {
            r.decide(&[c]);
        }
        assert_eq!(r.decide(&[c]).mode, Mode::Overlay);
        r.planner.fallback();
        for _ in 0..40 {
            assert_eq!(r.decide(&[c]).mode, Mode::Composite);
        }
        assert_eq!(r.planner.stats.fallbacks, 1);
    }

    #[test]
    fn crop_maps_the_visible_part() {
        let src = IRect::new(0, 0, 640, 360);
        let dst = IRect::new(-100, 0, 320, 180);
        let s = crop(src, dst, IRect::new(0, 0, 220, 180));
        assert_eq!(s.x, 200 << 16);
        assert_eq!(s.w, 440 << 16);
        assert_eq!((s.y, s.h), (0, 360 << 16));
        assert_eq!(crop(src, dst, dst), SrcRect::whole(640, 360));
    }

    #[test]
    fn formats_follow_the_planes() {
        use nitro_wire::types::format;
        let hsw = Rig::new(hsw()).planes;
        let kbl = Rig::new(kbl()).planes;
        let plain = Rig::new(vec![FakePlaneSpec::default_primary()]).planes;
        assert_eq!(hint_format(&hsw), format::YUYV);
        assert_eq!(hint_format(&kbl), format::NV12);
        assert_eq!(hint_format(&plain), format::NV12);
        assert_eq!(alloc_format(&hsw), format::YUYV);
        assert_eq!(alloc_format(&kbl), format::NV12);
        assert_eq!(alloc_format(&plain), format::XR24);
    }
}
