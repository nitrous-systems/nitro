//! End-to-end exercise against libseat's `noop` backend, which needs no
//! logind/seatd and opens device paths with a plain `open(2)` — enough to
//! drive every code path of the wrapper in CI.
//!
//! `LIBSEAT_BACKEND` is process-wide, so the outer tests re-execute this
//! test binary with the variable set and run the `#[ignore]`d inner tests
//! there (no `set_var`, which is `unsafe` in edition 2024).

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::process::Command;

use nitro_seat::{ErrorKind, Seat, SeatEvent};

fn rerun_with_noop(inner: &str) {
    let status = Command::new(std::env::current_exe().unwrap())
        .env("LIBSEAT_BACKEND", "noop")
        .args(["--exact", inner, "--ignored", "--nocapture"])
        .status()
        .expect("re-exec test binary");
    assert!(
        status.success(),
        "{inner} failed under LIBSEAT_BACKEND=noop"
    );
}

#[test]
fn noop_roundtrip() {
    rerun_with_noop("noop_roundtrip_inner");
}

#[test]
fn noop_drop_seat_with_live_device() {
    rerun_with_noop("noop_drop_seat_with_live_device_inner");
}

#[test]
#[ignore = "run via noop_roundtrip"]
fn noop_roundtrip_inner() {
    assert_eq!(std::env::var("LIBSEAT_BACKEND").as_deref(), Ok("noop"));

    let mut seat = Seat::open().expect("open noop seat");
    assert_eq!(seat.name(), "seat0");
    assert!(seat.as_fd().as_raw_fd() >= 0);
    assert!(!seat.is_active(), "noop enables on first dispatch");
    assert_eq!(seat.open_devices(), 0);

    assert_eq!(seat.dispatch().unwrap(), [SeatEvent::Enable]);
    assert!(seat.is_active());
    assert_eq!(seat.dispatch().unwrap(), [], "events are drained once");
    assert!(format!("{seat:?}").contains("active: true"));

    // The fd is live: /dev/null reads as empty.
    let dev = seat.open_device(Path::new("/dev/null")).unwrap();
    assert_eq!(dev.path(), Path::new("/dev/null"));
    assert_eq!(seat.open_devices(), 1);
    let mut file = std::fs::File::from(dev.as_fd().try_clone_to_owned().unwrap());
    let mut buf = [0u8; 8];
    assert_eq!(file.read(&mut buf).unwrap(), 0);

    // Dropping closes through the seat.
    let first_id = dev.id();
    drop(dev);
    assert_eq!(seat.open_devices(), 0);

    // Explicit close reports success; ids are unique.
    let dev = seat.open_device(Path::new("/dev/null")).unwrap();
    assert_ne!(dev.id(), first_id);
    seat.close_device(dev).unwrap();
    assert_eq!(seat.open_devices(), 0);

    // Failure carries the path.
    let err = seat
        .open_device(Path::new("/nonexistent/device"))
        .unwrap_err();
    assert_eq!(
        err.kind(),
        &ErrorKind::OpenDevice {
            path: "/nonexistent/device".into()
        }
    );
    assert_eq!(err.raw_os_error(), Some(2));
    assert!(
        err.to_string()
            .starts_with("opening device /nonexistent/device: "),
        "{err}"
    );
    assert_eq!(seat.open_devices(), 0);

    // noop acks a disable without doing anything and has no VT to switch.
    seat.ack_disable().unwrap();
    let err = seat.switch_session(2).unwrap_err();
    assert_eq!(err.kind(), &ErrorKind::Switch);
    assert!(err.to_string().starts_with("switching session: "), "{err}");
}

#[test]
#[ignore = "run via noop_drop_seat_with_live_device"]
#[cfg_attr(debug_assertions, should_panic(expected = "seat must be dropped last"))]
fn noop_drop_seat_with_live_device_inner() {
    let mut seat = Seat::open().expect("open noop seat");
    seat.dispatch().unwrap();
    let _dev = seat.open_device(Path::new("/dev/null")).unwrap();
    drop(seat);
    // Release builds reach here: `_dev` drops after its seat and gives up
    // silently.
}
