//! `nitro-shot --record N`: a test client of the capture ops (#676).
//!
//! Connects to the wire socket with `caps::CAPTURE`, picks the output
//! (`--output NAME`, else the first), starts a capture, and for each
//! `CaptureFrame` waits for its fence, maps the LINEAR slot, releases it
//! and counts. At the end it prints fps, latency (flip → fence
//! signalled, as the client sees it), damage and the ring size to
//! stderr, and writes the last frame as PNG with `-o FILE`.

use std::io;
use std::os::fd::AsFd as _;
use std::path::Path;
use std::time::{Duration, Instant};

use nitro_wire::client::Connection;
use nitro_wire::msg::{CaptureBuffers, ServerMsg};
use nitro_wire::types::caps;

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(t.tv_nsec).unwrap_or(0)
}

/// Wait until `fd` is readable (a `sync_file` signalled), at most 1 s.
fn wait_fence(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut fds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    let t = rustix::event::Timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    matches!(rustix::event::poll(&mut fds, Some(&t)), Ok(n) if n > 0)
}

/// Block until at least one message arrives.
fn next(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        conn.flush().map_err(err)?;
        if conn.poll(seen).map_err(err)? > 0 {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(err("no message from the server in 10 s"));
        }
        let fd = conn.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let t = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        let _ = rustix::event::poll(&mut fds, Some(&t));
    }
}

/// The capture id this client uses.
const ID: u32 = 1;

/// Record `frames` frames; see the module doc.
#[allow(clippy::too_many_lines)] // One recording's steps in order.
pub fn run(frames: u32, output: Option<&str>, fps: u32, file: Option<&Path>) -> io::Result<()> {
    let mut conn = Connection::connect_default("nitro-shot-record").map_err(err)?;
    if !conn.has_caps(caps::CAPTURE) {
        return Err(err("the server does not offer caps::CAPTURE"));
    }
    conn.client_caps(caps::CAPTURE | caps::OUTPUTS)
        .map_err(err)?;
    conn.list_outputs().map_err(err)?;
    let mut seen = Vec::new();
    let mut outputs = Vec::new();
    loop {
        next(&mut conn, &mut seen)?;
        let mut end = false;
        for m in seen.drain(..) {
            match m {
                ServerMsg::OutputInfo(o) => outputs.push(o),
                ServerMsg::OutputsEnd(_) => end = true,
                _ => {}
            }
        }
        if end {
            break;
        }
    }
    let out = match output {
        Some(n) => outputs.iter().find(|o| o.name == n),
        None => outputs.first(),
    }
    .ok_or_else(|| err(format!("no output {}", output.unwrap_or(""))))?;
    eprintln!(
        "recording {} ({}x{}) for {frames} frames at ≤{} fps",
        out.name,
        out.w,
        out.h,
        if fps == 0 {
            "output".to_owned()
        } else {
            fps.to_string()
        }
    );
    conn.capture_start(ID, out.id, fps).map_err(err)?;
    let mut ring: Option<CaptureBuffers> = None;
    let mut got = 0u32;
    let mut first: Option<Instant> = None;
    let mut lat_us: Vec<u64> = Vec::new();
    let mut damage_px = 0u64;
    let mut last: Option<Vec<u8>> = None;
    while got < frames {
        next(&mut conn, &mut seen)?;
        for m in std::mem::take(&mut seen) {
            match m {
                ServerMsg::CaptureBuffers(b) => {
                    let bytes: u64 = b.slots.iter().map(|s| s.size).sum();
                    eprintln!(
                        "ring: {} slots, {}x{}, modifier {:#x}, {} KiB",
                        b.slots.len(),
                        b.width,
                        b.height,
                        b.modifier,
                        bytes / 1024
                    );
                    ring = Some(b);
                }
                ServerMsg::CaptureFrame(f) => {
                    let ok = wait_fence(f.fence.as_fd());
                    let signalled = now_ns();
                    first.get_or_insert_with(Instant::now);
                    got += 1;
                    lat_us.push(signalled.saturating_sub(f.time_ns) / 1000);
                    damage_px += f
                        .damage
                        .iter()
                        .map(|r| u64::from(r.w.unsigned_abs()) * u64::from(r.h.unsigned_abs()))
                        .sum::<u64>();
                    if ok
                        && got == frames
                        && file.is_some()
                        && let Some(b) = &ring
                        && b.modifier == 0
                        && let Some(s) = b.slots.get(usize::from(f.slot))
                    {
                        let len = usize::try_from(s.size).unwrap_or(0);
                        let map = nitro_shm::DmaBufMapping::map(s.fd.as_fd(), len).map_err(err)?;
                        let _ = nitro_shm::sync_start(s.fd.as_fd(), nitro_shm::SyncAccess::Read);
                        let off = s.offset as usize;
                        let n = s.pitch as usize * b.height as usize;
                        last = map.as_bytes().get(off..off + n).map(<[u8]>::to_vec);
                        let _ = nitro_shm::sync_end(s.fd.as_fd(), nitro_shm::SyncAccess::Read);
                    }
                    conn.capture_release(ID, f.slot).map_err(err)?;
                }
                ServerMsg::CaptureStopped(s) => {
                    return Err(err(format!("capture stopped: {:?}", s.reason)));
                }
                ServerMsg::Error(e) => return Err(err(format!("server: {}", e.msg))),
                _ => {}
            }
        }
    }
    let secs = first.map_or(0.0, |t| t.elapsed().as_secs_f64());
    conn.capture_stop(ID).map_err(err)?;
    conn.flush().map_err(err)?;
    lat_us.sort_unstable();
    let pct = |p: usize| {
        lat_us
            .get((lat_us.len() * p / 100).min(lat_us.len() - 1))
            .copied()
    };
    let fps_got = if secs > 0.0 && got > 1 {
        f64::from(got - 1) / secs
    } else {
        0.0
    };
    eprintln!(
        "frames {got} fps {fps_got:.1} latency_us p50 {} p95 {} max {} damage_px_per_frame {}",
        pct(50).unwrap_or(0),
        pct(95).unwrap_or(0),
        lat_us.last().copied().unwrap_or(0),
        damage_px / u64::from(got.max(1))
    );
    if let (Some(path), Some(b)) = (file, &ring) {
        match last {
            Some(px) => {
                let stride = b.slots[0].pitch;
                std::fs::write(
                    path,
                    crate::png::encode_xrgb(b.width, b.height, stride, &px),
                )?;
            }
            None => eprintln!("no LINEAR frame to write"),
        }
    }
    Ok(())
}
