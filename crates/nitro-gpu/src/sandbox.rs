//! Least privilege for the helper process, all in safe Rust.
//!
//! The helper holds a **render node** and nothing else: no DRM master
//! (`/dev/dri/card*`), no input devices. [`apply`] drops what can be
//! dropped without `unsafe`; [`check_fds`] refuses to run with a
//! forbidden descriptor inherited from a sloppy parent, and runs again
//! after device creation to prove the driver opened only a render node.
//!
//! **seccomp is deferred.** rustix has no seccomp, and loading a BPF
//! filter means `prctl(PR_SET_SECCOMP)` with a pointer — `unsafe` or
//! libc. It is the next hardening step (see the crate README); the syscall
//! set a Vulkan driver needs (ioctl on the render node, mmap, futex,
//! memfd, poll, sendmsg/recvmsg) is small and stable enough to allowlist.

use std::os::fd::OwnedFd;

use rustix::process::{Resource, Rlimit};

/// Most descriptors the helper may hold (textures hold none after import;
/// the in-flight fences and ring are bounded by the protocol).
pub const NOFILE: u64 = 4096;

/// What an inherited descriptor points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdClass {
    /// `/dev/dri/card*`: a primary node, which can hold DRM master.
    DrmPrimary,
    /// `/dev/dri/renderD*`: what the helper is meant to have.
    DrmRender,
    /// `/dev/input/*`: keyboards, mice.
    Input,
    /// Anything else (sockets, pipes, memfds, dma-bufs, files).
    Other,
}

impl FdClass {
    /// Whether the helper must refuse to run holding this.
    #[must_use]
    pub fn forbidden(self) -> bool {
        matches!(self, Self::DrmPrimary | Self::Input)
    }
}

/// Classify the target of a `/proc/self/fd/N` link. Pure, for tests.
#[must_use]
pub fn classify(target: &str) -> FdClass {
    if let Some(node) = target.strip_prefix("/dev/dri/") {
        if node.starts_with("renderD") {
            FdClass::DrmRender
        } else if node.starts_with("card") {
            FdClass::DrmPrimary
        } else {
            FdClass::Other
        }
    } else if target.starts_with("/dev/input/") {
        FdClass::Input
    } else {
        FdClass::Other
    }
}

/// Every open descriptor with its link target and class.
#[must_use]
pub fn list_fds() -> Vec<(i32, String, FdClass)> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
        return out;
    };
    for e in dir.flatten() {
        let Some(n) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(target) = std::fs::read_link(e.path()) else {
            continue;
        };
        let t = target.to_string_lossy().into_owned();
        let c = classify(&t);
        out.push((n, t, c));
    }
    out
}

/// The forbidden descriptors this process holds (empty = fine).
#[must_use]
pub fn check_fds() -> Vec<(i32, String)> {
    list_fds()
        .into_iter()
        .filter(|(_, _, c)| c.forbidden())
        .map(|(n, t, _)| (n, t))
        .collect()
}

/// Drop privileges: `no_new_privs`, not dumpable (no ptrace from other
/// same-uid processes, no core files), `RLIMIT_CORE = 0`, `RLIMIT_NOFILE`
/// capped at [`NOFILE`], and `chdir("/")`.
///
/// # Errors
/// The first failing syscall.
pub fn apply() -> Result<(), rustix::io::Errno> {
    rustix::thread::set_no_new_privs(true)?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    rustix::process::setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    let cur = rustix::process::getrlimit(Resource::Nofile);
    let cap = |v: Option<u64>| Some(v.map_or(NOFILE, |v| v.min(NOFILE)));
    rustix::process::setrlimit(
        Resource::Nofile,
        Rlimit {
            current: cap(cur.current),
            maximum: cap(cur.maximum),
        },
    )?;
    rustix::process::chdir("/")?;
    Ok(())
}

/// Take the server socket from fd 0 (how the server hands it over) and put
/// `/dev/null` in its place, so nothing later reads the socket by accident.
///
/// # Errors
/// `dup`, `open` or `dup2` failure.
pub fn take_stdin() -> Result<OwnedFd, rustix::io::Errno> {
    let fd = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdin(), 3)?;
    let null = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    rustix::stdio::dup2_stdin(&null)?;
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify("/dev/dri/renderD128"), FdClass::DrmRender);
        assert_eq!(classify("/dev/dri/card1"), FdClass::DrmPrimary);
        assert_eq!(classify("/dev/input/event3"), FdClass::Input);
        assert_eq!(classify("/dev/null"), FdClass::Other);
        assert_eq!(classify("socket:[123]"), FdClass::Other);
        assert_eq!(classify("/memfd:shadow (deleted)"), FdClass::Other);
        assert!(FdClass::DrmPrimary.forbidden());
        assert!(FdClass::Input.forbidden());
        assert!(!FdClass::DrmRender.forbidden());
    }

    #[test]
    fn test_process_holds_nothing_forbidden() {
        let fds = list_fds();
        assert!(fds.iter().any(|(n, _, _)| *n == 0 || *n == 1 || *n == 2));
        assert!(check_fds().is_empty(), "{:?}", check_fds());
    }
}
