//! Hotplug detection: a raw `NETLINK_KOBJECT_UEVENT` socket and the
//! parser that recognises DRM hotplug messages. No libudev.
//!
//! Kernel uevents (multicast group 1) are `action@devpath\0KEY=VALUE\0...`.
//! We only care about `SUBSYSTEM=drm` together with `HOTPLUG=1`, which is
//! what the DRM core emits when connector state changes.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{
    AddressFamily, RecvFlags, SocketFlags, SocketType, bind, netlink, recv, socket_with,
};

/// Multicast group carrying raw kernel uevents (group 2 is udev's).
const KERNEL_GROUP: u32 = 1;

/// Receive buffer; a uevent is well under 4 KiB, 8 KiB is what udev uses.
const RECV_BUF: usize = 8192;

/// True when `msg` is a kernel uevent for a DRM hotplug.
///
/// Tolerant of the udev-style binary header (starts with `libudev`): such
/// messages only arrive on group 2, which we don't join, but rejecting them
/// costs nothing.
#[must_use]
pub fn is_drm_hotplug(msg: &[u8]) -> bool {
    if msg.starts_with(b"libudev") {
        return false;
    }
    let mut subsystem_drm = false;
    let mut hotplug = false;
    for field in msg.split(|&b| b == 0) {
        match field {
            b"SUBSYSTEM=drm" => subsystem_drm = true,
            b"HOTPLUG=1" => hotplug = true,
            _ => {}
        }
    }
    subsystem_drm && hotplug
}

/// True when `msg` is a kernel uevent adding or removing a device on
/// `subsystem` (`b"input"`, `b"drm"`, …).
///
/// Unlike [`is_drm_hotplug`] this does not look for `HOTPLUG=1`: that flag
/// is the DRM core's way of saying "a connector changed state on a device
/// that is already here", and what an input device does instead is
/// *appear* and *disappear*, which is `ACTION=add` / `ACTION=remove`.
#[must_use]
pub fn is_device_change(msg: &[u8], subsystem: &[u8]) -> bool {
    if msg.starts_with(b"libudev") {
        return false;
    }
    let mut want_subsystem = Vec::with_capacity(10 + subsystem.len());
    want_subsystem.extend_from_slice(b"SUBSYSTEM=");
    want_subsystem.extend_from_slice(subsystem);
    let mut matched = false;
    let mut action = false;
    for field in msg.split(|&b| b == 0) {
        if field == want_subsystem.as_slice() {
            matched = true;
        } else if field == b"ACTION=add" || field == b"ACTION=remove" {
            action = true;
        }
    }
    matched && action
}

/// A non-blocking socket subscribed to kernel uevents.
pub struct UeventSocket {
    fd: OwnedFd,
    buf: Vec<u8>,
}

impl UeventSocket {
    /// Open and bind. Unprivileged receive is allowed for this family
    /// (`NL_CFG_F_NONROOT_RECV`).
    ///
    /// # Errors
    /// Socket creation or bind failure (e.g. inside a network namespace
    /// without netlink).
    pub fn open() -> io::Result<Self> {
        let fd = socket_with(
            AddressFamily::NETLINK,
            SocketType::RAW,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            Some(netlink::KOBJECT_UEVENT),
        )?;
        bind(&fd, &netlink::SocketAddrNetlink::new(0, KERNEL_GROUP))?;
        Ok(Self {
            fd,
            buf: vec![0; RECV_BUF],
        })
    }

    /// Drain every queued message; returns whether any was a DRM hotplug.
    /// Never blocks.
    ///
    /// # Errors
    /// A receive failure other than "would block".
    pub fn drain(&mut self) -> io::Result<bool> {
        self.drain_with(is_drm_hotplug)
    }

    /// Drain every queued message; returns whether any was an add or a
    /// remove on `subsystem`. Never blocks.
    ///
    /// # Errors
    /// A receive failure other than "would block".
    pub fn drain_subsystem(&mut self, subsystem: &[u8]) -> io::Result<bool> {
        self.drain_with(|msg| is_device_change(msg, subsystem))
    }

    fn drain_with(&mut self, mut interesting: impl FnMut(&[u8]) -> bool) -> io::Result<bool> {
        let mut hotplug = false;
        loop {
            match recv(&self.fd, &mut self.buf[..], RecvFlags::empty()) {
                Ok((n, _)) => {
                    if interesting(&self.buf[..n]) {
                        hotplug = true;
                    }
                }
                Err(rustix::io::Errno::AGAIN) => return Ok(hotplug),
                // Receive queue overflowed: treat as "something changed".
                Err(rustix::io::Errno::NOBUFS) => hotplug = true,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl AsFd for UeventSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_drm_hotplug() {
        let msg = b"change@/devices/pci0000:00/0000:00:02.0/drm/card1\0ACTION=change\0DEVPATH=/devices/pci0000:00/0000:00:02.0/drm/card1\0SUBSYSTEM=drm\0HOTPLUG=1\0DEVNAME=dri/card1\0DEVTYPE=drm_minor\0SEQNUM=4711\0";
        assert!(is_drm_hotplug(msg));
    }

    #[test]
    fn rejects_other_subsystems_and_non_hotplug() {
        assert!(!is_drm_hotplug(
            b"add@/devices/virtual/input/input7\0ACTION=add\0SUBSYSTEM=input\0HOTPLUG=1\0"
        ));
        assert!(!is_drm_hotplug(
            b"change@/devices/.../drm/card1\0ACTION=change\0SUBSYSTEM=drm\0LEASE=1\0"
        ));
        assert!(!is_drm_hotplug(b""));
        // A `SUBSYSTEM=drm_foo` prefix must not match.
        assert!(!is_drm_hotplug(b"x\0SUBSYSTEM=drm_dp_aux_dev\0HOTPLUG=1\0"));
    }

    #[test]
    fn rejects_udev_binary_header() {
        let mut msg = b"libudev\0".to_vec();
        msg.extend_from_slice(&[0xfe, 0xed, 0xca, 0xfe, 0, 0, 0, 0]);
        msg.extend_from_slice(b"SUBSYSTEM=drm\0HOTPLUG=1\0");
        assert!(!is_drm_hotplug(&msg));
        assert!(!is_device_change(&msg, b"drm"));
    }

    #[test]
    fn recognises_an_input_device_appearing_and_leaving() {
        let add = b"add@/devices/virtual/input/input7/event9\0ACTION=add\0SUBSYSTEM=input\0DEVNAME=input/event9\0";
        let remove = b"remove@/devices/virtual/input/input7/event9\0ACTION=remove\0SUBSYSTEM=input\0";
        assert!(is_device_change(add, b"input"));
        assert!(is_device_change(remove, b"input"));
        // An input device appearing is not a DRM hotplug.
        assert!(!is_drm_hotplug(add));
        // Another subsystem's add is not ours.
        assert!(!is_device_change(add, b"drm"));
        // A `change` on a device already present is neither.
        let change = b"change@/devices/x\0ACTION=change\0SUBSYSTEM=input\0";
        assert!(!is_device_change(change, b"input"));
        // A prefix must not match.
        assert!(!is_device_change(
            b"add@/x\0ACTION=add\0SUBSYSTEM=input_foo\0",
            b"input"
        ));
        assert!(!is_device_change(b"", b"input"));
    }

    #[test]
    fn socket_opens_or_is_denied_cleanly() {
        // Sandboxes may forbid netlink; either outcome must be an io::Error
        // or a usable socket, never a panic.
        if let Ok(mut s) = UeventSocket::open() {
            assert!(!s.drain().unwrap());
        }
    }
}
