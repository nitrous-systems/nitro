//! Helper counters, and the driver-memory figures from DRM fdinfo.

use std::collections::HashSet;
use std::time::Duration;

use crate::proto::{ShadowPath, Stats};

/// Running counters kept by the event loop.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    /// Frames submitted.
    pub frames: u64,
    /// Textures imported.
    pub imports: u64,
    /// Error replies.
    pub errors: u64,
    submit_total_us: u64,
    submit_max_us: u32,
}

impl Counters {
    /// Record one `composite` call's CPU time.
    pub fn submit(&mut self, t: Duration) {
        let us = u32::try_from(t.as_micros()).unwrap_or(u32::MAX);
        self.frames += 1;
        self.submit_total_us += u64::from(us);
        self.submit_max_us = self.submit_max_us.max(us);
    }

    /// The protocol's [`Stats`], with the process figures read now.
    #[must_use]
    pub fn snapshot(
        &self,
        textures_live: usize,
        in_flight: usize,
        shadow_path: ShadowPath,
    ) -> Stats {
        let drm = drm_memory();
        Stats {
            frames: self.frames,
            imports: self.imports,
            errors: self.errors,
            textures_live: textures_live as u32,
            in_flight: in_flight as u32,
            submit_us_avg: self
                .submit_total_us
                .checked_div(self.frames)
                .map_or(0, |v| v as u32),
            submit_us_max: self.submit_max_us,
            shadow_path,
            drm_total: drm.total,
            drm_resident: drm.resident,
            rss: proc_kib("/proc/self/status", "VmRSS:"),
            pss: proc_kib("/proc/self/smaps_rollup", "Pss:"),
        }
    }
}

/// Driver memory of one DRM client (or a sum), bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrmMemory {
    /// Sum of `drm-total-<region>`.
    pub total: u64,
    /// Sum of `drm-resident-<region>`.
    pub resident: u64,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>` file. Returns the DRM client id
/// (`drm-client-id`) and its memory, or `None` if it is not a DRM fd.
///
/// Values are `<n> [KiB|MiB|GiB]` (DRM usage stats, kernel
/// `Documentation/gpu/drm-usage-stats.rst`).
#[must_use]
pub fn parse_fdinfo(text: &str) -> Option<(u64, DrmMemory)> {
    let mut id = None;
    let mut mem = DrmMemory::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k == "drm-client-id" {
            id = v.parse().ok();
        } else if let Some(rest) = k.strip_prefix("drm-total-") {
            let _ = rest;
            mem.total += parse_size(v);
        } else if k.starts_with("drm-resident-") {
            mem.resident += parse_size(v);
        }
    }
    id.map(|id| (id, mem))
}

fn parse_size(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: u64 = it.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let mul = match it.next() {
        Some("KiB") => 1 << 10,
        Some("MiB") => 1 << 20,
        Some("GiB") => 1 << 30,
        _ => 1,
    };
    n.saturating_mul(mul)
}

/// Driver memory of this process: the fdinfo of every DRM fd, each DRM
/// client counted once (dup'd fds share a client id).
#[must_use]
pub fn drm_memory() -> DrmMemory {
    drm_memory_of("/proc/self")
}

/// Driver memory of the process whose `/proc` directory is `proc_dir`.
#[must_use]
pub fn drm_memory_of(proc_dir: &str) -> DrmMemory {
    let mut sum = DrmMemory::default();
    let mut seen = HashSet::new();
    let Ok(dir) = std::fs::read_dir(format!("{proc_dir}/fdinfo")) else {
        return sum;
    };
    for e in dir.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        if let Some((id, m)) = parse_fdinfo(&text)
            && seen.insert(id)
        {
            sum.total += m.total;
            sum.resident += m.resident;
        }
    }
    sum
}

/// A `<key> <n> kB` line of a `/proc` file, in bytes (0 if unreadable).
#[must_use]
pub fn proc_kib(path: &str, key: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|n| n.parse::<u64>().ok())
            })
        })
        .map_or(0, |kib| kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdinfo_sums_regions() {
        let t = "pos:\t0\nflags:\t02100002\ndrm-driver:\ti915\ndrm-client-id:\t42\n\
                 drm-total-system0:\t12 MiB\ndrm-resident-system0:\t8 MiB\n\
                 drm-total-stolen-system0:\t0\ndrm-resident-stolen-system0:\t4 KiB\n";
        let (id, m) = parse_fdinfo(t).unwrap();
        assert_eq!(id, 42);
        assert_eq!(m.total, 12 << 20);
        assert_eq!(m.resident, (8 << 20) + 4096);
        assert!(parse_fdinfo("pos:\t0\nflags:\t0\n").is_none());
    }

    #[test]
    fn submit_times() {
        let mut c = Counters::default();
        c.submit(Duration::from_micros(10));
        c.submit(Duration::from_micros(30));
        let s = c.snapshot(1, 0, ShadowPath::Staging);
        assert_eq!((s.frames, s.submit_us_avg, s.submit_us_max), (2, 20, 30));
        assert!(s.rss > 0);
        assert!(s.pss > 0);
    }
}
