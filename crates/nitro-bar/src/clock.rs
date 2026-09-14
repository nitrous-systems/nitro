//! The clock in the bar's centre: the local UTC offset, `HH:MM` formatting,
//! and the delay that keeps the timer on the minute boundary.
//!
//! # Time is an argument
//!
//! Nothing here reads the clock. `format_hm` and [`ms_to_next_minute`] take
//! the instant they are asked about, and the zone is a value parsed from
//! bytes, so every rule in this module — a midnight rollover, a pre-1970
//! instant, the hour a DST transition lands on — is an ordinary assertion in
//! an ordinary unit test. The alternative, a module that calls
//! `clock_gettime` and is tested by mocking it, buys nothing: the mock would
//! be exercising the mock. The caller (`main.rs`) owns the one syscall.
//!
//! It is also deliberately `std`-only, down to hand-rolling the `TZif` parse
//! and the seconds-to-`HH:MM` arithmetic. A date library would be a
//! dependency carrying a calendar, a locale database and a parser for
//! formats the bar never prints; the bar needs the time of day and nothing
//! else, and the time of day is two euclidean divisions.
//!
//! # Why the delay is never zero
//!
//! [`ms_to_next_minute`] returns `1..=60_000`, never 0. A timer that fires
//! *exactly* on the boundary — which is the case the alignment is designed
//! to produce, so it is the common case, not the rare one — has zero
//! milliseconds left in the current minute; rearming it for 0 ms would fire
//! it again immediately and spin the event loop at whatever rate the kernel
//! can deliver timerfd wakeups. The minute that matters at that instant is
//! the *next* one, so a boundary asks for a full `60_000`.

/// The local-time offset from UTC, as a function of the instant.
///
/// Built from the transition table of a `TZif` file, which is the only part of
/// the format the bar needs: the POSIX TZ footer string that extrapolates
/// past the last transition is ignored, because real files carry
/// transitions out to 2037 and a bar showing the wrong hour in 2038 is a
/// problem for 2038.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Transition instants, in unix seconds, ascending.
    transitions: Vec<i64>,
    /// The offset in force *from* the transition at the same index, so the
    /// two vectors always have equal length and the lookup is one
    /// `partition_point` with no second indirection through a type table.
    offsets: Vec<i32>,
    /// The offset in force before the first transition, and the answer for a
    /// table with no transitions at all.
    before: i32,
}

impl Zone {
    /// UTC, with no transitions.
    #[must_use]
    pub fn utc() -> Zone {
        Zone {
            transitions: Vec::new(),
            offsets: Vec::new(),
            before: 0,
        }
    }

    /// Load the local zone from `/etc/localtime`, falling back to UTC when it
    /// cannot be read or parsed.
    ///
    /// A missing or corrupt zone file is not worth failing a bar over: an
    /// hour of offset error is a cosmetic bug, an absent clock is a visible
    /// one. `TZ` is deliberately not honoured — the bar is a desktop
    /// component, not a shell command, and the user's zone is the machine's.
    #[must_use]
    pub fn local() -> Zone {
        std::fs::read("/etc/localtime")
            .ok()
            .and_then(|bytes| Zone::parse(&bytes))
            .unwrap_or_else(Zone::utc)
    }

    /// Parse `TZif` bytes; `None` when they are not `TZif`, or are truncated or
    /// otherwise malformed.
    ///
    /// Every read is bounds-checked and every count is validated against the
    /// remaining length before anything is allocated, so a hostile or
    /// corrupt `/etc/localtime` costs an error, not a panic and not a
    /// gigabyte of `Vec`.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Zone> {
        let head = Header::parse(bytes, 0)?;
        if head.version == b'\0' {
            return Zone::parse_block(bytes, head.end, &head, 4);
        }
        // Version 2+ repeats the whole file with 64-bit transition times,
        // and that second block is the real one: the 32-bit block exists
        // only so a v1 reader sees something, and on a 64-bit-time file it
        // is often empty or truncated to the 2038 window.
        let second = head.end.checked_add(head.data_len(4)?)?;
        let head2 = Header::parse(bytes, second)?;
        Zone::parse_block(bytes, head2.end, &head2, 8)
    }

    /// Offset from UTC in seconds, in force at `unix` seconds.
    #[must_use]
    pub fn offset_at(&self, unix: i64) -> i32 {
        // The last transition at or before `unix`; `partition_point` on the
        // sorted table is the same search a hand-written loop would do, but
        // without the off-by-one.
        let idx = self.transitions.partition_point(|&t| t <= unix);
        match idx.checked_sub(1).and_then(|i| self.offsets.get(i)) {
            Some(&off) => off,
            None => self.before,
        }
    }

    /// Parse one data block, with `time_size`-byte transition times (4 in a
    /// v1 block, 8 in a v2+ one), starting at `at`.
    fn parse_block(bytes: &[u8], at: usize, head: &Header, time_size: usize) -> Option<Zone> {
        let timecnt = usize::try_from(head.timecnt).ok()?;
        let typecnt = usize::try_from(head.typecnt).ok()?;
        // A block with no local-time types cannot answer any query, so it is
        // malformed rather than empty.
        if typecnt == 0 {
            return None;
        }
        // Check the whole block is present before allocating anything: the
        // counts come from the file, and the length check is what bounds
        // them.
        let end = at.checked_add(head.data_len(time_size)?)?;
        bytes.get(at..end)?;

        let types_at = at.checked_add(timecnt.checked_mul(time_size)?)?;
        let typedefs_at = types_at.checked_add(timecnt)?;

        // utoff per local-time type, plus the index of the first standard
        // (non-DST) type, which is what applies before the first transition.
        let mut utoffs = Vec::with_capacity(typecnt);
        let mut first_std = None;
        for i in 0..typecnt {
            let base = typedefs_at.checked_add(i.checked_mul(6)?)?;
            utoffs.push(be_i32(bytes, base)?);
            if bytes.get(base.checked_add(4)?)? == &0 && first_std.is_none() {
                first_std = Some(i);
            }
        }

        let mut transitions = Vec::with_capacity(timecnt);
        let mut offsets = Vec::with_capacity(timecnt);
        for i in 0..timecnt {
            let t = if time_size == 8 {
                be_i64(bytes, at.checked_add(i.checked_mul(8)?)?)?
            } else {
                i64::from(be_i32(bytes, at.checked_add(i.checked_mul(4)?)?)?)
            };
            let ty = usize::from(*bytes.get(types_at.checked_add(i)?)?);
            // An out-of-range type index is corruption; type 0 is the
            // conventional fallback and keeps the table usable.
            let off = utoffs.get(ty).or_else(|| utoffs.first())?;
            transitions.push(t);
            offsets.push(*off);
        }

        let before = *utoffs
            .get(first_std.unwrap_or(0))
            .or_else(|| utoffs.first())?;
        Some(Zone {
            transitions,
            offsets,
            before,
        })
    }
}

/// A `TZif` header: the six counts, plus where the data block after it starts.
struct Header {
    version: u8,
    isutcnt: u32,
    isstdcnt: u32,
    leapcnt: u32,
    timecnt: u32,
    typecnt: u32,
    charcnt: u32,
    /// Offset of the first byte after this header.
    end: usize,
}

impl Header {
    /// Parse the 44-byte header at `at`, rejecting a bad magic or a version
    /// byte this parser has no layout for.
    fn parse(bytes: &[u8], at: usize) -> Option<Header> {
        let end = at.checked_add(44)?;
        let h = bytes.get(at..end)?;
        if h.get(..4)? != b"TZif" {
            return None;
        }
        let version = *h.get(4)?;
        // v2, v3 and v4 differ only in the footer and the leap-second rules,
        // neither of which this parser touches, so they share a branch.
        if !matches!(version, b'\0' | b'2' | b'3' | b'4') {
            return None;
        }
        Some(Header {
            version,
            isutcnt: be_u32(bytes, at.checked_add(20)?)?,
            isstdcnt: be_u32(bytes, at.checked_add(24)?)?,
            leapcnt: be_u32(bytes, at.checked_add(28)?)?,
            timecnt: be_u32(bytes, at.checked_add(32)?)?,
            typecnt: be_u32(bytes, at.checked_add(36)?)?,
            charcnt: be_u32(bytes, at.checked_add(40)?)?,
            end,
        })
    }

    /// Byte length of the data block this header describes, with
    /// `time_size`-byte transition times and leap-second instants.
    ///
    /// All checked: the counts are attacker-controlled `u32`s and a product
    /// of two of them overflows `usize` on a 32-bit target.
    fn data_len(&self, time_size: usize) -> Option<usize> {
        let timecnt = usize::try_from(self.timecnt).ok()?;
        let typecnt = usize::try_from(self.typecnt).ok()?;
        let leapcnt = usize::try_from(self.leapcnt).ok()?;
        let charcnt = usize::try_from(self.charcnt).ok()?;
        let isstdcnt = usize::try_from(self.isstdcnt).ok()?;
        let isutcnt = usize::try_from(self.isutcnt).ok()?;
        // A leap-second record is a transition time plus a 4-byte count.
        let leap_size = time_size.checked_add(4)?;
        timecnt
            .checked_mul(time_size)?
            .checked_add(timecnt)?
            .checked_add(typecnt.checked_mul(6)?)?
            .checked_add(charcnt)?
            .checked_add(leapcnt.checked_mul(leap_size)?)?
            .checked_add(isstdcnt)?
            .checked_add(isutcnt)
    }
}

/// Big-endian `u32` at `at`, or `None` if it does not fit.
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let s = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(<[u8; 4]>::try_from(s).ok()?))
}

/// Big-endian `i32` at `at`, or `None` if it does not fit.
fn be_i32(bytes: &[u8], at: usize) -> Option<i32> {
    let s = bytes.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes(<[u8; 4]>::try_from(s).ok()?))
}

/// Big-endian `i64` at `at`, or `None` if it does not fit.
fn be_i64(bytes: &[u8], at: usize) -> Option<i64> {
    let s = bytes.get(at..at.checked_add(8)?)?;
    Some(i64::from_be_bytes(<[u8; 8]>::try_from(s).ok()?))
}

/// Hour and minute of the day for a local-time-seconds value.
///
/// Euclidean, so a pre-1970 instant (negative seconds) gives the same answer
/// a positive one would: truncating division would put 23:59 on the wrong
/// side of the epoch.
#[must_use]
pub fn hm_of(local_secs: i64) -> (u32, u32) {
    let day = local_secs.rem_euclid(86_400);
    // `rem_euclid` bounds `day` to 0..86_400, so both quotients fit a `u32`.
    let hour = (day / 3_600) as u32;
    let minute = (day % 3_600 / 60) as u32;
    (hour, minute)
}

/// Local wall-clock time at `unix` seconds as `HH:MM`, 24-hour, zero-padded.
///
/// 24-hour and unlocalised on purpose: the bar has a fixed-width slot in the
/// centre section, and `%H:%M` is the only format whose width does not
/// change at 1 pm or in another language.
#[must_use]
pub fn format_hm(unix: i64, zone: &Zone) -> String {
    // Saturating: only reachable with an absurd `unix` near `i64::MAX`, and
    // a clamped hour beats a release-mode wrap (or a debug-mode panic).
    let local = unix.saturating_add(i64::from(zone.offset_at(unix)));
    let (h, m) = hm_of(local);
    format!("{h:02}:{m:02}")
}

/// Milliseconds from `unix_ms` until the next whole minute: always
/// `1..=60_000`, never 0.
///
/// See the module docs for why a boundary returns a full minute rather than
/// 0 — a 0 ms rearm turns the aligned timer into a busy loop.
#[must_use]
pub fn ms_to_next_minute(unix_ms: i64) -> u64 {
    let into_minute = unix_ms.rem_euclid(60_000);
    // `rem_euclid` gives 0..60_000, so the difference is 1..=60_000 and the
    // cast is exact.
    (60_000 - into_minute) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local-time type: utoff, isdst, designation index.
    struct Ty(i32, u8, u8);

    /// Build a v1 `TZif` body (header + one 32-bit data block).
    fn tzif_v1(transitions: &[(i32, u8)], types: &[Ty]) -> Vec<u8> {
        let mut b = header(b'\0', transitions.len(), types.len());
        for (t, _) in transitions {
            b.extend_from_slice(&t.to_be_bytes());
        }
        for (_, i) in transitions {
            b.push(*i);
        }
        for Ty(off, dst, idx) in types {
            b.extend_from_slice(&off.to_be_bytes());
            b.push(*dst);
            b.push(*idx);
        }
        b.extend_from_slice(b"UTC\0"); // charcnt bytes of designations
        b
    }

    /// Build a v2 `TZif`: a v1 block (which must be *ignored*) followed by a
    /// second header and a 64-bit block, plus a footer.
    fn tzif_v2(
        transitions: &[(i64, u8)],
        types: &[Ty],
        v1: &[(i32, u8)],
        v1_types: &[Ty],
    ) -> Vec<u8> {
        let mut b = tzif_v1(v1, v1_types);
        b[4] = b'2';
        let mut second = header(b'2', transitions.len(), types.len());
        for (t, _) in transitions {
            second.extend_from_slice(&t.to_be_bytes());
        }
        for (_, i) in transitions {
            second.push(*i);
        }
        for Ty(off, dst, idx) in types {
            second.extend_from_slice(&off.to_be_bytes());
            second.push(*dst);
            second.push(*idx);
        }
        second.extend_from_slice(b"UTC\0");
        b.extend_from_slice(&second);
        b.extend_from_slice(b"\nUTC0\n"); // footer, deliberately ignored
        b
    }

    /// The 44-byte header: magic, version, reserved, six counts. `charcnt`
    /// is 4 to match the `b"UTC\0"` the builders append.
    fn header(version: u8, timecnt: usize, typecnt: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"TZif");
        b.push(version);
        b.extend_from_slice(&[0u8; 15]);
        for n in [0u32, 0, 0, timecnt as u32, typecnt as u32, 4] {
            b.extend_from_slice(&n.to_be_bytes());
        }
        b
    }

    #[test]
    fn midnight_and_noon_format_as_zero_padded_hours() {
        let utc = Zone::utc();
        assert_eq!(format_hm(0, &utc), "00:00");
        assert_eq!(format_hm(43_200, &utc), "12:00");
        assert_eq!(format_hm(86_399, &utc), "23:59");
        assert_eq!(format_hm(86_400, &utc), "00:00");
        assert_eq!(format_hm(3_600 + 540, &utc), "01:09");
    }

    #[test]
    fn a_pre_1970_instant_formats_by_euclidean_division() {
        let utc = Zone::utc();
        // One second before the epoch is 23:59:59 on the last day of 1969.
        assert_eq!(format_hm(-1, &utc), "23:59");
        assert_eq!(format_hm(-86_400, &utc), "00:00");
        assert_eq!(format_hm(-86_401, &utc), "23:59");
        // Apollo 11 launch, 1969-07-16 13:32 UTC.
        assert_eq!(format_hm(-14_552_880, &utc), "13:32");
    }

    #[test]
    fn a_positive_zone_offset_moves_the_clock_forward_across_midnight() {
        let zone = Zone {
            transitions: Vec::new(),
            offsets: Vec::new(),
            before: 5 * 3_600 + 1_800,
        };
        // 20:00 UTC + 05:30 wraps into the next day.
        assert_eq!(format_hm(72_000, &zone), "01:30");
    }

    #[test]
    fn a_negative_zone_offset_moves_the_clock_back_across_midnight() {
        let zone = Zone {
            transitions: Vec::new(),
            offsets: Vec::new(),
            before: -8 * 3_600,
        };
        // 04:00 UTC - 08:00 is the previous evening.
        assert_eq!(format_hm(14_400, &zone), "20:00");
        assert_eq!(format_hm(0, &zone), "16:00");
    }

    #[test]
    fn a_timer_landing_exactly_on_the_boundary_waits_a_whole_minute() {
        assert_eq!(ms_to_next_minute(0), 60_000);
        assert_eq!(ms_to_next_minute(1_700_000_040_000), 60_000);
    }

    #[test]
    fn one_millisecond_after_the_boundary_waits_almost_a_minute() {
        assert_eq!(ms_to_next_minute(1), 59_999);
        assert_eq!(ms_to_next_minute(1_700_000_040_001), 59_999);
    }

    #[test]
    fn one_millisecond_before_the_boundary_waits_one_millisecond() {
        assert_eq!(ms_to_next_minute(59_999), 1);
        assert_eq!(ms_to_next_minute(1_700_000_039_999), 1);
    }

    #[test]
    fn mid_minute_and_negative_times_stay_within_one_minute() {
        assert_eq!(ms_to_next_minute(30_000), 30_000);
        assert_eq!(ms_to_next_minute(-1), 1);
        assert_eq!(ms_to_next_minute(-60_000), 60_000);
        assert_eq!(ms_to_next_minute(-30_001), 30_001);
        for ms in [i64::MIN, i64::MAX, -7, 12_345, 1_699_999_999_999] {
            let d = ms_to_next_minute(ms);
            assert!((1..=60_000).contains(&d), "{ms} gave {d}");
        }
    }

    #[test]
    fn a_v1_tzif_yields_the_offsets_of_its_transitions() {
        let bytes = tzif_v1(&[(1_000, 1), (2_000, 0)], &[Ty(0, 0, 0), Ty(3_600, 1, 0)]);
        let zone = Zone::parse(&bytes).expect("valid v1 TZif");
        assert_eq!(zone.offset_at(999), 0);
        assert_eq!(zone.offset_at(1_000), 3_600);
        assert_eq!(zone.offset_at(1_999), 3_600);
        assert_eq!(zone.offset_at(2_000), 0);
    }

    #[test]
    fn a_v2_tzif_is_read_from_its_64_bit_block_not_the_v1_one() {
        // The v1 block claims +9h from 1970; the 64-bit block says +2h from
        // a date the 32-bit block could not even express.
        let bytes = tzif_v2(
            &[(4_000_000_000, 1)],
            &[Ty(0, 0, 0), Ty(7_200, 0, 0)],
            &[(0, 1)],
            &[Ty(0, 0, 0), Ty(32_400, 0, 0)],
        );
        let zone = Zone::parse(&bytes).expect("valid v2 TZif");
        assert_eq!(zone.offset_at(0), 0);
        assert_eq!(zone.offset_at(3_999_999_999), 0);
        assert_eq!(zone.offset_at(4_000_000_000), 7_200);
        assert_eq!(format_hm(4_000_000_000, &zone), "09:06");
    }

    #[test]
    fn the_offset_before_the_first_transition_is_the_first_standard_type() {
        // Type 0 is DST here, so the pre-transition offset must come from
        // type 1, the first non-DST one.
        let bytes = tzif_v1(&[(500, 0)], &[Ty(7_200, 1, 0), Ty(3_600, 0, 0)]);
        let zone = Zone::parse(&bytes).expect("valid v1 TZif");
        assert_eq!(zone.offset_at(0), 3_600);
        assert_eq!(zone.offset_at(499), 3_600);
        assert_eq!(zone.offset_at(500), 7_200);
    }

    #[test]
    fn the_offset_after_the_last_transition_is_that_transitions_type() {
        let bytes = tzif_v1(
            &[(100, 1), (200, 2), (300, 0)],
            &[Ty(0, 0, 0), Ty(3_600, 1, 0), Ty(-3_600, 0, 0)],
        );
        let zone = Zone::parse(&bytes).expect("valid v1 TZif");
        assert_eq!(zone.offset_at(150), 3_600);
        assert_eq!(zone.offset_at(250), -3_600);
        assert_eq!(zone.offset_at(300), 0);
        assert_eq!(zone.offset_at(i64::MAX), 0);
    }

    #[test]
    fn a_tzif_with_no_transitions_uses_its_first_type_everywhere() {
        let bytes = tzif_v1(&[], &[Ty(-18_000, 0, 0)]);
        let zone = Zone::parse(&bytes).expect("valid v1 TZif");
        assert_eq!(zone.offset_at(i64::MIN), -18_000);
        assert_eq!(zone.offset_at(0), -18_000);
        assert_eq!(zone.offset_at(i64::MAX), -18_000);
    }

    #[test]
    fn a_truncated_tzif_is_rejected_rather_than_panicking() {
        let full = tzif_v1(&[(1_000, 1), (2_000, 0)], &[Ty(0, 0, 0), Ty(3_600, 1, 0)]);
        for cut in 0..full.len() {
            assert!(
                Zone::parse(&full[..cut]).is_none(),
                "accepted {cut} of {} bytes",
                full.len()
            );
        }
        assert!(Zone::parse(&full).is_some());

        let v2 = tzif_v2(&[(0, 1)], &[Ty(0, 0, 0), Ty(60, 0, 0)], &[], &[Ty(0, 0, 0)]);
        for cut in 0..v2.len() - 6 {
            assert!(
                Zone::parse(&v2[..cut]).is_none(),
                "accepted {cut} of {} bytes",
                v2.len()
            );
        }
    }

    #[test]
    fn garbage_and_bogus_counts_are_rejected_rather_than_panicking() {
        assert!(Zone::parse(b"").is_none());
        assert!(Zone::parse(b"not a zone file at all, just some bytes").is_none());
        assert!(Zone::parse(&[0xff; 200]).is_none());

        // Right magic, version this parser has no layout for.
        let mut bad_version = tzif_v1(&[], &[Ty(0, 0, 0)]);
        bad_version[4] = b'9';
        assert!(Zone::parse(&bad_version).is_none());

        // Counts that would overflow a naive length computation.
        let mut huge = tzif_v1(&[], &[Ty(0, 0, 0)]);
        huge[32..36].copy_from_slice(&u32::MAX.to_be_bytes()); // timecnt
        huge[36..40].copy_from_slice(&u32::MAX.to_be_bytes()); // typecnt
        assert!(Zone::parse(&huge).is_none());

        // typecnt == 0: nothing can answer a query.
        let no_types = tzif_v1(&[], &[]);
        assert!(Zone::parse(&no_types).is_none());
    }

    #[test]
    fn the_real_localtime_parses_to_a_plausible_offset_when_it_exists() {
        let Ok(bytes) = std::fs::read("/etc/localtime") else {
            return; // Containers and minimal images have none; not a failure.
        };
        let Some(zone) = Zone::parse(&bytes) else {
            return; // Some images ship a symlink-ish stub; UTC fallback covers it.
        };
        for unix in [0, 1_700_000_000, 2_000_000_000, -1_000_000_000] {
            let off = zone.offset_at(unix);
            assert!(
                (-14 * 3_600..=14 * 3_600).contains(&off),
                "implausible offset {off}"
            );
        }
    }

    #[test]
    fn local_falls_back_to_utc_and_always_formats_something() {
        let zone = Zone::local();
        let s = format_hm(1_700_000_000, &zone);
        assert_eq!(s.len(), 5);
        assert_eq!(s.as_bytes()[2], b':');
        assert!(
            s.bytes()
                .enumerate()
                .all(|(i, c)| i == 2 || c.is_ascii_digit())
        );
    }

    #[test]
    fn hm_of_splits_a_day_into_hours_and_minutes() {
        assert_eq!(hm_of(0), (0, 0));
        assert_eq!(hm_of(59), (0, 0));
        assert_eq!(hm_of(60), (0, 1));
        assert_eq!(hm_of(86_399), (23, 59));
        assert_eq!(hm_of(-60), (23, 59));
        assert_eq!(hm_of(i64::MIN), hm_of(i64::MIN.rem_euclid(86_400)));
    }
}
