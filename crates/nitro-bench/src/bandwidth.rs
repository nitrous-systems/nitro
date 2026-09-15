//! The denominator: this machine's memory copy bandwidth.
//!
//! # Why a benchmark needs it
//!
//! "A fullscreen putimage at 60 Hz costs 8 ms" is not a result. It
//! becomes one next to "this box copies 2.1 GB/s, and the frame needs
//! 497 MB/s written plus the same read back, so the pixel path is using a
//! quarter of the machine's memory bandwidth before the compositor has
//! touched anything". The first number invites tuning; the second says
//! whether tuning is even possible, and on which side.
//!
//! The box is a Pentium G3240 — Haswell, two cores, SSE4.2 and **no
//! AVX2** (`docs/testbox.md`). Its memory bandwidth is the binding
//! constraint on every fullscreen effect in this crate, so it is measured
//! rather than looked up: the same silicon with one memory channel
//! populated instead of two halves this number, and nothing in a spec
//! sheet would tell you which one you have.
//!
//! # What is measured, honestly
//!
//! Three loops, because they are three different costs and conflating
//! them is how bandwidth claims go wrong:
//!
//! * **copy** — `dst[i] = src[i]` over a buffer: one read and one write
//!   per byte. This is what the server's `pread` of a client buffer and
//!   its copy into the scanout buffer both are.
//! * **write** — `dst[i] = value`: a write per byte and no read. This is
//!   what an effect filling its own surface does, and it is *faster* than
//!   a copy, so quoting a copy number for it would understate the
//!   headroom.
//! * **read** — sum every byte. Included because the difference between
//!   read and write bandwidth on a write-combined mapping is the entire
//!   reason `NITRO_SHADOW` exists (`docs/latency.md` §4.5).
//!
//! Buffers are sized well past this box's 3 MB last-level cache so the
//! number is memory and not L3; the default is 64 MB, and a run that
//! fitted in cache would report something like 20 GB/s, which is a true
//! statement about the cache and a false one about the frame path.
//!
//! The loops are written to be un-eliminable without `unsafe` or a
//! `black_box`: the copy and write loops feed a checksum that the caller
//! is handed back and the binary prints, so the optimiser cannot drop the
//! stores. That is the same trick #3711 used in this tree for the PNG
//! measurement, and for the same reason: an LTO'd-away loop measures
//! nothing and reports a spectacular number for it.

use std::time::Instant;

/// One bandwidth measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bandwidth {
    /// Bytes moved.
    pub bytes: u64,
    /// Microseconds it took.
    pub micros: u64,
    /// A value derived from the data, printed by the caller so the
    /// optimiser cannot delete the loop that produced it.
    pub checksum: u64,
}

impl Bandwidth {
    /// Bytes per second.
    ///
    /// Zero microseconds gives zero rather than an infinity: a
    /// measurement that took no time was not a measurement, and
    /// `inf GB/s` in a table is worse than a blank.
    #[must_use]
    pub fn bytes_per_second(&self) -> f64 {
        if self.micros == 0 {
            return 0.0;
        }
        self.bytes as f64 * 1e6 / self.micros as f64
    }

    /// Gigabytes (10⁹, not 2³⁰) per second, which is how memory
    /// bandwidth is conventionally quoted and how a DDR3-1600 channel's
    /// 12.8 GB/s is derived.
    #[must_use]
    pub fn gb_per_second(&self) -> f64 {
        self.bytes_per_second() / 1e9
    }

    /// How many 1080p BGRA frames per second this bandwidth is worth.
    ///
    /// The one conversion that turns a memory number into a frame budget:
    /// at 2 GB/s a copy of an 8.29 MB frame can happen about 240 times a
    /// second, which is four times the 60 Hz requirement and twice the
    /// 120 Hz one — before anything else in the system has run.
    #[must_use]
    pub fn frames_1080p(&self) -> f64 {
        self.bytes_per_second() / 8_294_400.0
    }
}

/// Default buffer size: 64 MB, comfortably past any desktop last-level
/// cache and past this box's 3 MB by a factor of twenty.
pub const DEFAULT_BYTES: usize = 64 << 20;

/// Copy `bytes` from one buffer to another, `rounds` times.
///
/// Uses `copy_from_slice`, which is `memcpy`: measuring a hand-written
/// byte loop would produce a smaller number that no real code path pays,
/// since every copy in the tree is a slice copy.
#[must_use]
pub fn copy(bytes: usize, rounds: u32) -> Bandwidth {
    let src = vec![0xa5u8; bytes];
    let mut dst = vec![0u8; bytes];
    let t = Instant::now();
    let mut checksum = 0u64;
    for r in 0..rounds {
        dst.copy_from_slice(&src);
        // One byte per round, read back from the destination: enough to
        // make the copy observable, cheap enough not to be measured.
        checksum = checksum
            .wrapping_mul(31)
            .wrapping_add(u64::from(dst[(r as usize * 4096) % bytes]));
    }
    Bandwidth {
        bytes: bytes as u64 * u64::from(rounds),
        micros: t.elapsed().as_micros() as u64,
        checksum,
    }
}

/// Fill a buffer with a value, `rounds` times: the write-only cost.
#[must_use]
pub fn write(bytes: usize, rounds: u32) -> Bandwidth {
    let mut dst = vec![0u8; bytes];
    let t = Instant::now();
    let mut checksum = 0u64;
    for r in 0..rounds {
        let v = (r & 0xff) as u8;
        dst.fill(v);
        checksum = checksum
            .wrapping_mul(31)
            .wrapping_add(u64::from(dst[(r as usize * 4096) % bytes]));
    }
    Bandwidth {
        bytes: bytes as u64 * u64::from(rounds),
        micros: t.elapsed().as_micros() as u64,
        checksum,
    }
}

/// Sum every byte, `rounds` times: the read-only cost.
///
/// Summed into a `u64` the caller prints, which is what stops the loop
/// being deleted — a read whose result is unused is not a read.
#[must_use]
pub fn read(bytes: usize, rounds: u32) -> Bandwidth {
    let src = vec![0x5au8; bytes];
    let t = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..rounds {
        let mut sum = 0u64;
        for chunk in src.chunks_exact(8) {
            // Eight bytes at a time so the loop is not dominated by the
            // bounds check; still no `unsafe` and still no wider type than
            // a `u64`, which is what an SSE2-only baseline would use.
            sum = sum.wrapping_add(u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]));
        }
        checksum = checksum.wrapping_add(sum);
    }
    Bandwidth {
        bytes: bytes as u64 * u64::from(rounds),
        micros: t.elapsed().as_micros() as u64,
        checksum,
    }
}

/// All three, at the default size, with the round count chosen so the
/// whole probe takes roughly a second on a slow machine.
///
/// Returned as a labelled triple rather than printed, so the binary's
/// `--json` mode and its human mode format the same measurement.
#[must_use]
pub fn probe() -> [(&'static str, Bandwidth); 3] {
    [
        ("copy", copy(DEFAULT_BYTES, 8)),
        ("write", write(DEFAULT_BYTES, 8)),
        ("read", read(DEFAULT_BYTES, 8)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small and cheap: the test is that the arithmetic and the
    /// anti-elimination work, not that this machine is fast.
    const SMALL: usize = 1 << 16;

    #[test]
    fn a_copy_moves_the_bytes_it_says_it_did() {
        let b = copy(SMALL, 4);
        assert_eq!(b.bytes, SMALL as u64 * 4);
        assert!(b.bytes_per_second() >= 0.0);
    }

    #[test]
    fn all_three_loops_run_and_produce_a_checksum() {
        for b in [copy(SMALL, 2), write(SMALL, 2), read(SMALL, 2)] {
            assert_eq!(b.bytes, SMALL as u64 * 2);
            // A checksum of zero from the read loop is legitimate only if
            // every byte were zero, which it is not: the buffers are
            // filled with 0xa5/0x5a precisely so a deleted loop shows up.
            assert_ne!(b.checksum, 0, "{b:?}");
        }
    }

    /// The conversion the whole pixel-path verdict uses.
    #[test]
    fn a_bandwidth_converts_to_1080p_frames_per_second() {
        let b = Bandwidth {
            bytes: 8_294_400 * 240,
            micros: 1_000_000,
            checksum: 1,
        };
        assert!((b.frames_1080p() - 240.0).abs() < 0.001, "{b:?}");
        assert!((b.gb_per_second() - 1.990_656).abs() < 1e-6);
    }

    /// A zero-microsecond measurement is not a measurement, and an
    /// infinity in a table is worse than a blank.
    #[test]
    fn a_zero_duration_is_zero_bandwidth_not_infinity() {
        let b = Bandwidth {
            bytes: 1_000_000,
            micros: 0,
            checksum: 0,
        };
        assert!(b.bytes_per_second() < f64::EPSILON);
        assert!(b.frames_1080p() < f64::EPSILON);
    }

    /// The default must be far past any last-level cache, or the probe
    /// reports the cache and calls it memory.
    #[test]
    fn the_default_buffer_is_far_larger_than_any_last_level_cache() {
        const { assert!(DEFAULT_BYTES >= 32 << 20) };
    }
}
