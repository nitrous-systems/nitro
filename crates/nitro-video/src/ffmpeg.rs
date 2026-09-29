//! [`SoftwareDecoder`]: H.264 decoded by an **`ffmpeg` child process**.
//!
//! # Why a child process
//!
//! The same trade `nitro-amp` makes (`crates/nitro-amp/src/source.rs`):
//! the codec is somebody else's decade of work, the machines this runs on
//! already have it, and a malformed file crashes `ffmpeg`, not the
//! player. The in-process alternative measured while planning #3906 —
//! `openh264` — mis-decoded B-frame streams (2 of 150 frames matched
//! `ffmpeg` for x264's default Main/High), and would have cost a C++
//! build, `nasm` and about 1 MB of binary. See `DEPENDENCIES.md`.
//!
//! # The pipes
//!
//! `ffmpeg` never sees the container. A writer thread reads each sample
//! from the file with `pread`, converts it from length-prefixed to
//! Annex-B ([`crate::annexb`]) — SPS/PPS first — and writes the stream to
//! the child's stdin; it exits on `EPIPE` when the child is killed. The
//! child writes raw NV12 frames to stdout, which is non-blocking: the
//! player's loop reads it **directly into a ring slot** as it becomes
//! readable, with no intermediate copy.
//!
//! # Timestamps
//!
//! The child is told nothing about time (`-fps_mode passthrough`): it
//! emits frames in presentation order, one per access unit, so the nth
//! output frame gets the nth pts of the samples from the start keyframe
//! on, sorted. That is exact for closed GOPs (x264's default: every
//! keyframe an IDR); in an open-GOP stream the leading pictures after a
//! seek point are dropped by the decoder and the mapping slips by that
//! many frames until the next IDR — a documented v1 limitation.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;

use crate::annexb;
use crate::decode::{Decoded, Decoder, FrameBuf, FrameSink, Nv12Layout, Poll};
use crate::mp4::Track;

/// The directories on `$PATH`.
#[must_use]
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// The first executable called `name` in `dirs` (copied from
/// `nitro-amp` rather than depending on it).
#[must_use]
pub fn find_program(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    dirs.iter().map(|d| d.join(name)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// One decoding run: the child, its pipes and the writer thread.
struct Run {
    child: Child,
    out: ChildStdout,
    writer: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<String>>,
    /// The pts still to hand out, in presentation order.
    pts: VecDeque<i64>,
    /// The last pts handed out, to extrapolate if the child emits more
    /// frames than there were samples.
    last_pts: i64,
    /// The slot being filled and how many bytes it holds.
    cur: Option<(usize, usize)>,
}

/// H.264 → NV12 through an `ffmpeg` child. See the module docs.
pub struct SoftwareDecoder {
    ffmpeg: Option<PathBuf>,
    file: PathBuf,
    layout: Nv12Layout,
    run: Option<Run>,
    /// Frames completed, over every run.
    pub decoded: u64,
}

impl SoftwareDecoder {
    /// A decoder for `file`, using the `ffmpeg` found in `dirs` (pass
    /// [`path_dirs`] for the real thing). A missing `ffmpeg` is reported
    /// by [`Decoder::start`], so the caller can still show its window.
    #[must_use]
    pub fn new(file: &Path, layout: Nv12Layout, dirs: &[PathBuf]) -> Self {
        Self {
            ffmpeg: find_program(dirs, "ffmpeg"),
            file: file.to_path_buf(),
            layout,
            run: None,
            decoded: 0,
        }
    }

    /// Whether an `ffmpeg` was found.
    #[must_use]
    pub fn has_ffmpeg(&self) -> bool {
        self.ffmpeg.is_some()
    }

    /// The child's process id while a run is live (for measurements).
    #[must_use]
    pub fn child_pid(&self) -> Option<u32> {
        self.run.as_ref().map(|r| r.child.id())
    }

    /// Reap the finished child and turn a failure into a message.
    fn finish(&mut self) -> Result<Poll, String> {
        let Some(mut run) = self.run.take() else {
            return Ok(Poll::Eof);
        };
        let status = run.child.wait().map_err(|e| format!("ffmpeg: {e}"))?;
        if let Some(w) = run.writer.take() {
            let _ = w.join();
        }
        let err = run
            .stderr
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        if status.success() {
            Ok(Poll::Eof)
        } else {
            let msg = err.trim();
            Err(if msg.is_empty() {
                format!("ffmpeg failed: {status}")
            } else {
                format!("ffmpeg failed: {msg}")
            })
        }
    }
}

/// The child's command line.
fn command(ffmpeg: &Path, layout: Nv12Layout, track: &Track) -> Command {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "pipe:0"]);
    if track.width != layout.width || track.height != layout.height {
        // Odd dimensions: NV12 needs even ones.
        cmd.args([
            "-vf",
            &format!("crop={}:{}:0:0", layout.width, layout.height),
        ]);
    }
    cmd.args([
        "-fps_mode",
        "passthrough",
        "-pix_fmt",
        "nv12",
        "-f",
        "rawvideo",
        "pipe:1",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    cmd
}

/// The writer thread's body: samples `from..` as Annex-B on `stdin`.
fn feed(
    file: &Path,
    samples: &[(u64, u32)],
    head: &[u8],
    nal_len: u8,
    mut stdin: impl Write,
) -> io::Result<()> {
    let f = std::fs::File::open(file)?;
    let mut buf = Vec::new();
    let mut out = Vec::with_capacity(head.len() + 64 * 1024);
    out.extend_from_slice(head);
    for &(offset, size) in samples {
        buf.resize(size as usize, 0);
        f.read_exact_at(&mut buf, offset)?;
        annexb::to_annexb(&buf, nal_len, &mut out).map_err(io::Error::other)?;
        stdin.write_all(&out)?;
        out.clear();
    }
    Ok(())
}

impl Decoder for SoftwareDecoder {
    fn start(&mut self, track: &Track, from: usize) -> Result<(), String> {
        self.stop();
        let Some(ffmpeg) = self.ffmpeg.clone() else {
            return Err(
                "ffmpeg is not installed: nitro-video decodes through an ffmpeg child process"
                    .to_owned(),
            );
        };
        let cfg = annexb::parse_avcc(&track.avcc)?;
        let head = cfg.parameter_sets();
        let samples: Vec<(u64, u32)> = track
            .samples
            .get(from..)
            .unwrap_or(&[])
            .iter()
            .map(|s| (s.offset, s.size))
            .collect();
        let mut child = command(&ffmpeg, self.layout, track)
            .spawn()
            .map_err(|e| format!("{}: {e}", ffmpeg.display()))?;
        let stdin = child.stdin.take().ok_or("ffmpeg: no stdin pipe")?;
        let out = child.stdout.take().ok_or("ffmpeg: no stdout pipe")?;
        let mut err = child.stderr.take().ok_or("ffmpeg: no stderr pipe")?;
        let flags = rustix::fs::fcntl_getfl(&out).map_err(|e| e.to_string())?;
        rustix::fs::fcntl_setfl(&out, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| e.to_string())?;
        let file = self.file.clone();
        let nal_len = cfg.nal_length_size;
        let writer = std::thread::Builder::new()
            .name("nitro-video-feed".to_owned())
            .spawn(move || {
                // EPIPE is how a stop or a seek ends us; anything else is
                // reported by ffmpeg running dry, so it is dropped here.
                let _ = feed(&file, &samples, &head, nal_len, stdin);
            })
            .map_err(|e| e.to_string())?;
        let stderr = std::thread::Builder::new()
            .name("nitro-video-stderr".to_owned())
            .spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                s
            })
            .map_err(|e| e.to_string())?;
        let pts: VecDeque<i64> = track.pts_sorted_from(from).into();
        self.run = Some(Run {
            child,
            out,
            writer: Some(writer),
            stderr: Some(stderr),
            last_pts: pts.front().copied().unwrap_or(0),
            pts,
            cur: None,
        });
        Ok(())
    }

    fn readiness_fd(&self) -> Option<BorrowedFd<'_>> {
        self.run.as_ref().map(|r| r.out.as_fd())
    }

    fn next_frame(&mut self, sink: &mut dyn FrameSink) -> Result<Poll, String> {
        let frame_len = self.layout.frame_len();
        let Some(run) = self.run.as_mut() else {
            return Ok(Poll::Eof);
        };
        let (slot, mut filled) = match run.cur {
            Some(c) => c,
            None => match sink.reserve() {
                Some(i) => (i, 0),
                None => return Ok(Poll::Pending),
            },
        };
        let buf = sink.slot(slot);
        loop {
            match run.out.read(&mut buf[filled..frame_len]) {
                Ok(0) => {
                    run.cur = Some((slot, filled));
                    return self.finish();
                }
                Ok(n) => {
                    filled += n;
                    if filled == frame_len {
                        run.cur = None;
                        let pts = run.pts.pop_front().unwrap_or(run.last_pts + 1);
                        run.last_pts = pts;
                        self.decoded += 1;
                        return Ok(Poll::Frame(Decoded {
                            pts,
                            buf: FrameBuf::Shm(slot),
                        }));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    run.cur = Some((slot, filled));
                    return Ok(Poll::Pending);
                }
                Err(e) => return Err(format!("ffmpeg output: {e}")),
            }
        }
    }

    fn stop(&mut self) {
        let Some(mut run) = self.run.take() else {
            return;
        };
        let _ = run.child.kill();
        let _ = run.child.wait();
        // Closing our end of stdout is what the writer needs to see its
        // EPIPE if the kill raced it.
        drop(run.out);
        if let Some(w) = run.writer.take() {
            let _ = w.join();
        }
        if let Some(h) = run.stderr.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SoftwareDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::Sample;

    /// A scratch directory under the target dir, unique per test.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nitro-video-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A fake `ffmpeg`: swallows stdin into `in.h264` next to itself,
    /// then writes `frames` NV12 frames of `w×h`, each filled with the
    /// frame number.
    fn fake_ffmpeg(dir: &Path, w: u32, h: u32, frames: u32, exit: i32) {
        let len = w * h * 3 / 2;
        let mut script = format!("#!/bin/sh\ncat > '{}/in.h264'\n", dir.display());
        for i in 1..=frames {
            script += &format!("head -c {len} /dev/zero | tr '\\000' '\\{i:03o}'\n");
        }
        script += &format!("echo boom >&2\nexit {exit}\n");
        let p = dir.join("ffmpeg");
        std::fs::write(&p, script).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// avcC with 4-byte lengths, one SPS `[0x67, 1]` and one PPS `[0x68, 2]`.
    const AVCC: [u8; 17] = [
        1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 2, 0, 0,
    ];

    /// A track of `n` one-NAL samples in a file, with B-frame-style pts.
    fn track(dir: &Path, n: usize) -> (Track, PathBuf) {
        let mut data = Vec::new();
        let mut samples = Vec::new();
        for i in 0..n {
            let offset = data.len() as u64;
            data.extend_from_slice(&[0, 0, 0, 2, 0x65, i as u8]);
            // Decode order I P B B: pts 0, 3, 1, 2, then +4 per group.
            let g = (i / 4) as i64 * 4;
            let pts = g + [0, 3, 1, 2][i % 4];
            samples.push(Sample {
                offset,
                size: 6,
                dts: i as i64,
                pts,
                sync: i % 4 == 0,
            });
        }
        let path = dir.join("clip.bin");
        std::fs::write(&path, &data).unwrap();
        let t = Track {
            width: 4,
            height: 2,
            timescale: 30,
            duration: n as u64,
            avcc: AVCC[..16].to_vec(),
            color: None,
            samples,
        };
        (t, path)
    }

    struct Ring {
        slots: Vec<Vec<u8>>,
        free: Vec<usize>,
    }

    impl FrameSink for Ring {
        fn reserve(&mut self) -> Option<usize> {
            self.free.pop()
        }
        fn slot(&mut self, i: usize) -> &mut [u8] {
            &mut self.slots[i]
        }
    }

    fn drain(d: &mut SoftwareDecoder, ring: &mut Ring) -> Result<Vec<(i64, u8)>, String> {
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline, "timed out");
            match d.next_frame(ring)? {
                Poll::Frame(f) => {
                    let FrameBuf::Shm(i) = f.buf;
                    got.push((f.pts, ring.slots[i][0]));
                    ring.free.push(i);
                }
                Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(2)),
                Poll::Eof => return Ok(got),
            }
        }
    }

    fn ring(layout: Nv12Layout) -> Ring {
        Ring {
            slots: vec![vec![0; layout.frame_len()]; 2],
            free: vec![0, 1],
        }
    }

    #[test]
    fn frames_come_back_in_pts_order_and_the_stream_is_annexb() {
        let dir = scratch("order");
        let layout = Nv12Layout::for_video(4, 2);
        fake_ffmpeg(&dir, 4, 2, 8, 0);
        let (t, path) = track(&dir, 8);
        let mut d = SoftwareDecoder::new(&path, layout, std::slice::from_ref(&dir));
        assert!(d.has_ffmpeg());
        d.start(&t, 0).unwrap();
        let mut r = ring(layout);
        let got = drain(&mut d, &mut r).unwrap();
        let pts: Vec<i64> = got.iter().map(|g| g.0).collect();
        assert_eq!(pts, (0..8).collect::<Vec<_>>());
        let marks: Vec<u8> = got.iter().map(|g| g.1).collect();
        assert_eq!(marks, (1..=8).collect::<Vec<u8>>(), "each frame whole");
        assert_eq!(d.decoded, 8);
        let fed = std::fs::read(dir.join("in.h264")).unwrap();
        let mut want = vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2];
        for i in 0..8u8 {
            want.extend_from_slice(&[0, 0, 0, 1, 0x65, i]);
        }
        assert_eq!(fed, want);
    }

    #[test]
    fn a_restart_from_a_keyframe_maps_the_later_pts() {
        let dir = scratch("seek");
        let layout = Nv12Layout::for_video(4, 2);
        fake_ffmpeg(&dir, 4, 2, 4, 0);
        let (t, path) = track(&dir, 8);
        let mut d = SoftwareDecoder::new(&path, layout, std::slice::from_ref(&dir));
        d.start(&t, 0).unwrap();
        // Kill mid-run, as a seek does, then restart at sample 4.
        d.stop();
        d.start(&t, 4).unwrap();
        let got = drain(&mut d, &mut ring(layout)).unwrap();
        let pts: Vec<i64> = got.iter().map(|g| g.0).collect();
        assert_eq!(pts, vec![4, 5, 6, 7]);
    }

    #[test]
    fn a_failing_child_reports_its_stderr() {
        let dir = scratch("fail");
        let layout = Nv12Layout::for_video(4, 2);
        fake_ffmpeg(&dir, 4, 2, 1, 3);
        let (t, path) = track(&dir, 4);
        let mut d = SoftwareDecoder::new(&path, layout, std::slice::from_ref(&dir));
        d.start(&t, 0).unwrap();
        let e = drain(&mut d, &mut ring(layout)).unwrap_err();
        assert!(e.contains("boom"), "{e}");
    }

    #[test]
    fn a_full_ring_is_back_pressure() {
        let dir = scratch("full");
        let layout = Nv12Layout::for_video(4, 2);
        fake_ffmpeg(&dir, 4, 2, 2, 0);
        let (t, path) = track(&dir, 4);
        let mut d = SoftwareDecoder::new(&path, layout, std::slice::from_ref(&dir));
        d.start(&t, 0).unwrap();
        let mut r = Ring {
            slots: vec![vec![0; layout.frame_len()]],
            free: Vec::new(),
        };
        assert_eq!(d.next_frame(&mut r).unwrap(), Poll::Pending);
        d.stop();
    }

    #[test]
    fn no_ffmpeg_is_a_plain_error() {
        let dir = scratch("none");
        let (t, path) = track(&dir, 4);
        let mut d = SoftwareDecoder::new(&path, Nv12Layout::for_video(4, 2), &[dir]);
        assert!(!d.has_ffmpeg());
        let e = d.start(&t, 0).unwrap_err();
        assert!(e.contains("ffmpeg is not installed"), "{e}");
    }
}
