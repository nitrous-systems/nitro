//! Hardware-independent decisions: which mode to use, which CRTC and
//! primary plane drive which connector, refresh arithmetic. Pure functions
//! over plain data so they are unit-tested without a device.

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

/// Index of the mode to use: the preferred one, else the largest area,
/// ties broken by the highest refresh. Interlaced modes lose to any
/// progressive mode. `None` for an empty list.
#[must_use]
pub fn select_mode(modes: &[ModeCandidate]) -> Option<usize> {
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
        assert_eq!(select_mode(&modes), Some(1));
    }

    #[test]
    fn largest_then_fastest_without_preferred() {
        let modes = [
            m(1280, 720, 120_000, false),
            m(1920, 1080, 50_000, false),
            m(1920, 1080, 60_000, false),
            m(1920, 1080, 59_940, false),
        ];
        assert_eq!(select_mode(&modes), Some(2));
        assert_eq!(select_mode(&[]), None);
    }

    #[test]
    fn interlaced_loses_even_when_preferred() {
        let mut modes = [m(1920, 1080, 60_000, true), m(1280, 720, 60_000, false)];
        modes[0].interlaced = true;
        assert_eq!(select_mode(&modes), Some(1));
        // ... unless it is all there is.
        assert_eq!(select_mode(&modes[..1]), Some(0));
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
