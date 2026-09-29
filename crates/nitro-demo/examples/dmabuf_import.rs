//! `dmabuf_import`: import one client dma-buf into a `Surface` node and
//! present it (#3918).
//!
//! Connects, lists `DMABUF | SURFACE | RELEASE`, prints every
//! `DmabufFeedback` it receives, opens a window whose content is a
//! `Surface` node, registers the buffer with `CreateDmabufBuffer` and
//! shows it with a plain `PresentSurface` (implicit sync: the server
//! snapshots the buffer's write fences). It waits for `Presented`, prints
//! the timings, stays up for `--hold` seconds and finally prints the
//! server's `dmabuf_*` / `fence*` stats from the control socket.
//!
//! Sources:
//!
//! ```text
//! dmabuf_import --fd 3 --format NV12 --modifier 0x0100000000000002 \
//!               --size 1920x1080 --planes 0:2048,2228224:2048
//! dmabuf_import --linear [--size 1920x1080]
//! dmabuf_import --memfd  [--size 1920x1080]
//! ```
//!
//! - `--fd N`: an inherited descriptor (e.g. from `va_export`, see
//!   `docs/research/gpu-testbox/va_export.c`); every plane names the same
//!   fd. It is taken over with `pidfd_getfd` on ourselves, so no `unsafe`
//!   `from_raw_fd` is needed.
//! - `--linear`: a real linear dma-buf — a DRM dumb buffer (`R8`,
//!   `height * 3 / 2` rows) on the first `/dev/dri/card*`, exported with
//!   PRIME and filled with NV12 colour bars through the dumb mapping. No
//!   DRM master needed.
//! - `--memfd`: the same bars in a sealed memfd (not a dma-buf; the server
//!   accepts it on the same path, which is useful as a control).

use std::error::Error;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use drm::buffer::Buffer as _;
use drm::control::Device as _;
use nitro_core::{IRect, Rect, Size};
use nitro_demo::monotonic_ns;
use nitro_demo::video::{Frame, Layout, draw_full};
use nitro_wire::client::Connection;
use nitro_wire::msg::{CreateDmabufBuffer, DmabufFeedback, DmabufPlane, PresentSurface, ServerMsg};
use nitro_wire::types::{
    BufferId, ColorMatrix, ColorRange, Layer, NodeId, caps, dmabuf_flags, format, modifier,
};
use rustix::event::{PollFd, PollFlags, Timespec};

type Res<T> = Result<T, Box<dyn Error>>;

const WINDOW: NodeId = NodeId(1);
const SURFACE: NodeId = NodeId(2);
const BUFFER: BufferId = BufferId(1);
const COMMIT_SERIAL: u32 = 1;
const PRESENT_SERIAL: u32 = 2;

// ------------------------------------------------------------ args

#[derive(Debug)]
enum Source {
    Fd { fd: i32, planes: Vec<(u32, u32)> },
    Linear,
    Memfd,
}

#[derive(Debug)]
struct Opts {
    source: Source,
    format: u32,
    modifier: u64,
    size: (u32, u32),
    hold: u64,
}

const USAGE: &str = "usage: dmabuf_import (--fd N --format NV12 --modifier 0x.. --size WxH \
                     --planes off:stride[,off:stride..] | --linear | --memfd) [--size WxH] \
                     [--hold SECS]";

fn parse_u64(s: &str) -> Res<u64> {
    Ok(if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16)?
    } else {
        s.parse()?
    })
}

fn parse_fourcc(s: &str) -> Res<u32> {
    let b = s.as_bytes();
    if b.len() == 4 {
        return Ok(format::fourcc(&[b[0], b[1], b[2], b[3]]));
    }
    Ok(u32::try_from(parse_u64(s)?)?)
}

fn parse_size(s: &str) -> Res<(u32, u32)> {
    let (w, h) = s.split_once('x').ok_or("size is WxH")?;
    Ok((w.parse()?, h.parse()?))
}

fn parse_planes(s: &str) -> Res<Vec<(u32, u32)>> {
    s.split(',')
        .map(|p| {
            let (o, st) = p.split_once(':').ok_or("a plane is off:stride")?;
            Ok((
                u32::try_from(parse_u64(o)?)?,
                u32::try_from(parse_u64(st)?)?,
            ))
        })
        .collect()
}

fn parse_args() -> Res<Opts> {
    let mut fd = None;
    let mut planes = Vec::new();
    let mut kind = None;
    let mut opts = Opts {
        source: Source::Memfd,
        format: format::NV12,
        modifier: modifier::LINEAR,
        size: (1920, 1080),
        hold: 5,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--fd" => fd = Some(val()?.parse::<i32>()?),
            "--format" => opts.format = parse_fourcc(&val()?)?,
            "--modifier" => opts.modifier = parse_u64(&val()?)?,
            "--size" => opts.size = parse_size(&val()?)?,
            "--planes" => planes = parse_planes(&val()?)?,
            "--hold" => opts.hold = val()?.parse()?,
            "--linear" => kind = Some(Source::Linear),
            "--memfd" => kind = Some(Source::Memfd),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {a:?}\n{USAGE}").into()),
        }
    }
    opts.source = match (fd, kind) {
        (Some(fd), None) => {
            if planes.is_empty() {
                return Err("--fd needs --planes".into());
            }
            Source::Fd { fd, planes }
        }
        (None, Some(k)) => k,
        _ => return Err(USAGE.into()),
    };
    if !matches!(opts.source, Source::Fd { .. }) {
        opts.format = format::NV12;
        opts.modifier = modifier::LINEAR;
        opts.size = (opts.size.0 & !1, opts.size.1 & !1);
    }
    Ok(opts)
}

// ------------------------------------------------------------ names

fn fourcc_name(f: u32) -> String {
    f.to_le_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

fn flag_names(f: u32) -> String {
    let mut v = Vec::new();
    for (bit, name) in [
        (dmabuf_flags::SCANOUT, "SCANOUT"),
        (dmabuf_flags::CPU, "CPU"),
        (dmabuf_flags::IMPORT, "IMPORT"),
    ] {
        if f & bit != 0 {
            v.push(name.to_owned());
        }
    }
    let rest = f & !(dmabuf_flags::SCANOUT | dmabuf_flags::CPU | dmabuf_flags::IMPORT);
    if rest != 0 {
        v.push(format!("{rest:#x}"));
    }
    v.join("|")
}

fn print_feedback(f: &DmabufFeedback, t0: Instant) {
    let dev = f.main_device;
    // glibc's gnu_dev_major/minor.
    let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
    let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
    println!(
        "feedback id={} main_device={major}:{minor} max={}x{} formats={} (+{:.1} ms)",
        f.id.0,
        f.max_width,
        f.max_height,
        f.formats.len(),
        t0.elapsed().as_secs_f64() * 1e3
    );
    for e in &f.formats {
        println!(
            "  {} {:<22} {}",
            fourcc_name(e.format),
            nitro_kms::modifier_name(e.modifier),
            flag_names(e.flags)
        );
    }
}

// ------------------------------------------------------------ sources

/// A DRM card node, for dumb buffers.
struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

fn open_card() -> Res<(Card, String)> {
    let mut last = None;
    for i in 0..8 {
        let path = format!("/dev/dri/card{i}");
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(f) => return Ok((Card(f), path)),
            Err(e) => last = Some(format!("{path}: {e}")),
        }
    }
    Err(last.unwrap_or_else(|| "no /dev/dri/card*".into()).into())
}

/// The buffer to import, and what keeps it alive.
struct Imported {
    planes: Vec<(OwnedFd, u32, u32)>,
    _card: Option<Card>,
}

fn nv12_bars(buf: &mut [u8], frame: Frame) {
    draw_full(buf, &Layout::new(frame, 60), 0);
}

fn source_linear(w: u32, h: u32) -> Res<Imported> {
    let t = Instant::now();
    let (card, path) = open_card()?;
    let rows = h + h / 2;
    let mut db = card.create_dumb_buffer((w, rows), drm::buffer::DrmFourcc::R8, 8)?;
    let pitch = db.pitch();
    let frame = Frame {
        fourcc: format::NV12,
        width: w,
        height: h,
        offset0: 0,
        stride0: pitch,
        offset1: pitch * h,
        stride1: pitch,
    };
    {
        let mut map = card.map_dumb_buffer(&mut db)?;
        nv12_bars(&mut map, frame);
    }
    let fd = card.buffer_to_prime_fd(db.handle(), drm::CLOEXEC | drm::RDWR)?;
    let dup = rustix::io::dup(&fd)?;
    println!(
        "source: dumb buffer on {path}: R8 {w}x{rows} pitch={pitch} prime fd, dma-buf={} \
         ({:.2} ms incl. fill)",
        nitro_shm::is_dmabuf(&fd),
        t.elapsed().as_secs_f64() * 1e3
    );
    Ok(Imported {
        planes: vec![(fd, 0, pitch), (dup, pitch * h, pitch)],
        _card: Some(card),
    })
}

fn source_memfd(w: u32, h: u32) -> Res<Imported> {
    let frame = Frame::nv12(w, h);
    let len = frame.bytes();
    let fd = nitro_shm::create_sealed("dmabuf_import", len as u64)?;
    let mut map = nitro_shm::MappingMut::map_mut(fd.as_fd(), len)?;
    nv12_bars(map.as_bytes_mut(), frame);
    drop(map);
    let dup = rustix::io::dup(&fd)?;
    println!("source: sealed memfd {len} bytes (NOT a dma-buf)");
    Ok(Imported {
        planes: vec![(fd, 0, w), (dup, w * h, w)],
        _card: None,
    })
}

fn source_fd(raw: i32, planes: &[(u32, u32)]) -> Res<Imported> {
    // Take the inherited descriptor over without `from_raw_fd`: ask the
    // kernel for a duplicate of our own fd `raw`.
    let pidfd = rustix::process::pidfd_open(
        rustix::process::getpid(),
        rustix::process::PidfdFlags::empty(),
    )?;
    let mut out = Vec::with_capacity(planes.len());
    for &(off, stride) in planes {
        let fd =
            rustix::process::pidfd_getfd(&pidfd, raw, rustix::process::PidfdGetfdFlags::empty())?;
        out.push((fd, off, stride));
    }
    let st = rustix::fs::fstat(&out[0].0)?;
    println!(
        "source: inherited fd {raw}: dma-buf={} size={}",
        nitro_shm::is_dmabuf(&out[0].0),
        st.st_size
    );
    Ok(Imported {
        planes: out,
        _card: None,
    })
}

// ------------------------------------------------------------ the client

fn poll_conn(conn: &Connection, timeout: Duration) -> Res<()> {
    let fd = conn.as_fd();
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let ts = Timespec {
        tv_sec: i64::try_from(timeout.as_secs())?,
        tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    match rustix::event::poll(&mut fds, Some(&ts)) {
        Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn flush(conn: &mut Connection) -> Res<()> {
    while !conn.flush()? {
        let fd = conn.as_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
        rustix::event::poll(&mut fds, None)?;
    }
    Ok(())
}

#[derive(Default)]
struct State {
    feedbacks: u32,
    presented: Option<(u64, u64, u64)>, // (client recv ns, server time_ns, seq)
    released: bool,
    committed: Option<u64>,
    closed: bool,
}

fn handle(msgs: &[ServerMsg], st: &mut State, t0: Instant) -> Res<()> {
    for m in msgs {
        match m {
            ServerMsg::DmabufFeedback(f) => {
                st.feedbacks += 1;
                print_feedback(f, t0);
            }
            ServerMsg::Presented(p) if p.serial == PRESENT_SERIAL => {
                st.presented = Some((monotonic_ns(), p.time_ns, p.seq));
            }
            ServerMsg::Presented(p) if p.serial == COMMIT_SERIAL => {
                st.committed = Some(monotonic_ns());
            }
            ServerMsg::BufferReleased(r) if r.id == BUFFER => {
                st.released = true;
                println!(
                    "BufferReleased (+{:.1} ms)",
                    t0.elapsed().as_secs_f64() * 1e3
                );
            }
            ServerMsg::SurfaceHint(h) if h.id == SURFACE => println!(
                "SurfaceHint {} {}x{}",
                fourcc_name(h.format),
                h.width,
                h.height
            ),
            ServerMsg::Closed(c) if c.window == WINDOW => st.closed = true,
            ServerMsg::Error(e) => {
                return Err(format!("server error {:?}: {}", e.code, e.msg).into());
            }
            _ => {}
        }
    }
    Ok(())
}

fn pump(conn: &mut Connection, st: &mut State, t0: Instant, timeout: Duration) -> Res<()> {
    poll_conn(conn, timeout)?;
    let mut msgs = Vec::new();
    match conn.poll(&mut msgs) {
        Ok(_) => {}
        Err(nitro_wire::Error::Closed) => {
            handle(&msgs, st, t0)?;
            return Err("server closed the connection".into());
        }
        Err(e) => return Err(e.into()),
    }
    handle(&msgs, st, t0)
}

fn print_stats() {
    match nitro_demo::control::stats() {
        Ok(s) => {
            for (k, v) in s.iter().filter(|(k, _)| {
                k.starts_with("dmabuf_")
                    || k.starts_with("fence")
                    || k.starts_with("implicit_fence")
                    || k.starts_with("scanout_")
            }) {
                println!("stats {k} {v}");
            }
        }
        Err(e) => println!("stats: unavailable ({e})"),
    }
}

fn connect(t0: Instant) -> Res<(Connection, State)> {
    let mut conn = Connection::connect_default("dmabuf_import")?;
    println!(
        "connected to {:?} caps={:#x} ({:.2} ms)",
        conn.server_name(),
        conn.caps(),
        t0.elapsed().as_secs_f64() * 1e3
    );
    let need = caps::DMABUF | caps::SURFACE | caps::RELEASE;
    if !conn.has_caps(need) {
        return Err(format!(
            "server lacks caps {:#x} (has {:#x})",
            need & !conn.caps(),
            conn.caps()
        )
        .into());
    }
    conn.client_caps(need)?;
    flush(&mut conn)?;
    let mut st = State::default();
    // The default feedback is the answer to ClientCaps.
    let deadline = Instant::now() + Duration::from_secs(2);
    while st.feedbacks == 0 && Instant::now() < deadline {
        pump(&mut conn, &mut st, t0, Duration::from_millis(100))?;
    }
    if st.feedbacks == 0 {
        println!("no DmabufFeedback within 2 s");
    }
    Ok((conn, st))
}

fn present(
    conn: &mut Connection,
    st: &mut State,
    opts: &Opts,
    buf: &Imported,
    t0: Instant,
) -> Res<()> {
    let (w, h) = opts.size;
    println!(
        "import: {} {} {}x{} planes {:?}",
        fourcc_name(opts.format),
        nitro_kms::modifier_name(opts.modifier),
        w,
        h,
        buf.planes
            .iter()
            .map(|(_, o, s)| (*o, *s))
            .collect::<Vec<_>>()
    );
    let planes = buf
        .planes
        .iter()
        .map(|(fd, offset, stride)| {
            Ok(DmabufPlane {
                fd: rustix::io::dup(fd)?,
                offset: *offset,
                stride: *stride,
            })
        })
        .collect::<Result<Vec<_>, rustix::io::Errno>>()?;
    let size = Size::new(w as f32, h as f32);
    let t_send = monotonic_ns();
    let t_send_i = Instant::now();
    conn.create_dmabuf_buffer(CreateDmabufBuffer {
        id: BUFFER,
        width: w,
        height: h,
        format: opts.format,
        modifier: opts.modifier,
        planes,
    })?;
    conn.tx()
        .create_window(WINDOW, "dmabuf_import", size, Layer::Normal)
        .create_surface(SURFACE, WINDOW, Rect::new(0.0, 0.0, size.w, size.h))
        .commit(COMMIT_SERIAL)?;
    let full = IRect::new(0, 0, w.cast_signed(), h.cast_signed());
    conn.present_surface(PresentSurface {
        id: SURFACE,
        buffer: BUFFER,
        serial: PRESENT_SERIAL,
        src: full,
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage: vec![full],
    })?;
    flush(conn)?;
    let send_us = t_send_i.elapsed().as_secs_f64() * 1e6;

    let deadline = Instant::now() + Duration::from_secs(5);
    while st.presented.is_none() && Instant::now() < deadline && !st.closed {
        pump(conn, st, t0, Duration::from_millis(50))?;
    }
    println!("timing: encode+send CreateDmabufBuffer+Commit+PresentSurface {send_us:.0} us");
    if let Some(c) = st.committed {
        println!(
            "timing: send → Presented(commit)  {:.2} ms",
            (c.saturating_sub(t_send)) as f64 / 1e6
        );
    }
    match st.presented {
        Some((recv, t_flip, seq)) => println!(
            "timing: send → Presented(surface) {:.2} ms (flip at +{:.2} ms, seq {seq}, \
             event latency {:.2} ms)",
            recv.saturating_sub(t_send) as f64 / 1e6,
            t_flip.saturating_sub(t_send) as f64 / 1e6,
            recv.saturating_sub(t_flip) as f64 / 1e6
        ),
        None => println!("no Presented for the surface frame within 5 s"),
    }
    Ok(())
}

fn run() -> Res<()> {
    let opts = parse_args()?;
    let (w, h) = opts.size;
    let buf = match &opts.source {
        Source::Fd { fd, planes } => source_fd(*fd, planes)?,
        Source::Linear => source_linear(w, h)?,
        Source::Memfd => source_memfd(w, h)?,
    };

    let t0 = Instant::now();
    let (mut conn, mut st) = connect(t0)?;

    present(&mut conn, &mut st, &opts, &buf, t0)?;

    print_stats();

    println!("holding {} s", opts.hold);
    let end = Instant::now() + Duration::from_secs(opts.hold);
    while Instant::now() < end && !st.closed {
        let left = end.saturating_duration_since(Instant::now());
        pump(&mut conn, &mut st, t0, left.min(Duration::from_millis(250)))?;
    }
    print_stats();
    println!(
        "done: feedbacks={} presented={} released={}",
        st.feedbacks,
        st.presented.is_some(),
        st.released
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dmabuf_import: {e}");
        std::process::exit(1);
    }
}
