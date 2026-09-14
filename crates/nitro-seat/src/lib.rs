//! Safe wrapper over [`libseat`]: the server's ownership root for every
//! privileged file descriptor.
//!
//! A [`Seat`] opens the libseat session (logind, seatd or a raw VT — libseat
//! picks at runtime, `LIBSEAT_BACKEND` overrides) and hands out [`Device`]
//! handles for `/dev/dri/cardN` and `/dev/input/event*`. The seat is opened
//! first and dropped last; everything that holds a device fd is its child.
//!
//! # Event loop contract
//!
//! * Register [`Seat::as_fd`] with epoll, level-triggered, readable.
//! * When it is readable call [`Seat::dispatch`] and act on the returned
//!   [`SeatEvent`]s in order.
//! * On [`SeatEvent::Disable`] stop touching every device (no DRM commits,
//!   no evdev reads), then call [`Seat::ack_disable`]. libseat will not
//!   complete the VT switch until the disable is acknowledged; the crate
//!   deliberately does **not** acknowledge on the caller's behalf.
//! * On [`SeatEvent::Enable`] the devices are usable again (re-modeset).
//!
//! # Drop order
//!
//! A [`Device`] closes itself through its seat when dropped, so the normal
//! path is `drop(device)` or the explicit [`Seat::close_device`] (which
//! also reports the libseat error). A `Seat` must outlive its devices: in
//! debug builds dropping a `Seat` with live devices panics; in release
//! builds such a `Device` still closes its own fd, but libseat is never
//! told, so keep the seat at the root of the ownership tree and this never
//! comes up.
//!
//! The fd itself is closed by this crate, not by libseat — see
//! [`Seat::close_device`].
//!
//! This crate never logs; it returns data and the server decides what to
//! say.

use std::cell::RefCell;
use std::fmt;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// What failed, for [`Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// `libseat_open_seat` failed, or the seat has no pollable fd.
    Open,
    /// `libseat_dispatch` failed.
    Dispatch,
    /// Acknowledging a disable (`libseat_disable_seat`) failed.
    AckDisable,
    /// Opening a device failed.
    OpenDevice {
        /// The device path that was requested.
        path: PathBuf,
    },
    /// Closing a device failed.
    CloseDevice,
    /// `libseat_switch_session` failed.
    Switch,
}

/// A libseat failure: which operation, plus the `errno` libseat set.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: std::io::Error,
}

impl Error {
    /// libseat's binding reports `errno::Errno`, which converts into
    /// `io::Error`; taking `Into` keeps this crate free of an `errno` dep.
    fn new(kind: ErrorKind, source: impl Into<std::io::Error>) -> Self {
        Self {
            kind,
            source: source.into(),
        }
    }

    /// Which operation failed.
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// The raw OS error number libseat reported, if it was one.
    pub fn raw_os_error(&self) -> Option<i32> {
        self.source.raw_os_error()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::Open => write!(f, "opening seat")?,
            ErrorKind::Dispatch => write!(f, "dispatching seat events")?,
            ErrorKind::AckDisable => write!(f, "acknowledging seat disable")?,
            ErrorKind::OpenDevice { path } => write!(f, "opening device {}", path.display())?,
            ErrorKind::CloseDevice => write!(f, "closing device")?,
            ErrorKind::Switch => write!(f, "switching session")?,
        }
        write!(f, ": {}", self.source)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// A seat state change delivered by [`Seat::dispatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatEvent {
    /// The seat became active: devices may be used (again).
    Enable,
    /// The seat is being deactivated (VT switch away, session ended).
    /// Stop using devices, then call [`Seat::ack_disable`].
    Disable,
}

impl From<libseat::SeatEvent> for SeatEvent {
    fn from(ev: libseat::SeatEvent) -> Self {
        match ev {
            libseat::SeatEvent::Enable => Self::Enable,
            libseat::SeatEvent::Disable => Self::Disable,
        }
    }
}

/// Identifies a [`Device`] for the lifetime of its [`Seat`].
///
/// Assigned by this crate (monotonic per seat); libseat's own device id is
/// private in the binding and is never needed by callers because
/// [`Seat::close_device`] takes the whole [`Device`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(u64);

impl DeviceId {
    /// The numeric id.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// State shared between the `Seat` and libseat's event callback.
#[derive(Debug, Default)]
struct Shared {
    /// Events received but not yet returned by [`Seat::dispatch`].
    events: Vec<SeatEvent>,
    /// The last state libseat told us about.
    active: bool,
}

impl Shared {
    fn push(&mut self, ev: SeatEvent) {
        self.active = matches!(ev, SeatEvent::Enable);
        self.events.push(ev);
    }
}

/// The libseat session. Owns the connection and every open [`Device`].
///
/// See the [crate docs](crate) for the event-loop and drop-order contracts.
pub struct Seat {
    /// `Rc` so a [`Device`] can hold a [`Weak`] back-reference and close
    /// itself on drop; `Weak::upgrade` failing means "seat is gone, ignore".
    inner: Rc<RefCell<libseat::Seat>>,
    shared: Rc<RefCell<Shared>>,
    /// A dup of libseat's pollable fd. libseat only lends its fd through a
    /// `&mut` accessor; duplicating it once at open lets `as_fd` be a plain
    /// `&self` borrow. Both fds share one open file description, so epoll
    /// readiness is identical.
    poll_fd: OwnedFd,
    name: String,
    next_id: u64,
}

impl fmt::Debug for Seat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Seat")
            .field("name", &self.name)
            .field("active", &self.is_active())
            .field("open_devices", &self.open_devices())
            .finish_non_exhaustive()
    }
}

impl Seat {
    /// Opens the seat. libseat picks the backend (logind, seatd, builtin)
    /// unless `LIBSEAT_BACKEND` is set.
    ///
    /// The initial `Enable` normally arrives on the first
    /// [`dispatch`](Self::dispatch) (observed with logind and `noop`); any
    /// event a backend delivers synchronously during open is queued for it
    /// too, with [`is_active`](Self::is_active) already reflecting it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Open`] if libseat cannot open a seat, or if the backend
    /// exposes no pollable fd.
    pub fn open() -> Result<Self, Error> {
        let shared = Rc::new(RefCell::new(Shared::default()));
        let sink = Rc::clone(&shared);
        let mut seat = libseat::Seat::open(move |_seat, ev| sink.borrow_mut().push(ev.into()))
            .map_err(|e| Error::new(ErrorKind::Open, e))?;
        let poll_fd = seat
            .get_fd()
            .map_err(|e| Error::new(ErrorKind::Open, e))?
            .try_clone_to_owned()
            .map_err(|e| Error::new(ErrorKind::Open, e))?;
        let name = seat.name().to_owned();
        Ok(Self {
            inner: Rc::new(RefCell::new(seat)),
            shared,
            poll_fd,
            name,
            next_id: 0,
        })
    }

    /// The seat name libseat reports (e.g. `seat0`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads whatever libseat has pending, without blocking, and returns the
    /// resulting events in delivery order. Call when [`as_fd`](Self::as_fd)
    /// is readable. A `Disable` is **not** acknowledged here; see
    /// [`ack_disable`](Self::ack_disable).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Dispatch`] when libseat reports an error (typically the
    /// connection to logind/seatd is gone).
    pub fn dispatch(&mut self) -> Result<Vec<SeatEvent>, Error> {
        self.inner
            .borrow_mut()
            .dispatch(0)
            .map_err(|e| Error::new(ErrorKind::Dispatch, e))?;
        Ok(std::mem::take(&mut self.shared.borrow_mut().events))
    }

    /// Tells libseat the caller has stopped using its devices after a
    /// [`SeatEvent::Disable`]. Until this is called the VT switch (or session
    /// hand-over) does not complete. Devices must not be used again until the
    /// next [`SeatEvent::Enable`].
    ///
    /// # Errors
    ///
    /// [`ErrorKind::AckDisable`] when libseat rejects the call.
    pub fn ack_disable(&mut self) -> Result<(), Error> {
        self.inner
            .borrow_mut()
            .disable()
            .map_err(|e| Error::new(ErrorKind::AckDisable, e))
    }

    /// Whether the seat is currently active as far as libseat has told us:
    /// `true` after the last `Enable`, `false` after the last `Disable` or
    /// before any event.
    pub fn is_active(&self) -> bool {
        self.shared.borrow().active
    }

    /// Number of [`Device`]s currently open on this seat.
    pub fn open_devices(&self) -> usize {
        Rc::weak_count(&self.inner)
    }

    /// Opens a device (`/dev/dri/cardN`, `/dev/input/eventN`) through the
    /// seat. Only succeeds while the seat is active and the path is of a
    /// kind the backend permits.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::OpenDevice`] carrying `path`.
    pub fn open_device(&mut self, path: &Path) -> Result<Device, Error> {
        let inner = self.inner.borrow_mut().open_device(&path).map_err(|e| {
            Error::new(
                ErrorKind::OpenDevice {
                    path: path.to_owned(),
                },
                e,
            )
        })?;
        let id = DeviceId(self.next_id);
        self.next_id += 1;
        Ok(Device {
            inner: Some(inner),
            seat: Rc::downgrade(&self.inner),
            id,
            path: path.to_owned(),
        })
    }

    /// Closes a device through the seat. Prefer this over `drop(dev)` when
    /// the error matters; the fd is invalid afterwards either way.
    ///
    /// The descriptor is closed by *this crate*: `libseat_close_device`
    /// closes the device on the seat but leaves the fd it handed out open
    /// (see [`close_device_fd`]).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::CloseDevice`] when libseat rejects the close. The device
    /// is consumed, and its fd closed, regardless.
    pub fn close_device(&mut self, mut dev: Device) -> Result<(), Error> {
        // `dev` came from this seat, or from one that is already gone (in
        // which case `take()` below leaves nothing for `Drop` to do either).
        let Some(inner) = dev.inner.take() else {
            return Ok(());
        };
        if !Weak::ptr_eq(&dev.seat, &Rc::downgrade(&self.inner)) {
            // Foreign device: hand it back to its own seat via Drop.
            dev.inner = Some(inner);
            drop(dev);
            return Ok(());
        }
        // Release the Weak before the device is dropped so `open_devices`
        // is accurate immediately after this call.
        dev.seat = Weak::new();
        // `close_device` consumes `inner`, so read the fd out first.
        let raw = inner.as_fd().as_raw_fd();
        let res = self
            .inner
            .borrow_mut()
            .close_device(inner)
            .map_err(|e| Error::new(ErrorKind::CloseDevice, e));
        // After libseat is done with the device, never before: a backend
        // that revokes rather than closes may still touch the fd inside
        // the call. An error is not a reason to keep the fd: libseat has
        // handed ownership over either way.
        close_device_fd(raw);
        res
    }

    /// Asks libseat to switch to session `vt`. For VT-bound seats this is a
    /// VT switch. Success only means the request was accepted; the switch,
    /// if it happens, arrives as a [`SeatEvent::Disable`].
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Switch`] when libseat rejects the request.
    pub fn switch_session(&mut self, vt: i32) -> Result<(), Error> {
        self.inner
            .borrow_mut()
            .switch_session(vt)
            .map_err(|e| Error::new(ErrorKind::Switch, e))
    }
}

impl AsFd for Seat {
    /// The pollable fd: register it level-triggered readable and call
    /// [`Seat::dispatch`] when it fires.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.poll_fd.as_fd()
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        debug_assert_eq!(
            self.open_devices(),
            0,
            "nitro_seat::Seat dropped while {} Device(s) are alive; the seat must be dropped last",
            self.open_devices()
        );
    }
}

/// An open device fd owned by a [`Seat`].
///
/// Use [`AsFd`] to hand the fd to DRM/libinput. Closing goes through the
/// seat: either [`Seat::close_device`] or simply dropping the `Device`.
/// The fd is closed by this crate either way, even if the seat is already
/// gone (see the drop-order rule in the [crate docs](crate)); libseat only
/// learns about the close while the seat is alive.
#[derive(Debug)]
#[must_use = "dropping a Device closes it"]
pub struct Device {
    inner: Option<libseat::Device>,
    seat: Weak<RefCell<libseat::Seat>>,
    id: DeviceId,
    path: PathBuf,
}

impl Device {
    /// This device's id within its seat.
    pub fn id(&self) -> DeviceId {
        self.id
    }

    /// The path the device was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsFd for Device {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner
            .as_ref()
            .expect("Device fd taken; only possible mid-close")
            .as_fd()
    }
}

/// Closes a descriptor libseat handed out for a device.
///
/// libseat's contract for the fd is undocumented: `libseat_close_device`
/// is specified only as "closes a device that has been opened on the seat
/// using the `device_id`", and on the builds we test against (libseat 0.9,
/// logind and `noop` backends) it leaves the descriptor open. Nor does the
/// `libseat` crate close it: its `Device` is `{ id, fd }` with no `Drop`,
/// and `close_device` consumes it, dropping the raw fd on the floor. So
/// the caller owns it, and the server leaked one fd per device per VT
/// round trip until this closed it.
///
/// Taking the fd back into an [`OwnedFd`] is how the close happens without
/// `unsafe` in this crate: `OwnedFd`'s own `Drop` issues the `close(2)`.
/// The safety condition is the one the callers uphold — the fd came from
/// `libseat_open_device`, has just been released by `libseat_close_device`
/// (or its seat is gone), and nothing else holds it.
fn close_device_fd(raw: RawFd) {
    // SAFETY: `raw` is a live descriptor that libseat opened for this
    // device and has finished with; the `libseat::Device` that named it
    // has been consumed, so this is the only owner.
    #[allow(unsafe_code)]
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    drop(owned);
}

impl Drop for Device {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let raw = inner.as_fd().as_raw_fd();
        // Seat gone (or currently dispatching — impossible for a caller to
        // arrange, but harmless): libseat cannot be told, but the fd is
        // still ours to close. Errors from libseat are ignored: use
        // `Seat::close_device` to observe them.
        //
        // On the seat-gone branch specifically, this closes an fd that
        // libseat was never told about. That is the documented-illegal drop
        // order (`Seat::drop` `debug_assert`s against it, so a debug build
        // panics first) and it is better than leaking, but it is worth
        // recording what would make it *worse* than leaking: if a future
        // libseat closed device fds inside `libseat_close_seat`, this would
        // close a descriptor *number* that may already have been handed out
        // again — a cross-close, which is far nastier than a leak.
        //
        // Measured behaviour today (libseat 0.9, logind and noop backends)
        // is that libseat closes the fd neither on `close_device` nor,
        // apparently, on `close_seat`, so there is nothing to fix.
        //
        // An `fcntl(F_GETFD)` probe before the close was considered for this
        // path only (issue #551.2) and **not taken**, for two reasons:
        //
        //   1. It is not safe Rust here. `rustix::io::fcntl_getfd` is safe,
        //      but it takes an `AsFd`, and the only way to get one from a
        //      bare `RawFd` is `BorrowedFd::borrow_raw`, which is an `unsafe
        //      fn`. It would need the same `#[allow(unsafe_code)]` the close
        //      below carries, and the tree's rule is that a new `unsafe` site
        //      has to earn its place.
        //   2. More importantly it would not answer the question. `F_GETFD`
        //      succeeds for *any* open descriptor at that number, including
        //      one some other part of the process opened after libseat closed
        //      ours — which is exactly the case the probe would exist to
        //      catch. Distinguishing "still ours" from "reused" needs an
        //      identity the fd number does not carry.
        //
        // The real fix, if libseat's behaviour ever changes, is to stop
        // owning the number: keep the fd as an `OwnedFd` from the moment
        // libseat hands it over.
        if let Some(seat) = self.seat.upgrade()
            && let Ok(mut seat) = seat.try_borrow_mut()
        {
            let _ = seat.close_device(inner);
        }
        close_device_fd(raw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn event_mapping() {
        assert_eq!(
            SeatEvent::from(libseat::SeatEvent::Enable),
            SeatEvent::Enable
        );
        assert_eq!(
            SeatEvent::from(libseat::SeatEvent::Disable),
            SeatEvent::Disable
        );
    }

    #[test]
    fn shared_tracks_active_and_queues_in_order() {
        let mut s = Shared::default();
        assert!(!s.active);
        s.push(SeatEvent::Enable);
        assert!(s.active);
        s.push(SeatEvent::Disable);
        assert!(!s.active);
        s.push(SeatEvent::Enable);
        assert_eq!(
            s.events,
            [SeatEvent::Enable, SeatEvent::Disable, SeatEvent::Enable]
        );
    }

    fn err(kind: ErrorKind) -> Error {
        Error {
            kind,
            source: std::io::Error::from_raw_os_error(13), // EACCES
        }
    }

    #[test]
    fn error_display_names_operation_and_errno() {
        let e = err(ErrorKind::Open);
        let s = e.to_string();
        assert!(s.starts_with("opening seat: "), "{s}");
        assert!(s.contains("denied"), "{s}");
        assert_eq!(e.raw_os_error(), Some(13));
        assert!(e.source().is_some());

        let e = err(ErrorKind::OpenDevice {
            path: PathBuf::from("/dev/dri/card1"),
        });
        assert!(
            e.to_string().starts_with("opening device /dev/dri/card1: "),
            "{e}"
        );
        assert_eq!(
            e.kind(),
            &ErrorKind::OpenDevice {
                path: PathBuf::from("/dev/dri/card1")
            }
        );

        for (kind, prefix) in [
            (ErrorKind::Dispatch, "dispatching seat events: "),
            (ErrorKind::AckDisable, "acknowledging seat disable: "),
            (ErrorKind::CloseDevice, "closing device: "),
            (ErrorKind::Switch, "switching session: "),
        ] {
            assert!(err(kind).to_string().starts_with(prefix));
        }
    }

    #[test]
    fn device_id_is_transparent() {
        assert_eq!(DeviceId(7).get(), 7);
        assert!(DeviceId(1) < DeviceId(2));
    }
}
