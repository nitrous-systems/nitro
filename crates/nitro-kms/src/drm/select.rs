//! Hardware-independent decisions: which mode to use, which CRTC and
//! primary plane drive which connector, refresh arithmetic. Pure functions
//! over plain data so they are unit-tested without a device.
//!
//! # Picking a mode
//!
//! With nothing configured the rule is the kernel's own: the connector's
//! *preferred* mode, else the largest area, ties broken by the highest
//! refresh. [`ModeRequest`] is what `output.<connector>.mode` (and
//! `NITRO_MODE`) turn into, and [`select_mode`] applies it. A request that
//! matches nothing is **not** an error — it falls back to that default and
//! the caller warns with [`describe_modes`], which prints exactly the list
//! a user needs in order to write a line that does match.

use std::fmt;

/// Refresh rates are matched to the nearest listed mode within this many
/// millihertz.
///
/// Half a hertz, which is the gap that matters: `@60` must pick 60.000 and
/// not the 59.940 mode sitting next to it in every HDMI table, and `@120`
/// must find a 119.982 the kernel rounded differently from the EDID. A
/// tolerance smaller than the 60/59.94 gap (60 mHz) and larger than the
/// rounding noise is the whole requirement, and 500 is the round number in
/// the middle.
pub const REFRESH_TOLERANCE_MHZ: u32 = 500;

/// A mode as far as selection is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeCandidate {
    /// Horizontal pixels.
    pub width: u32,
    /// Vertical pixels.
    pub height: u32,
    /// Refresh in millihertz.
    pub refresh_mhz: u32,
    /// Kernel marked it `DRM_MODE_TYPE_PREFERRED`.
    pub preferred: bool,
    /// Interlaced modes are avoided when anything else exists.
    pub interlaced: bool,
}

impl fmt::Display for ModeCandidate {
    /// `1920x1080@120` — the spelling `output.<c>.mode` takes, so a warning
    /// that lists the modes is a warning a user can copy a line out of.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{}@{}",
            self.width,
            self.height,
            hz_text(self.refresh_mhz)
        )?;
        if self.interlaced {
            f.write_str("i")?;
        }
        Ok(())
    }
}

/// Millihertz as a person writes a refresh rate: `120`, `59.94`.
#[must_use]
pub fn hz_text(refresh_mhz: u32) -> String {
    if refresh_mhz.is_multiple_of(1000) {
        format!("{}", refresh_mhz / 1000)
    } else {
        let s = format!("{:.3}", f64::from(refresh_mhz) / 1000.0);
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// How many modes a "no such mode" warning names before it summarises.
///
/// The box's HDMI connector lists **45**, which came as a surprise: the
/// kernel's `i915_display_info` shows a handful of interesting ones and
/// the EDID's real list runs all the way down to 720x400@70. Printed in
/// full that is a 1 400-character log line, which is worse than useless —
/// the reader scrolls past it. Twelve is enough to cover every mode of
/// the size anyone asked for plus the sizes above it, and the tail is
/// reported as a count with a pointer at `modes`, which prints all of
/// them in a form worth reading.
const MODES_IN_A_WARNING: usize = 12;

/// Every mode a connector lists, in the spelling `output.<c>.mode` takes.
///
/// This is the text a "no such mode" warning carries, and it is the whole
/// reason that warning is worth emitting: the user asked for a mode the
/// panel does not have, and the next thing they need is the list of the
/// ones it does. Truncated at [`MODES_IN_A_WARNING`] with the remainder
/// counted — a real connector lists far more modes than a log line should
/// carry, and a warning nobody reads is not a warning.
#[must_use]
pub fn describe_modes(modes: &[ModeCandidate]) -> String {
    if modes.is_empty() {
        return "none".to_owned();
    }
    let head = modes
        .iter()
        .take(MODES_IN_A_WARNING)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match modes.len().checked_sub(MODES_IN_A_WARNING) {
        Some(rest) if rest > 0 => {
            format!("{head}, and {rest} more (`modes` on the control socket lists them all)")
        }
        _ => head,
    }
}

/// Raw mode timings, the way an X-style modeline spells them.
///
/// A mode the connector does **not** list, handed to the kernel as a
/// `MODE_ID` blob of its own. The clock is in kHz, like the kernel's own
/// `i915_display_info` prints it and unlike X's MHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modeline {
    /// Pixel clock in kHz.
    pub clock_khz: u32,
    /// Horizontal active pixels.
    pub hdisplay: u32,
    /// Horizontal sync start.
    pub hsync_start: u32,
    /// Horizontal sync end.
    pub hsync_end: u32,
    /// Horizontal total.
    pub htotal: u32,
    /// Vertical active lines.
    pub vdisplay: u32,
    /// Vertical sync start.
    pub vsync_start: u32,
    /// Vertical sync end.
    pub vsync_end: u32,
    /// Vertical total.
    pub vtotal: u32,
    /// `+hsync` rather than `-hsync`.
    pub hsync_positive: bool,
    /// `+vsync` rather than `-vsync`.
    pub vsync_positive: bool,
}

impl Modeline {
    /// Vertical refresh in millihertz, by the same arithmetic the kernel
    /// uses for a listed mode.
    #[must_use]
    pub fn refresh_mhz(&self) -> u32 {
        refresh_millihertz(self.clock_khz, self.htotal, self.vtotal, 0, false, false)
    }

    /// Parse `<clock_khz> <hdisp> <hss> <hse> <htotal> <vdisp> <vss> <vse>
    /// <vtotal> [+hsync|-hsync] [+vsync|-vsync]`.
    ///
    /// # Errors
    /// A message naming what is wrong, suitable for a warning line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut words = text.split_ascii_whitespace();
        let mut num = |what: &str| -> Result<u32, String> {
            let w = words
                .next()
                .ok_or_else(|| format!("modeline is missing {what}"))?;
            // The clock may be written with decimals (`285500.0`); the
            // timings are line and pixel counts and never are.
            let v: f64 = w
                .parse()
                .map_err(|_| format!("modeline {what} {w:?} is not a number"))?;
            if !v.is_finite() || v < 0.0 || v > f64::from(u32::MAX) {
                return Err(format!("modeline {what} {w:?} is out of range"));
            }
            Ok(v.round() as u32)
        };
        let mut this = Self {
            clock_khz: num("clock")?,
            hdisplay: num("hdisplay")?,
            hsync_start: num("hsync_start")?,
            hsync_end: num("hsync_end")?,
            htotal: num("htotal")?,
            vdisplay: num("vdisplay")?,
            vsync_start: num("vsync_start")?,
            vsync_end: num("vsync_end")?,
            vtotal: num("vtotal")?,
            // X's own default when a modeline says nothing, and what
            // CVT-RB timings want.
            hsync_positive: true,
            vsync_positive: false,
        };
        for flag in words {
            match flag.to_ascii_lowercase().as_str() {
                "+hsync" => this.hsync_positive = true,
                "-hsync" => this.hsync_positive = false,
                "+vsync" => this.vsync_positive = true,
                "-vsync" => this.vsync_positive = false,
                other => return Err(format!("modeline flag {other:?} is not a sync polarity")),
            }
        }
        this.check()?;
        Ok(this)
    }

    /// Reject timings the kernel would reject, or that would divide by
    /// zero on the way there.
    ///
    /// Called by [`Modeline::parse`], which is how every modeline from a
    /// file or the environment arrives. The fields are `pub`, so a
    /// hand-built one bypasses it — `mode_from_modeline` therefore
    /// `debug_assert!`s on this before casting each timing to `u16`.
    ///
    /// # Errors
    /// A message naming what is wrong, suitable for a warning line.
    pub fn check(&self) -> Result<(), String> {
        if self.clock_khz == 0 {
            return Err("modeline clock is 0".to_owned());
        }
        if self.hdisplay == 0 || self.vdisplay == 0 {
            return Err("modeline has a zero active area".to_owned());
        }
        // `drm_mode_modeinfo` is 16-bit per timing, so anything past a
        // `u16` is not a mode the kernel can be told about at all.
        let fits = [
            self.hdisplay,
            self.hsync_start,
            self.hsync_end,
            self.htotal,
            self.vdisplay,
            self.vsync_start,
            self.vsync_end,
            self.vtotal,
        ]
        .iter()
        .all(|&v| u16::try_from(v).is_ok());
        if !fits {
            return Err("modeline timing does not fit in 16 bits".to_owned());
        }
        let h_ordered = self.hdisplay <= self.hsync_start
            && self.hsync_start <= self.hsync_end
            && self.hsync_end <= self.htotal;
        let v_ordered = self.vdisplay <= self.vsync_start
            && self.vsync_start <= self.vsync_end
            && self.vsync_end <= self.vtotal;
        if !h_ordered || !v_ordered {
            return Err(
                "modeline timings are not ordered display <= sync_start <= sync_end <= total"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

impl fmt::Display for Modeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {} {} {} {} {} {} {}hsync {}vsync",
            self.clock_khz,
            self.hdisplay,
            self.hsync_start,
            self.hsync_end,
            self.htotal,
            self.vdisplay,
            self.vsync_start,
            self.vsync_end,
            self.vtotal,
            if self.hsync_positive { '+' } else { '-' },
            if self.vsync_positive { '+' } else { '-' },
        )
    }
}

/// What `output.<connector>.mode` (or `NITRO_MODE`) asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeRequest {
    /// `WxH` or `WxH@Hz`: exactly this size, and either the nearest listed
    /// refresh within [`REFRESH_TOLERANCE_MHZ`] or — with no `@` — the
    /// highest refresh the connector lists at that size.
    Size {
        /// Horizontal pixels; matched exactly.
        width: u32,
        /// Vertical pixels; matched exactly.
        height: u32,
        /// Wanted refresh in millihertz, `None` for "the fastest at this size".
        refresh_mhz: Option<u32>,
    },
    /// `max`: largest area, then highest refresh. Ignores the connector's
    /// `PREFERRED` flag, which is the only thing that separates it from the
    /// default rule.
    Max,
    /// `fastest`: the highest refresh at the size the default rule would
    /// have picked.
    Fastest,
    /// A modeline: timings the connector does not list at all.
    Custom(Modeline),
}

impl ModeRequest {
    /// Parse `WxH`, `WxH@Hz`, `max` or `fastest`.
    ///
    /// The refresh may be fractional (`@59.94`). Case-insensitive on the
    /// `x` and on the two aliases, because a configuration file is typed
    /// by a person.
    ///
    /// # Errors
    /// A message naming what is wrong, suitable for a warning line.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        match text.to_ascii_lowercase().as_str() {
            "max" => return Ok(ModeRequest::Max),
            "fastest" => return Ok(ModeRequest::Fastest),
            _ => {}
        }
        let (size, rate) = match text.split_once('@') {
            Some((s, r)) => (s.trim(), Some(r.trim())),
            None => (text, None),
        };
        let (w, h) = size
            .split_once(['x', 'X'])
            .ok_or_else(|| format!("{text:?} is not `WxH`, `WxH@Hz`, `max` or `fastest`"))?;
        let width: u32 = w
            .trim()
            .parse()
            .map_err(|_| format!("width {:?} is not a number", w.trim()))?;
        let height: u32 = h
            .trim()
            .parse()
            .map_err(|_| format!("height {:?} is not a number", h.trim()))?;
        if width == 0 || height == 0 {
            return Err(format!("{text:?} has a zero dimension"));
        }
        let refresh_mhz = match rate {
            None => None,
            Some(r) => {
                let hz: f64 = r
                    .parse()
                    .map_err(|_| format!("refresh {r:?} is not a number of hertz"))?;
                if !hz.is_finite() || hz <= 0.0 || hz > 1_000.0 {
                    return Err(format!("refresh {r:?} is not a plausible rate"));
                }
                Some((hz * 1000.0).round() as u32)
            }
        };
        Ok(ModeRequest::Size {
            width,
            height,
            refresh_mhz,
        })
    }
}

impl fmt::Display for ModeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeRequest::Size {
                width,
                height,
                refresh_mhz: None,
            } => write!(f, "{width}x{height}"),
            ModeRequest::Size {
                width,
                height,
                refresh_mhz: Some(r),
            } => write!(f, "{width}x{height}@{}", hz_text(*r)),
            ModeRequest::Max => f.write_str("max"),
            ModeRequest::Fastest => f.write_str("fastest"),
            ModeRequest::Custom(m) => write!(f, "modeline {m}"),
        }
    }
}

/// The index the default rule picks: the preferred mode, else the largest
/// area, ties broken by the highest refresh. Interlaced modes lose to any
/// progressive mode. `None` for an empty list.
#[must_use]
pub fn default_mode(modes: &[ModeCandidate]) -> Option<usize> {
    let key = |m: &ModeCandidate| {
        (
            !m.interlaced,
            m.preferred,
            u64::from(m.width) * u64::from(m.height),
            m.refresh_mhz,
        )
    };
    modes
        .iter()
        .enumerate()
        .max_by_key(|(_, m)| key(m))
        .map(|(i, _)| i)
}

/// The index a request picks, or `None` when the connector lists nothing
/// that satisfies it.
///
/// Public because "did the request match?" is a question the caller has to
/// answer separately from "which mode do I set": an unmatched request is a
/// warning *and* a fall back to the default, and [`select_mode`] only does
/// the second half.
///
/// Interlaced modes are never returned. The default rule tolerates one as
/// a last resort (a connector that lists nothing else still has to light
/// up), but an explicit request is an instruction, and answering
/// `1920x1080@60` with a 1080i mode would be a different picture than the
/// one asked for.
#[must_use]
pub fn request_match(modes: &[ModeCandidate], wanted: &ModeRequest) -> Option<usize> {
    let progressive = |m: &ModeCandidate| !m.interlaced;
    match *wanted {
        // A modeline is not chosen from the list at all: the whole point
        // is timings the connector does not advertise.
        ModeRequest::Custom(_) => None,
        ModeRequest::Max => modes
            .iter()
            .enumerate()
            .filter(|(_, m)| progressive(m))
            .max_by_key(|(_, m)| (u64::from(m.width) * u64::from(m.height), m.refresh_mhz))
            .map(|(i, _)| i),
        ModeRequest::Fastest => {
            let (w, h) = {
                let i = default_mode(modes)?;
                (modes[i].width, modes[i].height)
            };
            modes
                .iter()
                .enumerate()
                .filter(|(_, m)| progressive(m) && m.width == w && m.height == h)
                .max_by_key(|(_, m)| m.refresh_mhz)
                .map(|(i, _)| i)
        }
        ModeRequest::Size {
            width,
            height,
            refresh_mhz,
        } => {
            let sized = modes
                .iter()
                .enumerate()
                .filter(|(_, m)| progressive(m) && m.width == width && m.height == height);
            match refresh_mhz {
                None => sized.max_by_key(|(_, m)| m.refresh_mhz).map(|(i, _)| i),
                Some(want) => sized
                    .map(|(i, m)| (i, m.refresh_mhz.abs_diff(want)))
                    .filter(|&(_, d)| d <= REFRESH_TOLERANCE_MHZ)
                    // Nearest wins; an exact tie between two equally close
                    // modes goes to the earlier one, which is the order the
                    // kernel listed them in.
                    .min_by_key(|&(i, d)| (d, i))
                    .map(|(i, _)| i),
            }
        }
    }
}

/// Index of the mode to use.
///
/// `wanted` is what the configuration asked for; `None` — and a request
/// that matches nothing — is the default rule ([`default_mode`]). The
/// fallback is deliberate: a `mode` line naming a resolution this monitor
/// does not have must cost a warning, not a desktop.
#[must_use]
pub fn select_mode(modes: &[ModeCandidate], wanted: Option<&ModeRequest>) -> Option<usize> {
    match wanted {
        Some(req) => request_match(modes, req).or_else(|| default_mode(modes)),
        None => default_mode(modes),
    }
}

/// Index of the mode to use when the connector is **already lit**:
/// `on_screen` is the listed mode whose timings the CRTC is scanning out
/// right now, and `chosen` is what [`select_mode`] picked.
///
/// A full modeset is what makes a monitor resync (one to three seconds of
/// black on many HDMI panels), and the kernel does one only when the new
/// mode differs from the old one. So a server that starts on a panel
/// fbcon or a previous nitro already lit should take the mode it finds,
/// **when that mode answers the configuration as well**. This is the
/// greeter-to-desktop handover in `docs/greeter.md`, and it applies to
/// every other start as well.
///
/// "As well" depends on what was asked for:
///
/// - **Nothing** (the default rule): the same size, progressive, and a
///   refresh no more than [`REFRESH_TOLERANCE_MHZ`] below the default
///   pick. 59.94 on screen where the preferred mode is 60 is kept, and
///   costs 0.1 %. 30 on screen where 60 is available is not kept: the
///   rule never lowers the refresh rate to avoid a blank.
/// - **An explicit request** (`WxH`, `WxH@Hz`, `max`, `fastest`): the
///   on-screen mode must have exactly the refresh the request resolved
///   to. The request is an instruction, and `@60` means 60.000, not the
///   59.940 mode that happens to be lit
///   ([`REFRESH_TOLERANCE_MHZ`] is about finding the mode, not about
///   substituting a different one). A request that matched nothing falls
///   back to the default rule in [`select_mode`], so it is treated as no
///   request here as well.
/// - **A modeline** never reaches this function: it is not chosen from
///   the list.
///
/// Interlaced on-screen modes are never kept.
#[must_use]
pub fn keep_on_screen(
    modes: &[ModeCandidate],
    chosen: usize,
    on_screen: Option<usize>,
    wanted: Option<&ModeRequest>,
) -> usize {
    let (Some(p), Some(i)) = (modes.get(chosen), on_screen) else {
        return chosen;
    };
    let Some(c) = modes.get(i) else {
        return chosen;
    };
    if c.interlaced || (c.width, c.height) != (p.width, p.height) {
        return chosen;
    }
    let explicit = wanted.is_some_and(|r| request_match(modes, r).is_some());
    let good_enough = if explicit {
        c.refresh_mhz == p.refresh_mhz
    } else {
        c.refresh_mhz + REFRESH_TOLERANCE_MHZ >= p.refresh_mhz
    };
    if good_enough { i } else { chosen }
}

/// Vertical refresh in millihertz from raw mode timings, the way the
/// kernel's `drm_mode_vrefresh` computes it.
#[must_use]
pub fn refresh_millihertz(
    clock_khz: u32,
    htotal: u32,
    vtotal: u32,
    vscan: u32,
    interlaced: bool,
    doublescan: bool,
) -> u32 {
    if htotal == 0 || vtotal == 0 {
        return 0;
    }
    let mut num = u64::from(clock_khz) * 1_000_000;
    let mut den = u64::from(htotal) * u64::from(vtotal);
    if interlaced {
        num *= 2;
    }
    if doublescan {
        den *= 2;
    }
    if vscan > 1 {
        den *= u64::from(vscan);
    }
    (num / den) as u32
}

/// What to do with an existing output when the connector was re-probed.
///
/// Extracted from `DrmBackend::reconcile` so the rule is testable without
/// a device, which is this module's whole reason to exist. The rule is
/// short but it is the difference between `output.<c>.mode` being a live
/// key and a restart-only one, and it is not visible from the DRM code
/// that implements it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    /// Nothing moved: leave the output completely alone.
    Keep,
    /// Same size, different timing. Swap the CRTC's mode blob and keep
    /// the output — same id, same buffers, same framebuffers.
    ///
    /// The case that matters: a rate change alters no geometry, so
    /// destroying the output would reach the server as an unplug followed
    /// by a plug — the scene drops it, its windows migrate to the
    /// primary, and every client is sent `OutputGone` — for a change the
    /// user asked for and that moves nothing.
    Retime,
    /// The connector is gone, the mode changed **size**, or the CRTC or
    /// plane moved. Destroy the output and build a fresh one: the buffers
    /// really are the wrong size and the clients really do need
    /// reconfiguring.
    Replace,
}

/// Decide [`Reconcile`] for one output.
///
/// `probed` is the mode the connector now wants, `None` when the
/// connector is gone or its resources moved. `current` is the mode the
/// output is on. Sizes are `(width, height)` in pixels.
#[must_use]
pub fn reconcile_one(current: (u32, u32, u32), probed: Option<(u32, u32, u32)>) -> Reconcile {
    let Some(p) = probed else {
        return Reconcile::Replace;
    };
    if p == current {
        Reconcile::Keep
    } else if (p.0, p.1) == (current.0, current.1) {
        Reconcile::Retime
    } else {
        Reconcile::Replace
    }
}

/// A connected connector as far as CRTC assignment is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorCandidate {
    /// Bit `i` set: CRTC index `i` can drive this connector (union over
    /// its encoders' `possible_crtcs`).
    pub crtc_mask: u32,
    /// CRTC index currently driving it, if any; preferred to avoid a
    /// needless full modeset.
    pub current_crtc: Option<usize>,
}

/// A plane as far as CRTC assignment is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneCandidate {
    /// Bit `i` set: plane can sit on CRTC index `i`.
    pub crtc_mask: u32,
    /// `type == Primary`. Only primary planes are ever used.
    pub primary: bool,
}

/// The result of assignment for one connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    /// CRTC index into the device's CRTC list.
    pub crtc: usize,
    /// Plane index into the device's plane list.
    pub plane: usize,
}

/// Give every connector a distinct CRTC and a distinct primary plane.
/// `used_crtcs` / `used_planes` are already taken (by outputs that are
/// being kept across a rescan). Greedy: connectors that can keep their
/// current CRTC do so first, then the rest take the lowest free CRTC that
/// has a free primary plane. Not a maximum matching — good enough for the
/// handful of CRTCs real hardware has, and documented as such.
#[must_use]
pub fn assign(
    connectors: &[ConnectorCandidate],
    planes: &[PlaneCandidate],
    used_crtcs: u32,
    used_planes: &[bool],
) -> Vec<Option<Assignment>> {
    let mut crtc_taken = used_crtcs;
    let mut plane_taken: Vec<bool> = planes
        .iter()
        .enumerate()
        .map(|(i, _)| used_planes.get(i).copied().unwrap_or(false))
        .collect();
    let mut result = vec![None; connectors.len()];

    let free_plane_for = |crtc: usize, plane_taken: &mut Vec<bool>| -> Option<usize> {
        let idx = planes
            .iter()
            .enumerate()
            .position(|(i, p)| p.primary && !plane_taken[i] && p.crtc_mask & (1 << crtc) != 0)?;
        plane_taken[idx] = true;
        Some(idx)
    };

    // Pass 1: keep current CRTCs where possible.
    for (ci, conn) in connectors.iter().enumerate() {
        let Some(cur) = conn.current_crtc else {
            continue;
        };
        if cur >= 32 || conn.crtc_mask & (1 << cur) == 0 || crtc_taken & (1 << cur) != 0 {
            continue;
        }
        if let Some(plane) = free_plane_for(cur, &mut plane_taken) {
            crtc_taken |= 1 << cur;
            result[ci] = Some(Assignment { crtc: cur, plane });
        }
    }

    // Pass 2: everything else, lowest free CRTC first.
    for (ci, conn) in connectors.iter().enumerate() {
        if result[ci].is_some() {
            continue;
        }
        for crtc in 0..32 {
            if conn.crtc_mask & (1 << crtc) == 0 || crtc_taken & (1 << crtc) != 0 {
                continue;
            }
            if let Some(plane) = free_plane_for(crtc, &mut plane_taken) {
                crtc_taken |= 1 << crtc;
                result[ci] = Some(Assignment { crtc, plane });
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(w: u32, h: u32, r: u32, preferred: bool) -> ModeCandidate {
        ModeCandidate {
            width: w,
            height: h,
            refresh_mhz: r,
            preferred,
            interlaced: false,
        }
    }

    #[test]
    fn preferred_mode_wins_over_larger() {
        let modes = [m(1920, 1080, 60_000, false), m(1280, 720, 60_000, true)];
        assert_eq!(select_mode(&modes, None), Some(1));
    }

    #[test]
    fn largest_then_fastest_without_preferred() {
        let modes = [
            m(1280, 720, 120_000, false),
            m(1920, 1080, 50_000, false),
            m(1920, 1080, 60_000, false),
            m(1920, 1080, 59_940, false),
        ];
        assert_eq!(select_mode(&modes, None), Some(2));
        assert_eq!(select_mode(&[], None), None);
    }

    #[test]
    fn interlaced_loses_even_when_preferred() {
        let mut modes = [m(1920, 1080, 60_000, true), m(1280, 720, 60_000, false)];
        modes[0].interlaced = true;
        assert_eq!(select_mode(&modes, None), Some(1));
        // ... unless it is all there is.
        assert_eq!(select_mode(&modes[..1], None), Some(0));
    }

    // -- keeping the mode that is already lit --------------------------------

    fn size(w: u32, h: u32, r: Option<u32>) -> ModeRequest {
        ModeRequest::Size {
            width: w,
            height: h,
            refresh_mhz: r,
        }
    }

    #[test]
    fn nothing_on_screen_keeps_the_pick() {
        let modes = [m(1920, 1080, 60_000, true), m(1920, 1080, 59_940, false)];
        assert_eq!(keep_on_screen(&modes, 0, None, None), 0);
    }

    #[test]
    fn the_default_rule_gives_way_to_a_lit_mode_within_tolerance() {
        let modes = [m(1920, 1080, 60_000, true), m(1920, 1080, 59_940, false)];
        assert_eq!(select_mode(&modes, None), Some(0));
        assert_eq!(keep_on_screen(&modes, 0, Some(1), None), 1);
    }

    #[test]
    fn the_default_rule_never_lowers_the_refresh_to_avoid_a_blank() {
        let modes = [m(3840, 2160, 60_000, true), m(3840, 2160, 30_000, false)];
        assert_eq!(keep_on_screen(&modes, 0, Some(1), None), 0);
    }

    #[test]
    fn a_faster_lit_mode_at_the_same_size_is_kept() {
        // A previous master chose 144 Hz where the EDID prefers 60: keeping it
        // is no worse by the default rule's own standard, and skips a resync.
        let modes = [m(2560, 1440, 60_000, true), m(2560, 1440, 143_998, false)];
        assert_eq!(keep_on_screen(&modes, 0, Some(1), None), 1);
    }

    #[test]
    fn a_different_size_is_never_kept() {
        let modes = [m(1920, 1080, 60_000, true), m(1280, 720, 60_000, false)];
        assert_eq!(keep_on_screen(&modes, 0, Some(1), None), 0);
    }

    #[test]
    fn an_interlaced_lit_mode_is_never_kept() {
        let mut modes = [m(1920, 1080, 60_000, true), m(1920, 1080, 60_000, false)];
        modes[1].interlaced = true;
        assert_eq!(keep_on_screen(&modes, 0, Some(1), None), 0);
    }

    #[test]
    fn an_explicit_refresh_is_an_instruction_not_a_neighbourhood() {
        // `@60` resolves to 60.000; the lit 59.940 is within tolerance of the
        // *request* but is not what it resolved to.
        let modes = [m(1920, 1080, 59_940, false), m(1920, 1080, 60_000, true)];
        let req = size(1920, 1080, Some(60_000));
        let chosen = select_mode(&modes, Some(&req)).unwrap();
        assert_eq!(chosen, 1);
        assert_eq!(keep_on_screen(&modes, chosen, Some(0), Some(&req)), 1);
    }

    #[test]
    fn an_explicit_request_keeps_a_lit_twin_with_the_same_refresh() {
        // Two listings of the same nominal mode (a CEA and a detailed timing
        // with different porches): either answers the request, so the lit one
        // is kept.
        let modes = [m(1920, 1080, 60_000, true), m(1920, 1080, 60_000, false)];
        for req in [
            size(1920, 1080, Some(60_000)),
            size(1920, 1080, None),
            ModeRequest::Max,
            ModeRequest::Fastest,
        ] {
            let chosen = select_mode(&modes, Some(&req)).unwrap();
            let kept = keep_on_screen(&modes, chosen, Some(1), Some(&req));
            assert_eq!(kept, 1, "{req}");
        }
    }

    #[test]
    fn fastest_is_not_satisfied_by_a_slower_lit_mode() {
        let modes = [m(2560, 1440, 60_000, true), m(2560, 1440, 143_998, false)];
        let req = ModeRequest::Fastest;
        let chosen = select_mode(&modes, Some(&req)).unwrap();
        assert_eq!(chosen, 1);
        assert_eq!(keep_on_screen(&modes, chosen, Some(0), Some(&req)), 1);
    }

    #[test]
    fn an_unmatched_request_is_the_default_rule_here_too() {
        // `select_mode` fell back to the default for a size the monitor does
        // not have; the tolerance of the default rule applies again.
        let modes = [m(1920, 1080, 60_000, true), m(1920, 1080, 59_940, false)];
        let req = size(1600, 900, None);
        let chosen = select_mode(&modes, Some(&req)).unwrap();
        assert_eq!(chosen, 0);
        assert_eq!(keep_on_screen(&modes, chosen, Some(1), Some(&req)), 1);
    }

    #[test]
    fn out_of_range_indices_keep_the_pick() {
        let modes = [m(1920, 1080, 60_000, true)];
        assert_eq!(keep_on_screen(&modes, 0, Some(7), None), 0);
        assert_eq!(keep_on_screen(&modes, 7, Some(0), None), 7);
    }

    // -- the box's real mode table ------------------------------------------
    //
    // `sudo cat /sys/kernel/debug/dri/1/i915_display_info` on the test box
    // (Pentium G3240, i915 Haswell), connector HDMI-A-1, verbatim and in
    // the order the kernel lists it. The 148 500 kHz 60 Hz entry is the
    // one flagged PREFERRED; there are two 60 Hz entries, which is exactly
    // the case `@60` has to be unambiguous about. No 240 Hz mode is
    // offered — the link is HDMI 1.4 and 1080p240 needs ~550 MHz.
    fn box_hdmi_a_1() -> Vec<ModeCandidate> {
        vec![
            m(1920, 1080, 120_000, false), // clock 285 500
            m(1920, 1080, 85_000, false),
            m(1920, 1080, 60_000, true), // clock 148 500, PREFERRED
            m(1920, 1080, 60_000, false),
            m(1920, 1080, 50_000, false),
            m(1920, 1080, 24_000, false),
            m(3840, 2160, 30_000, false),
            m(3840, 2160, 25_000, false),
            m(3840, 2160, 24_000, false),
        ]
    }

    fn req(text: &str) -> ModeRequest {
        ModeRequest::parse(text).expect("a request the tests wrote themselves")
    }

    #[test]
    fn the_box_defaults_to_its_preferred_1080p60() {
        let modes = box_hdmi_a_1();
        // Not the 4K mode, although it is four times the area: the
        // connector's PREFERRED flag outranks size, and this is the line
        // that says today's box runs at 60.
        let i = select_mode(&modes, None).expect("a mode");
        assert_eq!(
            (modes[i].width, modes[i].height, modes[i].refresh_mhz),
            (1920, 1080, 60_000)
        );
        assert!(modes[i].preferred);
    }

    #[test]
    fn the_box_can_be_asked_for_120() {
        let modes = box_hdmi_a_1();
        let i = select_mode(&modes, Some(&req("1920x1080@120"))).expect("a mode");
        assert_eq!(modes[i].refresh_mhz, 120_000);
        // And for the other rates it lists.
        for (spec, want) in [
            ("1920x1080@85", 85_000),
            ("1920x1080@50", 50_000),
            ("1920x1080@24", 24_000),
            ("3840x2160@30", 30_000),
        ] {
            let i = select_mode(&modes, Some(&req(spec))).expect("a mode");
            assert_eq!(modes[i].refresh_mhz, want, "{spec}");
        }
    }

    #[test]
    fn sixty_means_sixty_not_fifty_nine_ninety_four() {
        // The case the tolerance exists for: both rates present, `@60`
        // must not round into the 59.94 and `@59.94` must not round into
        // the 60. 60 mHz apart, and the tolerance is 500 — so the
        // *nearest* rule, not a threshold rule, is what decides.
        let modes = [m(1920, 1080, 59_940, false), m(1920, 1080, 60_000, false)];
        assert_eq!(select_mode(&modes, Some(&req("1920x1080@60"))), Some(1));
        assert_eq!(select_mode(&modes, Some(&req("1920x1080@59.94"))), Some(0));
    }

    #[test]
    fn a_size_with_no_rate_takes_the_fastest_at_that_size() {
        let modes = box_hdmi_a_1();
        let i = select_mode(&modes, Some(&req("1920x1080"))).expect("a mode");
        assert_eq!(modes[i].refresh_mhz, 120_000);
        let i = select_mode(&modes, Some(&req("3840x2160"))).expect("a mode");
        assert_eq!(modes[i].refresh_mhz, 30_000);
    }

    #[test]
    fn max_and_fastest_disagree_on_this_box_which_is_why_both_exist() {
        let modes = box_hdmi_a_1();
        // `max` ignores PREFERRED: the 4K panel mode wins on area.
        let i = select_mode(&modes, Some(&req("max"))).expect("a mode");
        assert_eq!(
            (modes[i].width, modes[i].height, modes[i].refresh_mhz),
            (3840, 2160, 30_000)
        );
        // `fastest` keeps the preferred *size* and takes the top rate.
        let i = select_mode(&modes, Some(&req("fastest"))).expect("a mode");
        assert_eq!(
            (modes[i].width, modes[i].height, modes[i].refresh_mhz),
            (1920, 1080, 120_000)
        );
    }

    #[test]
    fn an_unlisted_mode_falls_back_to_the_default_and_is_reportable() {
        let modes = box_hdmi_a_1();
        let wanted = req("2560x1440@144");
        // Nothing matched — which the caller has to be able to see, so it
        // can warn — and yet a mode is still selected.
        assert_eq!(request_match(&modes, &wanted), None);
        let i = select_mode(&modes, Some(&wanted)).expect("the default");
        assert!(modes[i].preferred);
        // A rate this connector does not list at a size it does.
        assert_eq!(request_match(&modes, &req("1920x1080@144")), None);
        // The warning's evidence: exactly the lines a user could write.
        let listed = describe_modes(&modes);
        assert!(
            listed.starts_with("1920x1080@120, 1920x1080@85, 1920x1080@60"),
            "{listed}"
        );
        assert!(listed.ends_with("3840x2160@24"), "{listed}");
        assert_eq!(describe_modes(&[]), "none");
    }

    #[test]
    fn an_explicit_request_never_picks_an_interlaced_mode() {
        let mut modes = [m(1920, 1080, 60_000, false), m(1280, 720, 60_000, true)];
        modes[0].interlaced = true;
        // The default rule would take the progressive 720p; an explicit
        // 1080p60 request finds only the interlaced one, so it does not
        // match at all and falls back rather than lying.
        assert_eq!(request_match(&modes, &req("1920x1080@60")), None);
        assert_eq!(select_mode(&modes, Some(&req("1920x1080@60"))), Some(1));
        assert_eq!(request_match(&modes, &req("max")), Some(1));
    }

    #[test]
    fn a_long_mode_list_is_truncated_in_a_warning() {
        // The box's HDMI connector really lists **45** modes — the EDID
        // runs all the way down to 720x400@70 — and printing them all
        // produced a 1 400-character log line that a reader scrolls past.
        // A warning nobody reads is not a warning.
        let many: Vec<ModeCandidate> = (0..45)
            .map(|i| m(1920 - i * 8, 1080, 60_000, i == 0))
            .collect();
        let text = describe_modes(&many);
        assert!(text.len() < 400, "{} chars is a wall: {text}", text.len());
        assert!(text.starts_with("1920x1080@60, 1912x1080@60"), "{text}");
        assert!(
            text.ends_with("and 33 more (`modes` on the control socket lists them all)"),
            "the tail is counted and points at the command that prints it: {text}"
        );
        // A list that fits is printed whole, with no "and 0 more".
        let few = &many[..MODES_IN_A_WARNING];
        assert!(
            !describe_modes(few).contains("more"),
            "{}",
            describe_modes(few)
        );
    }

    #[test]
    fn the_boxs_real_120_hz_mode_is_119_982_and_at_120_finds_it() {
        // Measured on the box, not assumed: `modes` reports
        // `1920x1080@119.982`, because the 285 500 kHz clock over
        // 2080x1144 is 119.982 Hz and not a round 120. A rule that
        // required an exact match would have failed on the one mode this
        // whole task exists to reach — which is why the match is
        // *nearest within 0.5 Hz* and not equality.
        let modes = [
            m(1920, 1080, 60_000, true),
            m(1920, 1080, 119_982, false),
            m(1920, 1080, 84_904, false),
            m(1920, 1080, 59_940, false),
        ];
        let i = select_mode(&modes, Some(&req("1920x1080@120"))).expect("a mode");
        assert_eq!(modes[i].refresh_mhz, 119_982);
        // 85 likewise: the listed mode is 84.904, 96 mHz off nominal.
        let i = select_mode(&modes, Some(&req("1920x1080@85"))).expect("a mode");
        assert_eq!(modes[i].refresh_mhz, 84_904);
        // And the tolerance still has a floor: 0.5 Hz is not "anything
        // close-ish", so a rate the connector genuinely lacks is refused
        // rather than rounded into its neighbour.
        assert_eq!(request_match(&modes, &req("1920x1080@90")), None);
        assert_eq!(request_match(&modes, &req("1920x1080@119")), None);
    }

    #[test]
    fn a_retime_keeps_the_output_and_a_resize_replaces_it() {
        use Reconcile::{Keep, Replace, Retime};
        let at = |w, h, r| (w, h, r);
        let cur = at(1920, 1080, 60_000);

        // Nothing moved.
        assert_eq!(reconcile_one(cur, Some(cur)), Keep);

        // The case this whole distinction exists for: 1080p60 → 1080p120
        // changes no geometry, so destroying the output would reach the
        // server as an unplug followed by a plug — scene drops it, windows
        // migrate to the primary, every client gets `OutputGone` — for a
        // change the user asked for that moves nothing on screen.
        assert_eq!(reconcile_one(cur, Some(at(1920, 1080, 119_982))), Retime);
        assert_eq!(reconcile_one(cur, Some(at(1920, 1080, 59_940))), Retime);

        // A size change really is a rebuild: the buffers are the wrong
        // size and the clients really do need reconfiguring.
        assert_eq!(reconcile_one(cur, Some(at(1280, 720, 60_000))), Replace);
        assert_eq!(reconcile_one(cur, Some(at(1280, 720, 239_840))), Replace);
        assert_eq!(reconcile_one(cur, Some(at(1920, 1200, 60_000))), Replace);

        // Connector gone, or its CRTC/plane moved under us.
        assert_eq!(reconcile_one(cur, None), Replace);
    }

    #[test]
    fn requests_parse_the_way_people_write_them() {
        assert_eq!(
            req("1920x1080@120"),
            ModeRequest::Size {
                width: 1920,
                height: 1080,
                refresh_mhz: Some(120_000)
            }
        );
        assert_eq!(
            req(" 1280X720 "),
            ModeRequest::Size {
                width: 1280,
                height: 720,
                refresh_mhz: None
            }
        );
        assert_eq!(
            req("1920x1080@59.94"),
            ModeRequest::Size {
                width: 1920,
                height: 1080,
                refresh_mhz: Some(59_940)
            }
        );
        assert_eq!(req("MAX"), ModeRequest::Max);
        assert_eq!(req("Fastest"), ModeRequest::Fastest);
        for bad in [
            "",
            "1920",
            "1920x",
            "1920x1080@",
            "1920x1080@fast",
            "0x1080",
            "1920x1080@0",
            "1920x1080@99999",
            "-1x-1",
        ] {
            assert!(ModeRequest::parse(bad).is_err(), "{bad:?} should not parse");
        }
        // Round-trips through Display, which is what `outputs` and the
        // warnings print.
        for text in [
            "1920x1080@120",
            "1280x720",
            "1920x1080@59.94",
            "max",
            "fastest",
        ] {
            assert_eq!(req(text).to_string(), text);
        }
    }

    #[test]
    fn a_modeline_parses_and_computes_its_own_refresh() {
        // CVT-RB v1 1280x720@240, the experiment the box exists for; see
        // `tmp/cvt_rb.py` in the task and the recipe in `docs/settings.md`.
        let ml = Modeline::parse("279750 1280 1328 1360 1440 720 723 727 810 +hsync -vsync")
            .expect("a well-formed modeline");
        assert_eq!(ml.clock_khz, 279_750);
        assert_eq!((ml.hdisplay, ml.vdisplay), (1280, 720));
        assert!(ml.hsync_positive && !ml.vsync_positive);
        // 279 750 000 000 / (1440 * 810) = 239.840 Hz (integer) — the arithmetic,
        // not a wish: the clock is rounded down to CVT's 0.25 MHz step, so
        // these timings are 239.84 Hz and not 240, and what the server
        // reports has to be what the timings produce rather than what the
        // person asking for them had in mind.
        assert_eq!(ml.refresh_mhz(), 239_840);
        // Sync polarity defaults to +hsync -vsync (what CVT-RB wants) when
        // the line does not say.
        let bare = Modeline::parse("148500 1920 2008 2052 2200 1080 1084 1089 1125").expect("bare");
        assert_eq!(bare.refresh_mhz(), 60_000);
        assert!(bare.hsync_positive && !bare.vsync_positive);
        assert_eq!(
            bare.to_string(),
            "148500 1920 2008 2052 2200 1080 1084 1089 1125 +hsync -vsync"
        );
        for bad in [
            "",
            "148500 1920 2008 2052 2200 1080 1084 1089",
            "0 1920 2008 2052 2200 1080 1084 1089 1125",
            // hsync_start before hdisplay
            "148500 1920 1900 2052 2200 1080 1084 1089 1125",
            // vtotal before vsync_end
            "148500 1920 2008 2052 2200 1080 1084 1089 1000",
            // does not fit in 16 bits
            "148500 99999 99999 99999 99999 1080 1084 1089 1125",
            "148500 1920 2008 2052 2200 1080 1084 1089 1125 +sync",
            "abc 1920 2008 2052 2200 1080 1084 1089 1125",
        ] {
            assert!(Modeline::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn hz_text_is_what_the_config_file_spells() {
        assert_eq!(hz_text(120_000), "120");
        assert_eq!(hz_text(59_940), "59.94");
        assert_eq!(hz_text(239_840), "239.84");
        assert_eq!(hz_text(0), "0");
    }

    #[test]
    fn refresh_matches_kernel_formula() {
        // 1920x1080@60: clock 148500 kHz, htotal 2200, vtotal 1125.
        assert_eq!(
            refresh_millihertz(148_500, 2200, 1125, 0, false, false),
            60_000
        );
        // 1080i: half the clock, interlaced.
        assert_eq!(
            refresh_millihertz(74_250, 2200, 1125, 0, true, false),
            60_000
        );
        // 59.94 Hz variant.
        assert_eq!(
            refresh_millihertz(148_352, 2200, 1125, 0, false, false),
            59_940
        );
        assert_eq!(refresh_millihertz(0, 0, 0, 0, false, false), 0);
        assert_eq!(
            refresh_millihertz(148_500, 2200, 1125, 2, false, true),
            15_000
        );
    }

    fn plane(mask: u32, primary: bool) -> PlaneCandidate {
        PlaneCandidate {
            crtc_mask: mask,
            primary,
        }
    }

    #[test]
    fn assigns_distinct_crtcs_and_primary_planes() {
        // Intel-like: 3 CRTCs, each with primary + cursor, connectors can
        // use any CRTC.
        let planes = [
            plane(0b001, true),
            plane(0b001, false),
            plane(0b010, true),
            plane(0b010, false),
            plane(0b100, true),
            plane(0b100, false),
        ];
        let conns = [
            ConnectorCandidate {
                crtc_mask: 0b111,
                current_crtc: None,
            },
            ConnectorCandidate {
                crtc_mask: 0b111,
                current_crtc: None,
            },
        ];
        let got = assign(&conns, &planes, 0, &[]);
        assert_eq!(
            got,
            vec![
                Some(Assignment { crtc: 0, plane: 0 }),
                Some(Assignment { crtc: 1, plane: 2 }),
            ]
        );
    }

    #[test]
    fn keeps_current_crtc_first() {
        let planes = [plane(0b11, true), plane(0b11, true)];
        let conns = [
            ConnectorCandidate {
                crtc_mask: 0b11,
                current_crtc: None,
            },
            ConnectorCandidate {
                crtc_mask: 0b11,
                current_crtc: Some(0),
            },
        ];
        let got = assign(&conns, &planes, 0, &[]);
        assert_eq!(got[1], Some(Assignment { crtc: 0, plane: 0 }));
        assert_eq!(got[0], Some(Assignment { crtc: 1, plane: 1 }));
    }

    #[test]
    fn respects_used_resources_and_reports_failure() {
        let planes = [plane(0b01, true), plane(0b10, true)];
        let conns = [
            ConnectorCandidate {
                crtc_mask: 0b11,
                current_crtc: Some(0),
            },
            ConnectorCandidate {
                crtc_mask: 0b01,
                current_crtc: None,
            },
        ];
        // CRTC 0 and plane 0 already belong to a kept output.
        let got = assign(&conns, &planes, 0b01, &[true, false]);
        assert_eq!(got[0], Some(Assignment { crtc: 1, plane: 1 }));
        assert_eq!(got[1], None);
    }

    #[test]
    fn crtc_without_primary_plane_is_skipped() {
        let planes = [plane(0b01, false), plane(0b10, true)];
        let conns = [ConnectorCandidate {
            crtc_mask: 0b11,
            current_crtc: Some(0),
        }];
        assert_eq!(
            assign(&conns, &planes, 0, &[]),
            vec![Some(Assignment { crtc: 1, plane: 1 })]
        );
    }
}
