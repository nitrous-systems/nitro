//! Battery, CPU load and memory readouts for the bar's right-hand section.
//!
//! # Pure formatters, thin readers
//!
//! Every readout here is two things: a *formatter* that takes the file's
//! text as `&str` and returns the string the bar draws, and a *reader* that
//! does nothing but open the file and hand the bytes to the formatter. The
//! split is what makes the parsers testable: the shapes we parse are
//! `/proc/loadavg`, `/proc/meminfo` and `/sys/class/power_supply/*`, and a
//! test that needed a real battery would only ever run on somebody's
//! laptop. Fed as literal fixtures, the same parsers are exercised
//! everywhere, including the machine with no battery at all.
//!
//! # Nothing beats a wrong number
//!
//! Each reader returns `Option<String>`, and `None` means *leave the widget
//! empty* rather than draw a placeholder or a zero. A bar that says `0%`
//! on a desktop, or `0.0/0.0G` because `/proc/meminfo` was truncated under
//! us, is actively misleading — the user would go looking for the fault in
//! their machine instead of in the bar. So every parse is defensive and
//! every failure is silence: truncated files, missing keys, non-numeric
//! fields, an empty directory or a `/sys` that is not mounted all yield
//! `None`, and none of them panic.
//!
//! Reading is `std`-only (`std::fs::read_to_string` plus `read_dir`). These
//! are small virtual files polled once every five seconds, so the blocking
//! read and the `String` allocation are free at this rate, and staying off
//! the kernel-ABI crates keeps the bar's dependency list honest.

use std::fs;
use std::num::NonZeroU64;
use std::path::Path;

/// Where the kernel exports power-supply devices.
const POWER_SUPPLY: &str = "/sys/class/power_supply";

/// kB per GiB — `/proc/meminfo` counts in kB, we render GiB.
const KB_PER_GIB: u64 = 1024 * 1024;

/// kB per MiB.
const KB_PER_MIB: u64 = 1024;

/// The battery percentage and charge state, e.g. `"87%+"`.
///
/// Scans `/sys/class/power_supply` for the first device whose `type` is
/// `Battery` (in name order, so `BAT0` wins over `BAT1` and the readout
/// does not flip between polls with directory order), and formats its
/// `capacity` and `status`. `None` when there is no battery.
#[must_use]
pub fn battery() -> Option<String> {
    // Name order, not `read_dir` order: the latter is whatever the kernel
    // hands back and would make a two-battery machine's readout unstable.
    let mut dirs: Vec<_> = fs::read_dir(POWER_SUPPLY)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    dirs.sort();

    for dir in dirs {
        // `Mains` and `USB` supplies live in the same directory and have a
        // `status` but no meaningful `capacity`; skip anything not a battery.
        if read_field(&dir, "type").as_deref() != Some("Battery") {
            continue;
        }
        // A driver may omit `status`; the percentage alone is still worth
        // showing, and `Unknown` simply renders without a marker.
        let status = read_field(&dir, "status").unwrap_or_else(|| "Unknown".to_owned());

        if let Some(capacity) = read_field(&dir, "capacity")
            && let Some(out) = format_battery(&capacity, &status)
        {
            return Some(out);
        }
        // Some drivers export only the raw gauge. Both spellings occur:
        // energy (µWh) on most laptops, charge (µAh) on others.
        for (now, full) in [("energy_now", "energy_full"), ("charge_now", "charge_full")] {
            if let Some(now) = read_field(&dir, now)
                && let Some(full) = read_field(&dir, full)
                && let Some(out) = format_battery_from_gauge(&now, &full, &status)
            {
                return Some(out);
            }
        }
    }
    None
}

/// The 1-minute CPU load average, e.g. `"0.42"`.
///
/// `None` if `/proc/loadavg` is unreadable or does not start with a number.
#[must_use]
pub fn load() -> Option<String> {
    format_load(&fs::read_to_string("/proc/loadavg").ok()?)
}

/// Used and total memory, e.g. `"1.2/3.3G"`.
///
/// `None` if `/proc/meminfo` is unreadable or lacks the two keys.
#[must_use]
pub fn memory() -> Option<String> {
    format_memory(&fs::read_to_string("/proc/meminfo").ok()?)
}

/// Read one whitespace-trimmed field file out of a power-supply directory.
fn read_field(dir: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let trimmed = text.trim();
    // An empty file is a missing value, not the empty string: a driver that
    // has nothing to report writes nothing.
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Format the contents of `/proc/loadavg`: the 1-minute average, e.g. `"0.42"`.
///
/// The field is passed through *verbatim* rather than parsed to a float and
/// re-printed: the kernel already writes it as `%.2f`, so reprinting can
/// only lose information (`1.00` would come back as `1`) and would make the
/// readout's width depend on float formatting.
#[must_use]
pub fn format_load(loadavg: &str) -> Option<String> {
    let field = loadavg.split_whitespace().next()?;
    // Verbatim pass-through still has to be *validated*, or a garbage or
    // truncated file would end up drawn on the bar as-is.
    if is_decimal(field) {
        Some(field.to_owned())
    } else {
        None
    }
}

/// Format the contents of `/proc/meminfo` as used/total, e.g. `"1.2/3.3G"`.
///
/// Used is `MemTotal - MemAvailable`, not `MemTotal - MemFree`: `MemFree`
/// counts the page cache and reclaimable slab as *used*, which is the
/// classic wrong answer — it makes a healthy Linux box look permanently
/// out of memory. `MemAvailable` is the kernel's own estimate of what a
/// new allocation could get, so `total - available` is what a user means
/// by "in use".
///
/// Rendered in GiB with one decimal when the total is at least 1 GiB,
/// otherwise in MiB with no decimal — one decimal of a GiB is ~100 MiB of
/// resolution, which is all a 32 px bar can usefully say.
#[must_use]
pub fn format_memory(meminfo: &str) -> Option<String> {
    let total_kb = meminfo_field(meminfo, "MemTotal")?;
    let available_kb = meminfo_field(meminfo, "MemAvailable")?;
    // A zero total is a nonsense file, and `0.0/0.0G` would read as a real
    // measurement; also it is the divisor below.
    if total_kb == 0 {
        return None;
    }
    // Saturating: `MemAvailable` can exceed `MemTotal` transiently on some
    // kernels, and "negative used" must not wrap into a huge number.
    let used_kb = total_kb.saturating_sub(available_kb);

    Some(if total_kb >= KB_PER_GIB {
        format!(
            "{:.1}/{:.1}G",
            used_kb as f64 / KB_PER_GIB as f64,
            total_kb as f64 / KB_PER_GIB as f64
        )
    } else {
        format!("{}/{}M", kb_to_mib(used_kb), kb_to_mib(total_kb))
    })
}

/// Format one battery's `capacity` (integer percent) and `status`.
///
/// e.g. `"87%"`, `"87%+"` while charging, `"100%="` when full.
#[must_use]
pub fn format_battery(capacity: &str, status: &str) -> Option<String> {
    // Parsed as signed and clamped, not rejected: firmware gauges do report
    // 101 or a negative sample, and dropping the whole readout over one bad
    // poll would make the widget blink out. Non-numeric text is a different
    // thing — that means we are not looking at a capacity at all.
    let percent = capacity.trim().parse::<i64>().ok()?.clamp(0, 100);
    Some(format!("{percent}%{}", state_marker(status)))
}

/// Format a battery from the raw gauge pair (`energy_now`/`energy_full`, or
/// the `charge_*` spelling) for drivers that export no `capacity`.
///
/// The units cancel in the ratio, so it does not matter which pair it is.
#[must_use]
pub fn format_battery_from_gauge(now: &str, full: &str, status: &str) -> Option<String> {
    let now = now.trim().parse::<u64>().ok()?;
    // Parsed as `NonZero`, which rejects the broken gauge that reports a
    // full charge of 0 (that is not a 0% battery, it is no reading at all)
    // and makes the divide below unconditionally safe.
    let full = full.trim().parse::<NonZeroU64>().ok()?;
    // Rounded to nearest (the `full/2` bias), and clamped because
    // `now > full` happens on a fresh pack.
    let percent = ((now.saturating_mul(100) + full.get() / 2) / full).min(100);
    Some(format!("{percent}%{}", state_marker(status)))
}

/// The one-character state marker appended to the percentage.
///
/// Only the two states a user acts on get a mark: `+` for charging (the
/// number is going up) and `=` for full (it has stopped there). Discharging
/// is the common case, so marking it would be noise on a bar this small,
/// and `Not charging` — plugged in but held below full by a charge
/// threshold — is left bare for the same reason: the percentage is not
/// moving and nothing is wrong. ASCII only, because the bar's font is not
/// guaranteed to carry arrows or battery glyphs.
fn state_marker(status: &str) -> &'static str {
    match status.trim() {
        "Charging" => "+",
        "Full" => "=",
        _ => "",
    }
}

/// The value of one `Key:  <number> kB` line of `/proc/meminfo`, in kB.
fn meminfo_field(meminfo: &str, key: &str) -> Option<u64> {
    for line in meminfo.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        // Exact match on the key: a prefix test would let `MemAvailable`
        // answer a lookup for `Mem`, and `MemTotal` is not `MemTotal2`.
        if name.trim() != key {
            continue;
        }
        // `Key: 3290112 kB` — take the number and ignore the unit, which is
        // kB for every field we read.
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

/// Is this the `%.2f`-ish decimal the kernel writes (no sign, no exponent)?
fn is_decimal(s: &str) -> bool {
    let mut digits = 0usize;
    let mut dots = 0usize;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c == '.' {
            dots += 1;
        } else {
            return false;
        }
    }
    digits > 0 && dots <= 1
}

/// kB to MiB, rounded to nearest so 1.6 MiB does not read as 1 MiB.
fn kb_to_mib(kb: u64) -> u64 {
    (kb + KB_PER_MIB / 2) / KB_PER_MIB
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The head of a real `/proc/meminfo` (3.1 GiB box), trimmed to the
    /// fields we read plus enough neighbours to catch sloppy key matching.
    const MEMINFO: &str = "\
MemTotal:        3290112 kB
MemFree:          204800 kB
MemAvailable:     1994240 kB
Buffers:           41216 kB
Cached:          1503232 kB
SwapCached:            0 kB
Active:          1122304 kB
";

    /// A real `/proc/loadavg` line: three averages, running/total, last pid.
    const LOADAVG: &str = "0.42 0.31 0.28 2/412 30518\n";

    #[test]
    fn a_loadavg_line_yields_its_first_field_verbatim() {
        assert_eq!(format_load(LOADAVG).as_deref(), Some("0.42"));
    }

    #[test]
    fn a_whole_load_keeps_its_trailing_zeroes() {
        // The point of passing the field through: re-printing a parsed
        // float would render this as "1".
        assert_eq!(
            format_load("1.00 0.98 0.71 1/400 9\n").as_deref(),
            Some("1.00")
        );
    }

    #[test]
    fn a_load_above_ten_is_still_just_the_field() {
        assert_eq!(
            format_load("13.37 9.00 4.20 8/500 1\n").as_deref(),
            Some("13.37")
        );
    }

    #[test]
    fn an_empty_loadavg_yields_nothing() {
        assert_eq!(format_load(""), None);
        assert_eq!(format_load("   \n"), None);
    }

    #[test]
    fn a_garbage_loadavg_yields_nothing() {
        assert_eq!(format_load("not a number at all\n"), None);
        assert_eq!(format_load("1.2.3 0.1 0.1\n"), None);
        assert_eq!(format_load("-0.42 0.31 0.28\n"), None);
        assert_eq!(format_load("1e3 0.31 0.28\n"), None);
        assert_eq!(format_load("NaN 0.31 0.28\n"), None);
    }

    #[test]
    fn a_loadavg_truncated_mid_line_still_yields_the_first_field() {
        // A partial read is a real possibility and the first field, once
        // followed by a space, is complete.
        assert_eq!(format_load("0.42 0.3").as_deref(), Some("0.42"));
    }

    #[test]
    fn a_loadavg_truncated_inside_the_first_field_is_reported_as_read() {
        // "0.4" is a well-formed decimal; there is no way to tell it from a
        // genuine reading, and being one hundredth off for one poll is not
        // worth blanking the widget for.
        assert_eq!(format_load("0.4").as_deref(), Some("0.4"));
    }

    #[test]
    fn a_meminfo_yields_total_minus_available_in_gibibytes() {
        // 3290112 kB total, 1994240 kB available => 1295872 kB used
        // => 1.236 GiB used of 3.137 GiB.
        assert_eq!(format_memory(MEMINFO).as_deref(), Some("1.2/3.1G"));
    }

    #[test]
    fn memory_used_ignores_memfree_and_the_page_cache() {
        // MemFree alone (204800 kB) would report 2.9/3.1G, the classic
        // wrong answer; MemAvailable gives 1.2/3.1G.
        let out = format_memory(MEMINFO).unwrap();
        assert_eq!(out, "1.2/3.1G");
        assert_ne!(out, "2.9/3.1G");
    }

    #[test]
    fn a_small_total_is_rendered_in_mebibytes() {
        let meminfo = "MemTotal:         524288 kB\nMemAvailable:     131072 kB\n";
        // 512 MiB total, 128 MiB available => 384 MiB used.
        assert_eq!(format_memory(meminfo).as_deref(), Some("384/512M"));
    }

    #[test]
    fn a_total_of_exactly_one_gibibyte_is_rendered_in_gibibytes() {
        let meminfo = "MemTotal:        1048576 kB\nMemAvailable:     524288 kB\n";
        assert_eq!(format_memory(meminfo).as_deref(), Some("0.5/1.0G"));
    }

    #[test]
    fn a_meminfo_without_memavailable_yields_nothing() {
        let meminfo = "MemTotal:        3290112 kB\nMemFree:          204800 kB\n";
        assert_eq!(format_memory(meminfo), None);
    }

    #[test]
    fn a_meminfo_without_memtotal_yields_nothing() {
        let meminfo = "MemFree:          204800 kB\nMemAvailable:    1994240 kB\n";
        assert_eq!(format_memory(meminfo), None);
    }

    #[test]
    fn a_meminfo_with_a_zero_total_yields_nothing() {
        let meminfo = "MemTotal:              0 kB\nMemAvailable:          0 kB\n";
        assert_eq!(format_memory(meminfo), None);
    }

    #[test]
    fn a_meminfo_truncated_mid_value_yields_nothing() {
        let meminfo = "MemTotal:        3290112 kB\nMemAvailable:";
        assert_eq!(format_memory(meminfo), None);
    }

    #[test]
    fn a_meminfo_with_a_non_numeric_value_yields_nothing() {
        let meminfo = "MemTotal:        lots of kB\nMemAvailable:     1994240 kB\n";
        assert_eq!(format_memory(meminfo), None);
    }

    #[test]
    fn an_empty_or_garbage_meminfo_yields_nothing() {
        assert_eq!(format_memory(""), None);
        assert_eq!(format_memory("not a meminfo at all\n"), None);
        assert_eq!(format_memory("MemTotal\nMemAvailable\n"), None);
    }

    #[test]
    fn a_key_is_matched_whole_and_not_by_prefix() {
        // `MemTotalFoo` must not answer for `MemTotal`, and `MemAvailable`
        // must not be found by a prefix scan for `Mem`.
        let meminfo = "MemTotalFoo:     9999999 kB\nMemTotal:        1048576 kB\n\
                       MemAvailableX:         1 kB\nMemAvailable:     524288 kB\n";
        assert_eq!(format_memory(meminfo).as_deref(), Some("0.5/1.0G"));
    }

    #[test]
    fn more_available_than_total_reads_as_nothing_used() {
        // Seen transiently on some kernels; must not wrap.
        let meminfo = "MemTotal:        1048576 kB\nMemAvailable:    1148576 kB\n";
        assert_eq!(format_memory(meminfo).as_deref(), Some("0.0/1.0G"));
    }

    #[test]
    fn a_discharging_battery_is_bare_percent() {
        assert_eq!(
            format_battery("87\n", "Discharging\n").as_deref(),
            Some("87%")
        );
    }

    #[test]
    fn a_charging_battery_is_marked_with_a_plus() {
        assert_eq!(
            format_battery("87\n", "Charging\n").as_deref(),
            Some("87%+")
        );
    }

    #[test]
    fn a_full_battery_is_marked_with_an_equals() {
        assert_eq!(format_battery("100\n", "Full\n").as_deref(), Some("100%="));
    }

    #[test]
    fn an_unknown_or_held_status_is_left_unmarked() {
        assert_eq!(format_battery("64", "Unknown").as_deref(), Some("64%"));
        assert_eq!(format_battery("64", "Not charging").as_deref(), Some("64%"));
        assert_eq!(format_battery("64", "").as_deref(), Some("64%"));
    }

    #[test]
    fn a_capacity_above_one_hundred_is_clamped() {
        // Firmware gauges do report 101; showing it would look like a bug
        // in the bar rather than in the battery.
        assert_eq!(format_battery("101", "Full").as_deref(), Some("100%="));
    }

    #[test]
    fn a_negative_capacity_is_clamped_to_zero() {
        assert_eq!(format_battery("-5", "Discharging").as_deref(), Some("0%"));
    }

    #[test]
    fn a_non_numeric_or_empty_capacity_yields_nothing() {
        assert_eq!(format_battery("", "Charging"), None);
        assert_eq!(format_battery("eighty seven", "Charging"), None);
        assert_eq!(format_battery("87.5", "Charging"), None);
    }

    #[test]
    fn a_gauge_pair_is_turned_into_a_rounded_percentage() {
        // A real pair of µWh values: 45820/52560 = 87.17%.
        assert_eq!(
            format_battery_from_gauge("45820000", "52560000", "Discharging").as_deref(),
            Some("87%")
        );
        // Rounds to nearest, not down: 2/3 is 67%.
        assert_eq!(
            format_battery_from_gauge("2", "3", "Charging").as_deref(),
            Some("67%+")
        );
    }

    #[test]
    fn a_gauge_over_its_full_charge_is_clamped() {
        assert_eq!(
            format_battery_from_gauge("60", "50", "Full").as_deref(),
            Some("100%=")
        );
    }

    #[test]
    fn a_zero_or_unparsable_gauge_yields_nothing() {
        assert_eq!(format_battery_from_gauge("100", "0", "Full"), None);
        assert_eq!(format_battery_from_gauge("", "52560000", "Full"), None);
        assert_eq!(format_battery_from_gauge("45820000", "", "Full"), None);
        assert_eq!(format_battery_from_gauge("-1", "50", "Full"), None);
    }

    #[test]
    fn the_readers_never_panic_on_this_machine() {
        // The contract the bar relies on: whatever `/proc` and `/sys` look
        // like here — battery or none, `/sys` mounted or not — the readers
        // return, and a `Some` is a plausibly shaped string.
        if let Some(out) = battery() {
            assert!(
                out.contains('%'),
                "battery readout {out:?} has no percent sign"
            );
        }
        if let Some(out) = load() {
            assert!(is_decimal(&out), "load readout {out:?} is not a decimal");
        }
        if let Some(out) = memory() {
            assert!(
                out.contains('/'),
                "memory readout {out:?} has no used/total split"
            );
        }
    }
}
