//! `--video`: a video test client for `Surface` nodes (#3897).
//!
//! One window whose content is a single `Surface` node, fed by a ring of
//! [`RING`] NV12 buffers in sealed memfds that the client maps writable
//! and renders into directly. Each frame is SMPTE-style 75% colour bars
//! (BT.709, limited range), a moving white box and a 16-bit frame counter
//! drawn as luma blocks, sent with `PresentSurface` — outside any
//! transaction, latched by the server at its next vblank.
//!
//! # The buffer ring
//!
//! A buffer is written only after the server said `BufferReleased` for it;
//! if no buffer is free when the timer fires, the frame is **skipped**
//! (the client's fault, counted separately). A frame the server
//! superseded before it reached the screen comes back `BufferReleased`
//! without a `Presented` and is counted **dropped** (the server's
//! newest-wins latch at work). Every other frame gets `Presented{serial}`.
//!
//! # Damage
//!
//! Each ring buffer is two frames behind the one on screen, so its damage
//! is not simply "old box ∪ new box". The ring keeps, per buffer, where
//! the box was when that buffer was last drawn, and a frame's damage is
//! that box ∪ the box of the last frame sent ∪ the new box ∪ the counter:
//! correct whether the server reads damage relative to the buffer's own
//! previous content or to the frame it replaces. A buffer's first use is
//! drawn and damaged in full.
//!
//! # Overlay
//!
//! Controls are ordinary nodes above the surface: a translucent bar along
//! the bottom, a progress rect (a 60-second "clip", advanced by a
//! transaction once a second) and — when the server has fonts,
//! `caps::TEXT` — a label with the rate and the drop count.

use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Point, Rect, Size};
use nitro_shm::{DmaBufMapping, MappingMut, SyncAccess, sync_end, sync_start};
use nitro_wire::Error as WireError;
use nitro_wire::client::Connection;
use nitro_wire::msg::{
    AllocSurfaceBuffers, CreateSurfaceBuffer, PresentSurface, ServerMsg, SurfaceBufferAllocated,
};
use nitro_wire::types::{
    BufferId, ButtonState, ColorMatrix, ColorRange, Layer, NodeId, NodeKind, WindowState, caps,
    format,
};
use rustix::event::{PollFd, PollFlags};
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

use crate::app::{Error, keys, wait};
use crate::args::VideoOpts;

/// The window's root node.
pub const WINDOW: NodeId = NodeId(1);
/// The `Surface` node filling the window.
pub const SURFACE: NodeId = NodeId(2);
/// The translucent control bar along the bottom.
pub const BAR: NodeId = NodeId(3);
/// The progress rect inside the bar.
pub const PROGRESS: NodeId = NodeId(4);
/// The rate/drop label (only with `caps::TEXT`).
pub const LABEL: NodeId = NodeId(5);

/// Buffers in the ring: one on screen, one latched, one being drawn.
pub const RING: usize = 3;
/// Height of the control bar, logical pixels.
pub const BAR_H: f32 = 40.0;
/// Length of the pretend clip the progress rect runs over.
const CLIP_SECONDS: f64 = 60.0;
/// Bits in the frame counter.
const COUNTER_BITS: i32 = 16;

// ------------------------------------------------------------ colour

/// One 8-bit `Y'CbCr` sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Yuv {
    /// Luma.
    pub y: u8,
    /// Cb.
    pub u: u8,
    /// Cr.
    pub v: u8,
}

/// Limited-range black.
pub const BLACK: Yuv = Yuv {
    y: 16,
    u: 128,
    v: 128,
};
/// Limited-range 100% white (the box, and a counter bit that is set).
pub const WHITE: Yuv = Yuv {
    y: 235,
    u: 128,
    v: 128,
};
/// A dark grey: a counter bit that is clear, distinct from the black
/// background so the counter's extent is visible.
pub const GREY: Yuv = Yuv {
    y: 64,
    u: 128,
    v: 128,
};

/// The seven 75% bars, left to right: white, yellow, cyan, green,
/// magenta, red, blue — as non-linear R'G'B' in `0..=1`.
pub const BARS: [[f32; 3]; 7] = [
    [0.75, 0.75, 0.75],
    [0.75, 0.75, 0.0],
    [0.0, 0.75, 0.75],
    [0.0, 0.75, 0.0],
    [0.75, 0.0, 0.75],
    [0.75, 0.0, 0.0],
    [0.0, 0.0, 0.75],
];

fn quantize(x: f32) -> u8 {
    x.round().clamp(0.0, 255.0) as u8
}

/// `R'G'B'` in `0..=1` → BT.709 limited-range `Y'CbCr`.
#[must_use]
pub fn yuv709(r: f32, g: f32, b: f32) -> Yuv {
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - luma) / 1.8556;
    let cr = (r - luma) / 1.5748;
    Yuv {
        y: quantize(16.0 + 219.0 * luma),
        u: quantize(128.0 + 224.0 * cb),
        v: quantize(128.0 + 224.0 * cr),
    }
}

/// BT.709 limited-range `Y'CbCr` → 8-bit `R'G'B'`: the inverse of
/// [`yuv709`], for checking what a correct server must show.
#[must_use]
pub fn rgb709(c: Yuv) -> [u8; 3] {
    let luma = (f32::from(c.y) - 16.0) / 219.0;
    let cb = (f32::from(c.u) - 128.0) / 224.0;
    let cr = (f32::from(c.v) - 128.0) / 224.0;
    let r = luma + 1.5748 * cr;
    let b = luma + 1.8556 * cb;
    let g = (luma - 0.2126 * r - 0.0722 * b) / 0.7152;
    [
        quantize(r * 255.0),
        quantize(g * 255.0),
        quantize(b * 255.0),
    ]
}

/// Bar `i` as `Y'CbCr`.
#[must_use]
pub fn bar_yuv(i: usize) -> Yuv {
    let [r, g, b] = BARS[i];
    yuv709(r, g, b)
}

/// Bar `i` as the 8-bit RGB a correct server shows (0 or 191 per channel).
#[must_use]
pub fn bar_rgb(i: usize) -> [u8; 3] {
    BARS[i].map(|c| quantize(c * 255.0))
}

// ------------------------------------------------------------ frames

/// The geometry of one buffer: format, size and the per-plane offsets and
/// strides. A client-made (memfd) buffer is tight; a server-allocated
/// scanout buffer (#3914) has the kernel's padded pitch. Width is even
/// for the YUV formats, and height too for NV12.
///
/// - `NV12`: luma plane at `offset0`, interleaved `[Cb, Cr]` at `offset1`,
///   half resolution both ways;
/// - `YUYV`: packed `Y0 Cb Y1 Cr`, two pixels per four bytes;
/// - `XR24`: `B G R X` bytes; the pattern's colours converted to RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// DRM fourcc: `NV12`, `YUYV` or `XR24`.
    pub fourcc: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Byte offset of plane 0.
    pub offset0: u32,
    /// Row stride of plane 0.
    pub stride0: u32,
    /// Byte offset of plane 1 (NV12 chroma; 0 otherwise).
    pub offset1: u32,
    /// Row stride of plane 1 (NV12 chroma; 0 otherwise).
    pub stride1: u32,
}

impl Frame {
    /// A tight NV12 frame: stride = width, chroma right after luma.
    #[must_use]
    pub fn nv12(width: u32, height: u32) -> Self {
        Self::tight(format::NV12, width, height)
    }

    /// A tight frame of `fourcc` (`NV12`, `YUYV`, anything else is XR24).
    #[must_use]
    pub fn tight(fourcc: u32, width: u32, height: u32) -> Self {
        match fourcc {
            format::NV12 => Self {
                fourcc,
                width,
                height,
                offset0: 0,
                stride0: width,
                offset1: width * height,
                stride1: width,
            },
            format::YUYV => Self {
                fourcc,
                width,
                height,
                offset0: 0,
                stride0: 2 * width,
                offset1: 0,
                stride1: 0,
            },
            _ => Self {
                fourcc: format::XR24,
                width,
                height,
                offset0: 0,
                stride0: 4 * width,
                offset1: 0,
                stride1: 0,
            },
        }
    }

    /// Bytes the planes reach (what a tight buffer allocates).
    #[must_use]
    pub fn bytes(self) -> usize {
        let (h, o0, s0, o1, s1) = (
            self.height as usize,
            self.offset0 as usize,
            self.stride0 as usize,
            self.offset1 as usize,
            self.stride1 as usize,
        );
        if self.fourcc == format::NV12 {
            (o0 + s0 * h).max(o1 + s1 * h.div_ceil(2))
        } else {
            o0 + s0 * h
        }
    }

    /// The whole frame as a rect.
    #[must_use]
    pub fn full(self) -> IRect {
        IRect::new(0, 0, self.width.cast_signed(), self.height.cast_signed())
    }

    /// Fill `r` (clipped to the frame, widened to even edges) with `c`.
    pub fn fill(self, buf: &mut [u8], rect: IRect, color: Yuv) {
        let rect = rect.intersect(&self.full());
        if rect.is_empty() {
            return;
        }
        let (width, height) = (self.width as usize, self.height as usize);
        let x0 = (rect.x & !1) as usize;
        let y0 = (rect.y & !1) as usize;
        let x1 = (((rect.right() + 1) & !1) as usize).min(width);
        let y1 = (((rect.bottom() + 1) & !1) as usize).min(height);
        let (o0, s0) = (self.offset0 as usize, self.stride0 as usize);
        match self.fourcc {
            format::NV12 => {
                for row in y0..y1 {
                    buf[o0 + row * s0 + x0..o0 + row * s0 + x1].fill(color.y);
                }
                let (o1, s1) = (self.offset1 as usize, self.stride1 as usize);
                for crow in y0 / 2..y1 / 2 {
                    let start = o1 + crow * s1;
                    for px in buf[start + x0..start + x1].chunks_exact_mut(2) {
                        px[0] = color.u;
                        px[1] = color.v;
                    }
                }
            }
            format::YUYV => {
                let quad = [color.y, color.u, color.y, color.v];
                for row in y0..y1 {
                    let start = o0 + row * s0;
                    for px in buf[start + 2 * x0..start + 2 * x1].chunks_exact_mut(4) {
                        px.copy_from_slice(&quad);
                    }
                }
            }
            _ => {
                let [r, g, b] = rgb709(color);
                let px4 = [b, g, r, 0xff];
                for row in y0..y1 {
                    let start = o0 + row * s0;
                    for px in buf[start + 4 * x0..start + 4 * x1].chunks_exact_mut(4) {
                        px.copy_from_slice(&px4);
                    }
                }
            }
        }
    }

    /// The sample at `(x, y)` (for XR24: its RGB converted back).
    #[must_use]
    pub fn pixel(self, buf: &[u8], x: u32, y: u32) -> Yuv {
        let (x, y) = (x as usize, y as usize);
        let (o0, s0) = (self.offset0 as usize, self.stride0 as usize);
        match self.fourcc {
            format::NV12 => {
                let c = self.offset1 as usize + (y / 2) * self.stride1 as usize + (x & !1);
                Yuv {
                    y: buf[o0 + y * s0 + x],
                    u: buf[c],
                    v: buf[c + 1],
                }
            }
            format::YUYV => {
                let q = o0 + y * s0 + (x & !1) * 2;
                Yuv {
                    y: buf[q + (x & 1) * 2],
                    u: buf[q + 1],
                    v: buf[q + 3],
                }
            }
            _ => {
                let p = o0 + y * s0 + 4 * x;
                let c = |i: usize| f32::from(buf[p + i]) / 255.0;
                yuv709(c(2), c(1), c(0))
            }
        }
    }
}

// ------------------------------------------------------------ layout

/// Where everything in a frame goes. All edges are even, so no fill
/// straddles a chroma sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The buffer.
    pub fmt: Frame,
    /// Height of the bar band at the top.
    pub bars_h: i32,
    /// Edge of one counter block.
    pub blk: i32,
    /// Top of the moving box.
    pub box_y: i32,
    /// Edge of the moving box.
    pub box_edge: i32,
    /// How far the box moves per frame (bounces every ~2 s).
    pub step: i32,
}

impl Layout {
    /// The layout for a `fmt` buffer at `fps`.
    #[must_use]
    pub fn new(fmt: Frame, fps: u32) -> Self {
        let (w, h) = (fmt.width.cast_signed(), fmt.height.cast_signed());
        let bars_h = (h * 2 / 3) & !1;
        let blk = (h / 48).max(2) & !1;
        let box_y = bars_h + 3 * blk;
        let box_edge = (h - box_y - blk).min(w).max(2) & !1;
        let travel = (w - box_edge).max(0);
        let step = (travel / (fps.max(1).cast_signed() * 2)).max(2) & !1;
        Self {
            fmt,
            bars_h,
            blk,
            box_y,
            box_edge,
            step,
        }
    }

    /// Bar `i`'s rect (0..7).
    #[must_use]
    pub fn bar(&self, i: usize) -> IRect {
        let w = self.fmt.width.cast_signed();
        let x = |i: i32| if i == 7 { w } else { (w * i / 7) & !1 };
        let i = i32::try_from(i).unwrap_or(7);
        let (x0, x1) = (x(i), x(i + 1));
        IRect::new(x0, 0, x1 - x0, self.bars_h)
    }

    /// The counter's rect.
    #[must_use]
    pub fn counter(&self) -> IRect {
        IRect::new(
            self.blk,
            self.bars_h + self.blk,
            (2 * COUNTER_BITS - 1) * self.blk,
            self.blk,
        )
    }

    /// The moving box at frame `frame`: a triangle wave across the width.
    #[must_use]
    pub fn box_rect(&self, frame: u64) -> IRect {
        let travel = u64::from(
            (self.fmt.width.cast_signed() - self.box_edge)
                .max(0)
                .cast_unsigned(),
        );
        let x = if travel == 0 {
            0
        } else {
            let pos = frame.wrapping_mul(u64::from(self.step.cast_unsigned())) % (2 * travel);
            if pos <= travel { pos } else { 2 * travel - pos }
        };
        IRect::new((x as i32) & !1, self.box_y, self.box_edge, self.box_edge)
    }
}

/// Draw frame `frame` from scratch.
pub fn draw_full(buf: &mut [u8], l: &Layout, frame: u64) {
    let fmt = l.fmt;
    for i in 0..BARS.len() {
        fmt.fill(buf, l.bar(i), bar_yuv(i));
    }
    let bottom = IRect::new(
        0,
        l.bars_h,
        fmt.width.cast_signed(),
        fmt.height.cast_signed() - l.bars_h,
    );
    fmt.fill(buf, bottom, BLACK);
    draw_counter(buf, l, frame);
    fmt.fill(buf, l.box_rect(frame), WHITE);
}

/// Bring a buffer that holds some earlier frame, whose box was at
/// `old_box`, up to frame `frame`. Only the box and the counter move, so
/// that is all it touches.
pub fn draw_update(buf: &mut [u8], l: &Layout, old_box: IRect, frame: u64) {
    l.fmt.fill(buf, old_box, BLACK);
    draw_counter(buf, l, frame);
    l.fmt.fill(buf, l.box_rect(frame), WHITE);
}

/// The frame counter, most significant bit first: a set bit is a white
/// block, a clear one grey.
fn draw_counter(buf: &mut [u8], l: &Layout, frame: u64) {
    let c = l.counter();
    l.fmt.fill(buf, c, BLACK);
    for k in 0..COUNTER_BITS {
        let bit = COUNTER_BITS - 1 - k;
        let on = (frame >> bit) & 1 == 1;
        let r = IRect::new(c.x + 2 * k * l.blk, c.y, l.blk, l.blk);
        l.fmt.fill(buf, r, if on { WHITE } else { GREY });
    }
}

/// Read the counter back out of a buffer (the inverse of the drawing,
/// for tests and for eyeballing a dump).
#[must_use]
pub fn read_counter(buf: &[u8], l: &Layout) -> u64 {
    let c = l.counter();
    (0..COUNTER_BITS).fold(0, |acc, k| {
        let x = c.x + 2 * k * l.blk + l.blk / 2;
        let y = c.y + l.blk / 2;
        let on = l.fmt.pixel(buf, x.cast_unsigned(), y.cast_unsigned()).y > 150;
        (acc << 1) | u64::from(on)
    })
}

/// A frame's damage: the union of everything that may differ from what
/// the buffer held and from what the screen shows, clipped, without
/// empties or duplicates.
#[must_use]
pub fn damage(l: &Layout, new_box: IRect, own_old: IRect, last_sent: Option<IRect>) -> Vec<IRect> {
    let full = l.fmt.full();
    let mut out: Vec<IRect> = Vec::with_capacity(4);
    for r in [Some(new_box), Some(own_old), last_sent, Some(l.counter())]
        .into_iter()
        .flatten()
    {
        let r = r.intersect(&full);
        if !r.is_empty() && !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

// ------------------------------------------------------------ overlay geometry

/// The control bar in a window of `size`.
#[must_use]
pub fn bar_rect(size: Size) -> Rect {
    Rect::new(0.0, (size.h - BAR_H).max(0.0), size.w, BAR_H.min(size.h))
}

/// The progress rect: a strip along the top of the bar, `p` of the way.
#[must_use]
pub fn progress_rect(size: Size, p: f32) -> Rect {
    let bar = bar_rect(size);
    Rect::new(bar.x, bar.y, (size.w * p.clamp(0.0, 1.0)).max(1.0), 4.0)
}

/// Where the label goes.
#[must_use]
pub fn label_rect(size: Size) -> Rect {
    let bar = bar_rect(size);
    Rect::new(12.0, bar.y + 12.0, (size.w - 24.0).max(1.0), 22.0)
}

// ------------------------------------------------------------ the client

/// Where a ring buffer's pixels live.
#[derive(Debug)]
enum Store {
    /// The client's own sealed memfd (`CreateSurfaceBuffer`).
    Memfd(MappingMut),
    /// A server-allocated scanout buffer (`AllocSurfaceBuffers`, #3914):
    /// the mapping, and the dma-buf fd kept for the sync bracket.
    DmaBuf {
        /// The client's read/write mapping.
        map: DmaBufMapping,
        /// The dma-buf (a memfd on the fake backend).
        fd: OwnedFd,
    },
}

impl Store {
    /// Run `draw` over the pixels, bracketed by `DMA_BUF_IOCTL_SYNC` for a
    /// dma-buf (a no-op answer on the fake's memfd).
    fn write(&mut self, draw: impl FnOnce(&mut [u8])) -> Result<(), Error> {
        match self {
            Self::Memfd(m) => draw(m.as_bytes_mut()),
            Self::DmaBuf { map, fd } => {
                sync_start(&*fd, SyncAccess::Write)?;
                draw(map.as_bytes_mut());
                sync_end(&*fd, SyncAccess::Write)?;
            }
        }
        Ok(())
    }
}

/// One ring buffer.
#[derive(Debug)]
struct Slot {
    id: BufferId,
    map: Store,
    /// Waiting for `BufferReleased`.
    busy: bool,
    /// The serial of the frame it is carrying, until released.
    inflight: Option<u32>,
    /// Whether that frame got `Presented`.
    shown: bool,
    /// Where the box was when this buffer was last drawn; `None` = never.
    drawn: Option<IRect>,
}

/// The video client.
pub struct Video {
    /// The connection.
    pub conn: Connection,
    /// The command line's video options.
    pub opts: VideoOpts,
    timer: OwnedFd,
    ring: Vec<Slot>,
    /// Buffers of a ring replaced after a `SurfaceHint`, still held by the
    /// server; destroyed as each is released.
    retired: Vec<Slot>,
    generation: u32,
    /// An `AllocSurfaceBuffers` in flight (`--scanout`): its `first_id`.
    /// No frame is drawn until the whole ring has arrived.
    alloc_pending: Option<BufferId>,
    /// The current ring's layout.
    pub layout: Layout,
    serial: u32,
    /// Content frame number: advances with the timer, not while paused.
    pub frame: u64,
    last_box: Option<IRect>,
    /// `space` toggles.
    pub paused: bool,
    /// What we believe the window state is.
    pub fullscreen: bool,
    /// Set by `q`, `Closed`, a hangup or the frame limit.
    pub done: bool,
    /// The window's size, as last configured.
    pub window_size: Size,
    /// The window's content origin on the output, as last configured.
    pub window_pos: Point,
    /// The last `SurfaceHint`: `(format, width, height)`.
    pub hint: Option<(u32, u32, u32)>,
    /// `PresentSurface`s sent.
    pub sent: u64,
    /// Frames that got `Presented`.
    pub presented: u64,
    /// Frames released without `Presented` (superseded at the latch).
    pub dropped: u64,
    /// Timer ticks with no free buffer.
    pub skipped: u64,
    /// When the client connected.
    pub started: Instant,
    last_overlay: Instant,
    events: Vec<ServerMsg>,
}

impl Video {
    /// Connect to the default socket and start.
    ///
    /// # Errors
    /// As [`Video::with_connection`], or a connect failure.
    pub fn start(opts: VideoOpts) -> Result<Self, Error> {
        let conn = Connection::connect_default("nitro-demo")?;
        Self::with_connection(conn, opts)
    }

    /// Start over a connection the caller made: check the caps, opt in to
    /// the messages, build the window and its ring, and arm the timer.
    ///
    /// # Errors
    /// [`Error::Unsupported`] if the server lacks `SURFACE` or `RELEASE`,
    /// otherwise any memfd, mapping, timer or socket failure.
    pub fn with_connection(mut conn: Connection, opts: VideoOpts) -> Result<Self, Error> {
        if !conn.has_caps(caps::SURFACE) {
            return Err(Error::Unsupported(
                "the server does not advertise caps::SURFACE",
            ));
        }
        if !conn.has_caps(caps::RELEASE) {
            return Err(Error::Unsupported(
                "the server does not advertise caps::RELEASE (a remote link cannot carry buffers)",
            ));
        }
        conn.client_caps(caps::RELEASE | caps::SURFACE)?;
        let timer = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )?;
        let period = Timespec {
            tv_sec: 0,
            tv_nsec: i64::from(1_000_000_000 / opts.fps.max(1)),
        };
        timerfd_settime(
            &timer,
            TimerfdTimerFlags::empty(),
            &Itimerspec {
                it_interval: period,
                it_value: period,
            },
        )?;

        let fmt = Frame::tight(
            if opts.format == 0 {
                format::NV12
            } else {
                opts.format
            },
            opts.size.0,
            opts.size.1,
        );
        let now = Instant::now();
        let mut v = Self {
            conn,
            timer,
            ring: Vec::with_capacity(RING),
            retired: Vec::new(),
            generation: 0,
            alloc_pending: None,
            layout: Layout::new(fmt, opts.fps),
            serial: 0,
            frame: 0,
            last_box: None,
            paused: false,
            fullscreen: opts.fullscreen,
            done: false,
            window_size: Size::new(fmt.width as f32, fmt.height as f32),
            window_pos: Point::ZERO,
            hint: None,
            sent: 0,
            presented: 0,
            dropped: 0,
            skipped: 0,
            started: now,
            last_overlay: now,
            events: Vec::new(),
            opts,
        };
        v.build()?;
        v.flush()?;
        Ok(v)
    }

    /// Whether the ring is server-allocated scanout buffers (#3914).
    #[must_use]
    pub fn scanout(&self) -> bool {
        self.ring
            .first()
            .is_some_and(|s| matches!(s.map, Store::DmaBuf { .. }))
    }

    /// An `AllocSurfaceBuffersFailed`: the tight memfd frame to fall back
    /// to, at the size asked for, NV12 unless `--format` said otherwise.
    fn fallback_frame(&mut self) -> Frame {
        self.alloc_pending = None;
        self.ring.clear();
        let fmt = self.layout.fmt;
        let fourcc = if self.opts.format == 0 {
            format::NV12
        } else {
            self.opts.format
        };
        Frame::tight(fourcc, fmt.width, fmt.height)
    }

    /// Server name from the handshake.
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.conn.server_name()
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    /// The window, its surface, the first ring and the overlay, as one
    /// transaction.
    fn build(&mut self) -> Result<(), Error> {
        // `--scanout`: the ring comes from the server after the commit
        // that creates the node; the memfd ring is the fallback.
        let bufs = if self.opts.scanout {
            Vec::new()
        } else {
            self.alloc_ring(self.layout.fmt)?
        };
        let size = self.window_size;
        let label = self.label();
        let has_text = self.conn.has_caps(caps::TEXT);
        let fullscreen = self.opts.fullscreen;
        let serial = self.next_serial();
        let mut tx = self
            .conn
            .tx()
            .create_window(WINDOW, "nitro-demo video", size, Layer::Normal)
            .create_surface(SURFACE, WINDOW, Rect::new(0.0, 0.0, size.w, size.h));
        for b in bufs {
            tx = tx.create_surface_buffer(b);
        }
        tx = tx
            .create_rect(BAR, WINDOW, bar_rect(size))
            .fill_solid(BAR, Color::rgba(0, 0, 0, 0x99))
            .create_rect(PROGRESS, WINDOW, progress_rect(size, 0.0))
            .fill_solid(PROGRESS, Color::rgb(0xe0, 0x3b, 0x3b));
        if has_text {
            tx = tx
                .create_node(LABEL, NodeKind::Text, WINDOW)
                .bounds(LABEL, label_rect(size))
                .set_text(LABEL, "sans", 16.0, Color::WHITE, &label);
        }
        if self.opts.no_controls {
            tx = tx.visible(BAR, false).visible(PROGRESS, false);
            if has_text {
                tx = tx.visible(LABEL, false);
            }
        }
        if fullscreen {
            tx = tx.set_window_state(WINDOW, WindowState::Fullscreen);
        }
        tx.commit(serial)?;
        if self.opts.scanout {
            let (w, h) = (self.layout.fmt.width, self.layout.fmt.height);
            self.request_scanout(self.opts.format, w, h)?;
        }
        Ok(())
    }

    /// Ask the server for a ring of [`RING`] scanout buffers (#3914).
    /// `format` 0 is the server's choice.
    fn request_scanout(&mut self, format: u32, width: u32, height: u32) -> Result<(), Error> {
        let first = BufferId(self.generation * RING as u32 + 1);
        self.generation += 1;
        self.alloc_pending = Some(first);
        self.conn.alloc_surface_buffers(AllocSurfaceBuffers {
            node: SURFACE,
            first_id: first,
            count: RING as u8,
            format,
            width,
            height,
        })?;
        Ok(())
    }

    /// One `SurfaceBufferAllocated`: map it and add it to the ring. The
    /// last one of the ring switches the layout to the server's geometry.
    fn take_scanout(&mut self, a: &SurfaceBufferAllocated) -> Result<(), Error> {
        let fd = rustix::io::dup(&a.fd)?;
        let map = DmaBufMapping::map(fd.as_fd(), a.size as usize)?;
        self.ring.push(Slot {
            id: a.id,
            map: Store::DmaBuf { map, fd },
            busy: false,
            inflight: None,
            shown: false,
            drawn: None,
        });
        if self.ring.len() == RING {
            self.alloc_pending = None;
            self.layout = Layout::new(
                Frame {
                    fourcc: a.format,
                    width: a.width,
                    height: a.height,
                    offset0: a.offset0,
                    stride0: a.stride0,
                    offset1: a.offset1,
                    stride1: a.stride1,
                },
                self.opts.fps,
            );
            self.last_box = None;
        }
        Ok(())
    }

    /// A fresh ring of [`RING`] buffers for `fmt`, mapped, with the
    /// messages that register them.
    fn alloc_ring(&mut self, fmt: Frame) -> Result<Vec<CreateSurfaceBuffer>, Error> {
        let base = self.generation * RING as u32;
        self.generation += 1;
        let len = fmt.bytes();
        let mut out = Vec::with_capacity(RING);
        for i in 0..RING {
            let fd = nitro_shm::create_sealed("nitro-demo-video", len as u64)?;
            let map = MappingMut::map_mut(fd.as_fd(), len)?;
            let id = BufferId(base + i as u32 + 1);
            out.push(CreateSurfaceBuffer {
                id,
                width: fmt.width,
                height: fmt.height,
                format: fmt.fourcc,
                size: len as u32,
                offset0: fmt.offset0,
                stride0: fmt.stride0,
                offset1: fmt.offset1,
                stride1: fmt.stride1,
                fd,
            });
            self.ring.push(Slot {
                id,
                map: Store::Memfd(map),
                busy: false,
                inflight: None,
                shown: false,
                drawn: None,
            });
        }
        Ok(out)
    }

    /// The label text.
    #[must_use]
    pub fn label(&self) -> String {
        if self.paused {
            format!("❚❚ paused / dropped {}", self.dropped)
        } else {
            format!("▶ {} fps / dropped {}", self.opts.fps, self.dropped)
        }
    }

    /// How far through the pretend clip the content is, `0..1`.
    #[must_use]
    pub fn progress(&self) -> f32 {
        let secs = self.frame as f64 / f64::from(self.opts.fps.max(1));
        ((secs % CLIP_SECONDS) / CLIP_SECONDS) as f32
    }

    /// Wait up to `timeout` for the connection, the frame timer or
    /// `extra` (the binary's SIGINT pipe), and handle whatever is ready.
    /// Returns whether `extra` is readable.
    ///
    /// # Errors
    /// Socket, encode, timer or server `Error`.
    pub fn step(
        &mut self,
        timeout: Option<Duration>,
        extra: Option<BorrowedFd<'_>>,
    ) -> Result<bool, Error> {
        let (conn_ready, timer_ready, extra_ready) = {
            let conn_fd = self.conn.as_fd();
            let timer_fd = self.timer.as_fd();
            let mut fds = vec![
                PollFd::new(&conn_fd, PollFlags::IN),
                PollFd::new(&timer_fd, PollFlags::IN),
            ];
            if let Some(e) = &extra {
                fds.push(PollFd::new(e, PollFlags::IN));
            }
            let ts = timeout.map(|d| Timespec {
                tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(d.subsec_nanos()),
            });
            match rustix::event::poll(&mut fds, ts.as_ref()) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            let ready = |i: usize| {
                fds.get(i)
                    .is_some_and(|f| f.revents().intersects(PollFlags::IN | PollFlags::HUP))
            };
            (ready(0), ready(1), ready(2))
        };
        if conn_ready {
            self.read()?;
        }
        if timer_ready && !self.done {
            self.on_timer()?;
        }
        self.flush()?;
        Ok(extra_ready)
    }

    /// Drain the socket and handle what came.
    fn read(&mut self) -> Result<(), Error> {
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        let r = match self.conn.poll(&mut events) {
            Ok(_) => self.handle(&events),
            Err(WireError::Closed) => {
                self.done = true;
                Ok(())
            }
            Err(e) => Err(e.into()),
        };
        self.events = events;
        r
    }

    /// Push every queued byte.
    fn flush(&mut self) -> Result<(), Error> {
        while !self.conn.flush()? {
            wait(self.conn.as_fd(), PollFlags::OUT, None)?;
        }
        Ok(())
    }

    /// The frame timer fired: advance, present, and refresh the overlay
    /// once a second.
    fn on_timer(&mut self) -> Result<(), Error> {
        let mut buf = [0u8; 8];
        let ticks = match rustix::io::read(&self.timer, &mut buf[..]) {
            Ok(8) => u64::from_ne_bytes(buf),
            Ok(_) | Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if !self.paused {
            // One draw per wakeup however many periods passed: the counter
            // shows the timeline, so a late wakeup is visible as a jump.
            self.frame += ticks;
            self.present()?;
        }
        if self.last_overlay.elapsed() >= Duration::from_secs(1) {
            self.overlay()?;
        }
        Ok(())
    }

    /// Draw the current frame into a free buffer and send it, or count a
    /// skip.
    fn present(&mut self) -> Result<(), Error> {
        if self.alloc_pending.is_some() {
            // The scanout ring has not arrived yet.
            return Ok(());
        }
        let Some(i) = self.ring.iter().position(|s| !s.busy) else {
            self.skipped += 1;
            return Ok(());
        };
        let serial = self.next_serial();
        let (l, frame, last) = (self.layout, self.frame, self.last_box);
        let slot = &mut self.ring[i];
        let new_box = l.box_rect(frame);
        let damage = match slot.drawn {
            None => {
                slot.map.write(|b| draw_full(b, &l, frame))?;
                vec![l.fmt.full()]
            }
            Some(old) => {
                slot.map.write(|b| draw_update(b, &l, old, frame))?;
                damage(&l, new_box, old, last)
            }
        };
        slot.drawn = Some(new_box);
        slot.busy = true;
        slot.shown = false;
        slot.inflight = Some(serial);
        let buffer = slot.id;
        self.last_box = Some(new_box);
        self.conn.present_surface(PresentSurface {
            id: SURFACE,
            buffer,
            serial,
            src: l.fmt.full(),
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            damage,
        })?;
        self.sent += 1;
        Ok(())
    }

    /// Progress and label, in one transaction.
    fn overlay(&mut self) -> Result<(), Error> {
        self.last_overlay = Instant::now();
        let size = self.window_size;
        let p = self.progress();
        let label = self.label();
        let has_text = self.conn.has_caps(caps::TEXT);
        let serial = self.next_serial();
        let mut tx = self.conn.tx().bounds(PROGRESS, progress_rect(size, p));
        if has_text {
            tx = tx.set_text(LABEL, "sans", 16.0, Color::WHITE, &label);
        }
        tx.commit(serial)?;
        Ok(())
    }

    /// Handle one batch of server messages.
    ///
    /// # Errors
    /// A server `Error`, or an encode/memfd failure answering it.
    pub fn handle(&mut self, events: &[ServerMsg]) -> Result<(), Error> {
        let mut relayout = false;
        let mut realloc = None;
        let mut fallback = None;
        let mut destroy = Vec::new();
        for msg in events {
            match msg {
                ServerMsg::Presented(p) => {
                    let hit = self
                        .ring
                        .iter_mut()
                        .chain(self.retired.iter_mut())
                        .find(|s| s.inflight == Some(p.serial))
                        .map(|s| s.shown = true)
                        .is_some();
                    if hit {
                        self.presented += 1;
                        if self.opts.frames > 0 && self.presented >= self.opts.frames {
                            self.done = true;
                        }
                    }
                }
                ServerMsg::BufferReleased(r) => {
                    if let Some(s) = self.ring.iter_mut().find(|s| s.id == r.id) {
                        if release(s) {
                            self.dropped += 1;
                        }
                    } else if let Some(k) = self.retired.iter().position(|s| s.id == r.id) {
                        let mut s = self.retired.swap_remove(k);
                        if release(&mut s) {
                            self.dropped += 1;
                        }
                        destroy.push(s.id);
                    }
                }
                ServerMsg::Configure(c) if c.window == WINDOW => {
                    self.window_size = c.size;
                    self.window_pos = c.position;
                    relayout = true;
                }
                ServerMsg::WindowState(s) if s.window == WINDOW => {
                    self.fullscreen = s.state == WindowState::Fullscreen;
                }
                ServerMsg::SurfaceHint(h) if h.id == SURFACE => {
                    self.hint = Some((h.format, h.width, h.height));
                    let (w, hh) = ((h.width & !1).max(2), (h.height & !1).max(2));
                    let cur = self.layout.fmt;
                    if self.opts.follow_hint
                        && h.width > 0
                        && h.height > 0
                        && (w, hh) != (cur.width, cur.height)
                        && self.alloc_pending.is_none()
                    {
                        realloc = Some(Frame::tight(cur.fourcc, w, hh));
                    }
                }
                ServerMsg::SurfaceBufferAllocated(a) if a.node == SURFACE => {
                    self.take_scanout(a)?;
                }
                ServerMsg::AllocSurfaceBuffersFailed(f) if f.node == SURFACE => {
                    eprintln!(
                        "nitro-demo: scanout buffers refused ({:?}); using memfds",
                        f.reason
                    );
                    fallback = Some(self.fallback_frame());
                }
                ServerMsg::Closed(c) if c.window == WINDOW => self.done = true,
                ServerMsg::Key(k) if k.state == ButtonState::Pressed => self.key(k.keycode)?,
                ServerMsg::Error(e) => {
                    return Err(Error::Server(format!("{:?}: {}", e.code, e.msg)));
                }
                _ => {}
            }
        }
        self.apply_changes(relayout, realloc, fallback, destroy)
    }

    /// The tail of [`Video::handle`]: retire and replace the ring, fall
    /// back to memfds, re-lay-out — one transaction for all of it.
    fn apply_changes(
        &mut self,
        relayout: bool,
        realloc: Option<Frame>,
        fallback: Option<Frame>,
        mut destroy: Vec<BufferId>,
    ) -> Result<(), Error> {
        let mut bufs = Vec::new();
        if let Some(fmt) = realloc {
            // Retire the old ring: free buffers go now, busy ones when the
            // server releases them (the one on screen, when the first
            // frame of the new ring replaces it).
            let was_scanout = self.scanout();
            for s in std::mem::take(&mut self.ring) {
                if s.busy {
                    self.retired.push(s);
                } else {
                    destroy.push(s.id);
                }
            }
            if was_scanout {
                // A new server-allocated ring at the hinted size, same
                // format; the layout switches when it arrives.
                self.request_scanout(fmt.fourcc, fmt.width, fmt.height)?;
            } else {
                bufs = self.alloc_ring(fmt)?;
                self.layout = Layout::new(fmt, self.opts.fps);
                self.last_box = None;
            }
        }
        if let Some(fmt) = fallback {
            bufs = self.alloc_ring(fmt)?;
            self.layout = Layout::new(fmt, self.opts.fps);
            self.last_box = None;
        }
        if relayout || !bufs.is_empty() || !destroy.is_empty() {
            let size = self.window_size;
            let p = self.progress();
            let has_text = self.conn.has_caps(caps::TEXT);
            let serial = self.next_serial();
            let mut tx = self.conn.tx();
            for id in destroy {
                tx = tx.destroy_buffer(id);
            }
            for b in bufs {
                tx = tx.create_surface_buffer(b);
            }
            if relayout {
                tx = tx
                    .bounds(SURFACE, Rect::new(0.0, 0.0, size.w, size.h))
                    .bounds(BAR, bar_rect(size))
                    .bounds(PROGRESS, progress_rect(size, p));
                if has_text {
                    tx = tx.bounds(LABEL, label_rect(size));
                }
            }
            tx.commit(serial)?;
        }
        Ok(())
    }

    /// Act on a key press, by evdev keycode.
    fn key(&mut self, keycode: u32) -> Result<(), Error> {
        match keycode {
            keys::Q => self.done = true,
            keys::SPACE => {
                self.paused = !self.paused;
                self.overlay()?;
            }
            keys::F => {
                // Optimistic: a refusal sends no event, and a second `f`
                // should then still mean "the other state".
                self.fullscreen = !self.fullscreen;
                let state = if self.fullscreen {
                    WindowState::Fullscreen
                } else {
                    WindowState::Normal
                };
                let serial = self.next_serial();
                self.conn
                    .tx()
                    .set_window_state(WINDOW, state)
                    .commit(serial)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The exit line.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let secs = self.started.elapsed().as_secs_f64().max(1e-9);
        format!(
            "video: sent={} presented={} dropped={} skipped={} over {secs:.1}s ({:.1} presented/s)",
            self.sent,
            self.presented,
            self.dropped,
            self.skipped,
            self.presented as f64 / secs,
        )
    }
}

/// Mark a slot free; returns whether its frame was dropped (released
/// without ever being `Presented`).
fn release(s: &mut Slot) -> bool {
    let dropped = s.inflight.is_some() && !s.shown;
    s.busy = false;
    s.inflight = None;
    s.shown = false;
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32) -> (Layout, Vec<u8>) {
        let fmt = Frame::nv12(w, h);
        (Layout::new(fmt, 60), vec![0; fmt.bytes()])
    }

    /// A frame of `fourcc` with a 64-byte-aligned pitch (a dumb buffer's),
    /// and NV12's chroma after a padded luma plane.
    fn padded(fourcc: u32, w: u32, h: u32) -> (Layout, Vec<u8>) {
        let bpp = match fourcc {
            format::NV12 => 1,
            format::YUYV => 2,
            _ => 4,
        };
        let pitch = (w * bpp).div_ceil(64) * 64;
        let fmt = Frame {
            fourcc,
            width: w,
            height: h,
            offset0: 0,
            stride0: pitch,
            offset1: if fourcc == format::NV12 { pitch * h } else { 0 },
            stride1: if fourcc == format::NV12 { pitch } else { 0 },
        };
        (Layout::new(fmt, 60), vec![0; fmt.bytes()])
    }

    #[test]
    fn every_format_draws_and_reads_back_with_a_padded_stride() {
        for fourcc in [format::NV12, format::YUYV, format::XR24] {
            let (l, mut buf) = padded(fourcc, 650, 360);
            assert!(l.fmt.stride0 > l.fmt.width, "{fourcc:#x} is padded");
            draw_full(&mut buf, &l, 1234);
            assert_eq!(read_counter(&buf, &l), 1234, "{fourcc:#x}");
            for i in 0..7 {
                let r = l.bar(i);
                let p = l.fmt.pixel(&buf, (r.x + r.w / 2) as u32, 100);
                let want = bar_yuv(i);
                if fourcc == format::XR24 {
                    // Through RGB and back: close, not exact.
                    assert!(p.y.abs_diff(want.y) <= 2, "{fourcc:#x} bar {i}");
                } else {
                    assert_eq!(p, want, "{fourcc:#x} bar {i}");
                }
            }
            // The padding is never written.
            let s0 = l.fmt.stride0 as usize;
            let row_bytes = l.fmt.width as usize
                * if fourcc == format::YUYV {
                    2
                } else if fourcc == format::NV12 {
                    1
                } else {
                    4
                };
            assert!(buf[row_bytes..s0].iter().all(|&b| b == 0), "{fourcc:#x}");
            // An update equals a full redraw.
            let (_, mut b) = padded(fourcc, 650, 360);
            draw_full(&mut b, &l, 40);
            draw_full(&mut buf, &l, 3);
            draw_update(&mut buf, &l, l.box_rect(3), 40);
            assert!(buf == b, "{fourcc:#x}: incremental and full frames differ");
        }
    }

    #[test]
    fn a_yuyv_pixel_pair_shares_its_chroma() {
        let (l, mut buf) = padded(format::YUYV, 8, 2);
        l.fmt.fill(&mut buf, IRect::new(2, 0, 2, 2), WHITE);
        assert_eq!(&buf[4..8], &[235, 128, 235, 128]);
        assert_eq!(l.fmt.pixel(&buf, 3, 1), WHITE);
        assert_eq!(l.fmt.pixel(&buf, 0, 0).y, 0);
    }

    #[test]
    fn the_bars_are_the_textbook_bt709_values() {
        // 75% bars, BT.709 limited range, as every test-pattern generator
        // prints them.
        let want = [
            (180, 128, 128),
            (168, 44, 136),
            (145, 147, 44),
            (133, 63, 52),
            (63, 193, 204),
            (51, 109, 212),
            (28, 212, 120),
        ];
        for (i, (y, u, v)) in want.into_iter().enumerate() {
            assert_eq!(bar_yuv(i), Yuv { y, u, v }, "bar {i}");
        }
    }

    #[test]
    fn the_bars_round_trip_to_75_percent_rgb() {
        for i in 0..BARS.len() {
            let got = rgb709(bar_yuv(i));
            let want = bar_rgb(i);
            for c in 0..3 {
                assert!(
                    got[c].abs_diff(want[c]) <= 2,
                    "bar {i}: {got:?} vs {want:?}"
                );
            }
        }
        assert_eq!(bar_rgb(1), [191, 191, 0]);
    }

    #[test]
    fn a_full_frame_has_bars_box_and_counter() {
        let (l, mut buf) = frame(1280, 720);
        draw_full(&mut buf, &l, 5);
        for i in 0..7 {
            let r = l.bar(i);
            let p = l.fmt.pixel(&buf, (r.x + r.w / 2) as u32, 100);
            assert_eq!(p, bar_yuv(i), "bar {i}");
        }
        let b = l.box_rect(5);
        assert_eq!(
            l.fmt
                .pixel(&buf, (b.x + b.w / 2) as u32, (b.y + b.h / 2) as u32),
            WHITE
        );
        assert_eq!(read_counter(&buf, &l), 5);
        // Everything sits inside the frame, on even edges.
        for r in [l.counter(), b] {
            assert!(l.fmt.full().contains_rect(&r), "{r:?}");
            assert_eq!((r.x | r.y | r.w | r.h) & 1, 0, "{r:?}");
        }
    }

    #[test]
    fn an_update_equals_a_full_redraw() {
        let (l, mut a) = frame(640, 360);
        draw_full(&mut a, &l, 3);
        draw_update(&mut a, &l, l.box_rect(3), 40);
        let (_, mut b) = frame(640, 360);
        draw_full(&mut b, &l, 40);
        assert!(a == b, "incremental and full frames differ");
        assert_eq!(read_counter(&a, &l), 40);
    }

    #[test]
    fn the_box_bounces_and_stays_inside() {
        let (l, _) = frame(640, 360);
        let w = 640;
        let mut xs = Vec::new();
        for f in 0..1000 {
            let b = l.box_rect(f);
            assert!(b.x >= 0 && b.right() <= w, "frame {f}: {b:?}");
            xs.push(b.x);
        }
        assert!(xs.contains(&0));
        assert!(xs.iter().any(|&x| x + l.box_edge >= w - l.step));
        assert_ne!(l.box_rect(0), l.box_rect(1), "the box does not move");
    }

    #[test]
    fn damage_covers_both_old_boxes_the_new_one_and_the_counter() {
        let (l, _) = frame(640, 360);
        let (own, sent, new) = (l.box_rect(1), l.box_rect(2), l.box_rect(4));
        let got = damage(&l, new, own, Some(sent));
        for r in [own, sent, new, l.counter()] {
            assert!(got.contains(&r), "{r:?} missing from {got:?}");
        }
        // No duplicates when they coincide.
        assert_eq!(damage(&l, new, new, Some(new)).len(), 2);
    }

    #[test]
    fn a_fill_on_odd_edges_still_writes_whole_chroma_samples() {
        let (l, mut buf) = frame(8, 8);
        l.fmt.fill(&mut buf, IRect::new(1, 1, 3, 3), WHITE);
        assert_eq!(l.fmt.pixel(&buf, 0, 0), WHITE);
        assert_eq!(l.fmt.pixel(&buf, 3, 3), WHITE);
        assert_eq!(l.fmt.pixel(&buf, 4, 4).y, 0);
    }

    #[test]
    fn the_overlay_stays_in_the_window() {
        let s = Size::new(640.0, 360.0);
        assert!((bar_rect(s).y - (360.0 - BAR_H)).abs() < f32::EPSILON);
        assert!((progress_rect(s, 0.5).w - 320.0).abs() < f32::EPSILON);
        assert!(progress_rect(s, 2.0).w <= s.w);
        assert!(label_rect(s).y > bar_rect(s).y);
    }

    #[test]
    fn a_release_without_presented_is_a_drop() {
        let fd = nitro_shm::create_sealed("t", 64).unwrap();
        let mut s = Slot {
            id: BufferId(1),
            map: Store::Memfd(MappingMut::map_mut(fd.as_fd(), 64).unwrap()),
            busy: true,
            inflight: Some(7),
            shown: false,
            drawn: None,
        };
        assert!(release(&mut s));
        assert!(!s.busy);
        s.busy = true;
        s.inflight = Some(8);
        s.shown = true;
        assert!(!release(&mut s));
    }
}
