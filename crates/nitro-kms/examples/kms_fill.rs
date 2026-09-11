//! Hardware smoke test: open a DRM card directly (no libseat — run as root
//! or from a VT with no other master), modeset every connected output,
//! fill it with a gradient plus a bar that advances one column per flip
//! for three seconds, and print flip-interval statistics.
//!
//! ```text
//! sudo kms_fill /dev/dri/card1
//! ```
//!
//! Nothing is restored on exit; the kernel does that when the fd closes.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use nitro_kms::{Backend, BufferMut, DrmBackend, DrmOptions, Error, Event, OutputId, Rect};
use rustix::event::{PollFd, PollFlags, poll};

fn gradient_column(buf: &mut BufferMut<'_>, x: u32) {
    let r = (x * 255 / buf.width.max(1)) as u8;
    for y in 0..buf.height {
        let g = (y * 255 / buf.height.max(1)) as u8;
        let o = (y * buf.stride + x * 4) as usize;
        buf.data[o..o + 4].copy_from_slice(&[128, g, r, 0]);
    }
}

/// Per-output animation state.
struct Anim {
    id: OutputId,
    width: u32,
    height: u32,
    frames: u32,
    last_flip: Option<Duration>,
    intervals: Vec<Duration>,
}

impl Anim {
    /// Paint the next frame into the back buffer; returns the damage.
    fn paint(&self, buf: &mut BufferMut<'_>) -> Vec<Rect> {
        let (w, h) = (self.width, self.height);
        let x = self.frames % w;
        let mut damage = Vec::new();
        // Each buffer needs the gradient once; afterwards only the bar
        // moves. The back buffer lags two frames, so restore the two
        // previous bar columns plus paint the new one.
        if self.frames < 2 {
            for col in 0..w {
                gradient_column(buf, col);
            }
            damage.push(Rect::new(0, 0, w, h));
        } else {
            for back in 1..=2 {
                let px = (x + w - back) % w;
                gradient_column(buf, px);
                damage.push(Rect::new(px.cast_signed(), 0, 1, h));
            }
        }
        let bar = Rect::new(x.cast_signed(), 0, 1, h);
        buf.fill_rect(bar, 0x00FF_FFFF);
        damage.push(bar);
        damage
    }

    fn record(&mut self, t: Duration) {
        if let Some(prev) = self.last_flip {
            self.intervals.push(t.saturating_sub(prev));
        }
        self.last_flip = Some(t);
    }

    fn report(&self) {
        let iv = &self.intervals;
        if iv.is_empty() {
            println!("{}: no flips completed", self.id);
            return;
        }
        let total: Duration = iv.iter().sum();
        let mean = total / iv.len() as u32;
        let max = iv.iter().max().copied().unwrap_or_default();
        let min = iv.iter().min().copied().unwrap_or_default();
        println!(
            "{}: {} flips, interval mean {:.3} ms, min {:.3} ms, max {:.3} ms",
            self.id,
            iv.len() + 1,
            mean.as_secs_f64() * 1e3,
            min.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3
        );
    }
}

fn open(path: &str) -> Result<DrmBackend<'static>, Error> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error::Io {
            op: "open card",
            source: e,
        })?;
    let fd: OwnedFd = file.into();
    DrmBackend::open(fd, &DrmOptions::default())
}

fn wait_readable(kms: &DrmBackend<'_>) -> Result<(), Error> {
    let fds = kms.poll_fds();
    let mut pfds: Vec<PollFd<'_>> = fds
        .iter()
        .map(|fd| PollFd::new(fd, PollFlags::IN))
        .collect();
    let timeout = rustix::time::Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    poll(&mut pfds, Some(&timeout)).map_err(|e| Error::Io {
        op: "poll",
        source: e.into(),
    })?;
    Ok(())
}

fn main() -> Result<(), Error> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card1".to_owned());
    let mut kms = open(&path)?;
    if let Some(e) = kms.hotplug_error() {
        eprintln!("hotplug unavailable: {e}");
    }
    let mut anims: Vec<Anim> = kms
        .outputs()
        .iter()
        .map(|o| {
            println!(
                "{}: {} {}x{} @ {}.{:03} Hz, {}x{} mm",
                o.id,
                o.name,
                o.width,
                o.height,
                o.refresh_mhz / 1000,
                o.refresh_mhz % 1000,
                o.phys_mm.0,
                o.phys_mm.1
            );
            Anim {
                id: o.id,
                width: o.width,
                height: o.height,
                frames: 0,
                last_flip: None,
                intervals: Vec::new(),
            }
        })
        .collect();
    if anims.is_empty() {
        println!("no connected outputs");
        return Ok(());
    }

    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        for a in &mut anims {
            if kms.flip_pending(a.id) {
                continue;
            }
            let damage = a.paint(&mut kms.back_buffer(a.id)?);
            kms.commit(a.id, &damage)?;
            a.frames += 1;
        }
        wait_readable(&kms)?;
        events.clear();
        kms.dispatch(&mut events)?;
        for ev in &events {
            match ev {
                Event::Flipped { output, time, .. } => {
                    if let Some(a) = anims.iter_mut().find(|a| a.id == *output) {
                        a.record(*time);
                    }
                }
                Event::Hotplug => println!("hotplug"),
            }
        }
    }
    for a in &anims {
        a.report();
    }

    // Exercise the session and hotplug paths once: rescan with nothing
    // changed must be a no-op, pause/resume must re-modeset cleanly and
    // flips must keep working afterwards.
    let changed = kms.rescan()?;
    println!("rescan: changed={changed}");
    kms.pause();
    assert!(matches!(kms.commit(anims[0].id, &[]), Err(Error::Paused)));
    kms.resume()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut flips = 0;
    while Instant::now() < deadline {
        for a in &mut anims {
            if !kms.flip_pending(a.id) {
                let damage = a.paint(&mut kms.back_buffer(a.id)?);
                kms.commit(a.id, &damage)?;
                a.frames += 1;
            }
        }
        wait_readable(&kms)?;
        events.clear();
        kms.dispatch(&mut events)?;
        flips += events
            .iter()
            .filter(|e| matches!(e, Event::Flipped { .. }))
            .count();
    }
    println!("after resume: {flips} flips in 500 ms");
    Ok(())
}
