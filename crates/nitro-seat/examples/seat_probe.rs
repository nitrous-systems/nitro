//! Opens the seat, reports events for three seconds (second argument
//! overrides), opens a DRM device (default `/dev/dri/card1`, first argument
//! overrides), prints its fd and closes everything in order. Needs a real seat: run it on the test box
//! inside a logind session (see the crate README).

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nitro_seat::{Seat, SeatEvent};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("seat_probe: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), nitro_seat::Error> {
    let path = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let secs = std::env::args()
        .nth(2)
        .map_or(3, |s| s.parse().expect("seconds must be a number"));

    let mut seat = Seat::open()?;
    println!(
        "seat {:?} opened, active={}, poll fd={:?}",
        seat.name(),
        seat.is_active(),
        seat.as_fd()
    );

    // Poll with plain sleeps: this is a probe, not the server's epoll loop.
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        for ev in seat.dispatch()? {
            println!("event {ev:?} (active={})", seat.is_active());
            if ev == SeatEvent::Disable {
                seat.ack_disable()?;
                println!("acknowledged disable");
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let dev = seat.open_device(&path)?;
    println!(
        "opened {} as {:?}, fd={:?}",
        path.display(),
        dev.id(),
        dev.as_fd()
    );
    seat.close_device(dev)?;
    println!("closed device; open_devices={}", seat.open_devices());

    drop(seat);
    println!("seat closed");
    Ok(())
}
