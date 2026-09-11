//! Helpers shared by the integration tests: a dependency-free PRNG and a
//! memfd factory.

#![allow(dead_code)]

use std::os::fd::OwnedFd;

/// xorshift64*, seeded deterministically. Not cryptographic; it only has
/// to produce a repeatable, well-mixed byte stream for the fuzz tests.
pub struct Xorshift(u64);

impl Xorshift {
    /// Seed the generator. A zero seed is replaced (xorshift's fixed point).
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    /// Next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Next 32-bit value (the high half, which mixes best).
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
}

/// A sealed-nothing memfd of `size` bytes, for fd-passing tests.
pub fn memfd(name: &str, size: u64) -> OwnedFd {
    let fd = rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC)
        .expect("memfd_create is available on Linux");
    rustix::fs::ftruncate(&fd, size).expect("ftruncate a fresh memfd");
    fd
}

/// The `(device, inode)` pair identifying the file behind `fd`.
pub fn identity(fd: impl std::os::fd::AsFd) -> (u64, u64) {
    let st = rustix::fs::fstat(fd).expect("fstat");
    (st.st_dev as u64, st.st_ino as u64)
}
