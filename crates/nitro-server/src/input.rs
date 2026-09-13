//! Input: devices, the event abstraction, and pointer/touch routing.
//!
//! # The `InputSource` seam
//!
//! Everything above this module speaks [`InputEvent`], a small enum with no
//! libinput in it. [`LibinputSource`] produces those from real devices;
//! [`FakeSource`] is a `VecDeque` a test pushes into. That seam is what
//! makes the whole input path — hit-testing, focus, enter/leave
//! bookkeeping, the input-to-photon clock — testable on the fake backend,
//! with no evdev node and no root.
//!
//! # Opening devices
//!
//! libinput is created with `new_from_path`, not `new_with_udev`: the udev
//! backend would pull `libudev` into the tree for something we can do with
//! a `read_dir` of `/dev/input`. Devices are opened through
//! [`nitro_seat::Seat`], which is the only thing in the process allowed to
//! open `/dev/input/*` — that is what makes the server work without root.
//! Hotplug of input devices is M3: the kms uevent socket already carries
//! the notifications, but acting on them means re-scanning the directory
//! and diffing, which is not worth the code before the shell exists.
//!
//! # Pointer routing
//!
//! The pointer has one position in device pixels, clamped to the union of
//! the outputs, and acceleration is whatever libinput applied. Every motion
//! hit-tests the scene: the window under the pointer gets `PointerMotion`
//! with window-local coordinates and the node hit, and a change of window
//! produces `PointerLeave` on the old one and `PointerEnter` on the new.
//! Buttons and axis events go to the window the pointer is over, not to the
//! focused one — the focus follows the click, not the other way round.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use nitro_core::Point;
use nitro_scene::{Hit, OutputId, Scene, WindowKey};
use nitro_wire::types::{AxisSource, ButtonState, TouchPhase};

use crate::{debug, warn};

/// Evdev code of the left mouse button — the one that raises and focuses.
pub const BTN_LEFT: u32 = 0x110;

/// One input event, already free of libinput.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Relative pointer motion in device pixels, acceleration applied.
    PointerMotion {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
        /// `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    },
    /// Absolute pointer motion, already scaled to device pixels.
    PointerAbsolute {
        /// Position in device pixels.
        x: f64,
        /// Position in device pixels.
        y: f64,
        /// `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    },
    /// A pointer button changed state.
    PointerButton {
        /// Evdev button code.
        button: u32,
        /// Pressed or released.
        state: ButtonState,
        /// `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    },
    /// Scrolling, in logical pixels.
    PointerAxis {
        /// Horizontal scroll.
        dx: f32,
        /// Vertical scroll.
        dy: f32,
        /// Where the scroll came from.
        source: AxisSource,
        /// `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    },
    /// A key changed state; the evdev keycode, without the xkb `+8`.
    Key {
        /// Evdev keycode.
        keycode: u32,
        /// Pressed or released.
        pressed: bool,
        /// `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    },
    /// A touch point changed, in device pixels.
    Touch {
        /// Touch point id, stable from `Down` to `Up`/`Cancel`.
        id: i32,
        /// What happened.
        phase: TouchPhase,
        /// Position in device pixels (ignored for `Up` and `Cancel`).
        x: f64,
        /// Position in device pixels.
        y: f64,
        /// `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
    },
}

impl InputEvent {
    /// The event's timestamp, which is what the input-to-photon clock
    /// starts from.
    #[must_use]
    pub fn time_ns(&self) -> u64 {
        match self {
            InputEvent::PointerMotion { time_ns, .. }
            | InputEvent::PointerAbsolute { time_ns, .. }
            | InputEvent::PointerButton { time_ns, .. }
            | InputEvent::PointerAxis { time_ns, .. }
            | InputEvent::Key { time_ns, .. }
            | InputEvent::Touch { time_ns, .. } => *time_ns,
        }
    }
}

/// Where input comes from: real devices, or a test's queue.
pub trait InputSource {
    /// Descriptors to watch for readability. Empty for a source that never
    /// blocks (the fake one).
    fn poll_fds(&self) -> Vec<BorrowedFd<'_>>;

    /// Drain everything available, appending to `out`. Never blocks.
    fn dispatch(&mut self, out: &mut Vec<InputEvent>);

    /// Stop touching devices (the session went away).
    fn suspend(&mut self);

    /// Start again after a [`InputSource::suspend`].
    fn resume(&mut self);

    /// Human-readable device summary for the startup log.
    fn describe(&self) -> String;
}

/// A handle a test uses to inject input into a running server.
///
/// The server's loop is an epoll over descriptors, so a queue alone would
/// never be noticed: the handle carries an `eventfd` that [`FakeInput::push`]
/// writes to, which is what wakes the loop. That makes the fake source a
/// faithful stand-in for libinput — same "fd readable, then dispatch"
/// shape — rather than something the loop has to poll for.
#[derive(Debug, Clone)]
pub struct FakeInput {
    queue: Arc<Mutex<VecDeque<InputEvent>>>,
    notify: Arc<OwnedFd>,
}

impl FakeInput {
    /// A new, empty injection point.
    ///
    /// # Errors
    /// If the `eventfd` cannot be created.
    pub fn new() -> rustix::io::Result<Self> {
        let notify = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )?;
        Ok(Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(notify),
        })
    }

    /// Queue an event and wake the server's loop.
    pub fn push(&self, event: InputEvent) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(event);
        }
        // A failed write means the counter is saturated, which only
        // happens if the server has not drained in 2^64 - 1 events; it is
        // already awake in that case.
        let _ = rustix::io::write(self.notify.as_fd(), &1u64.to_ne_bytes());
    }

    /// Whether everything pushed has been consumed.
    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.queue.lock().is_ok_and(|q| q.is_empty())
    }
}

/// A source a test drives through a [`FakeInput`] handle.
#[derive(Debug)]
pub struct FakeSource {
    shared: FakeInput,
    /// Set while suspended; a suspended source yields nothing, exactly
    /// like libinput with its devices closed.
    suspended: bool,
}

impl FakeSource {
    /// A source reading from `shared`.
    #[must_use]
    pub fn new(shared: FakeInput) -> Self {
        Self {
            shared,
            suspended: false,
        }
    }

    /// A source nothing can ever push to: what a server with input
    /// disabled runs with.
    ///
    /// # Errors
    /// If the `eventfd` cannot be created.
    pub fn idle() -> rustix::io::Result<Self> {
        Ok(Self::new(FakeInput::new()?))
    }
}

impl InputSource for FakeSource {
    fn poll_fds(&self) -> Vec<BorrowedFd<'_>> {
        vec![self.shared.notify.as_fd()]
    }

    fn dispatch(&mut self, out: &mut Vec<InputEvent>) {
        // Drain the counter first: the fd is level-triggered, so leaving it
        // readable would spin the loop.
        let mut buf = [0u8; 8];
        let _ = rustix::io::read(self.shared.notify.as_fd(), &mut buf);
        let Ok(mut queue) = self.shared.queue.lock() else {
            return;
        };
        if self.suspended {
            queue.clear();
            return;
        }
        out.extend(queue.drain(..));
    }

    fn suspend(&mut self) {
        self.suspended = true;
    }

    fn resume(&mut self) {
        self.suspended = false;
    }

    fn describe(&self) -> String {
        String::from("fake input source")
    }
}

/// Where the pointer is and what it is over.
#[derive(Debug, Default)]
pub struct Pointer {
    /// Position in device pixels.
    pub x: f64,
    /// Position in device pixels.
    pub y: f64,
    /// The window the pointer is currently inside, if any.
    pub over: Option<WindowKey>,
    /// The output the position falls on.
    pub output: Option<OutputId>,
    /// Whether any pointer device exists (no device: no cursor drawn).
    pub present: bool,
}

impl Pointer {
    /// Position as a scene point.
    #[must_use]
    pub fn position(&self) -> Point {
        Point::new(self.x as f32, self.y as f32)
    }

    /// Device-pixel position, rounded towards the pixel the hotspot is in.
    #[must_use]
    pub fn device(&self) -> (i32, i32) {
        (self.x.floor() as i32, self.y.floor() as i32)
    }

    /// Move to `(x, y)`, clamped to the union of the outputs. Returns
    /// whether the position changed.
    ///
    /// Clamping to the *union* rather than to each output means the pointer
    /// can cross an inter-output gap in one motion, which is what a user
    /// expects; it may momentarily sit in a gap between two outputs of
    /// different heights, where it hits nothing and is drawn nowhere.
    pub fn move_to(&mut self, x: f64, y: f64, bounds: Option<(f64, f64, f64, f64)>) -> bool {
        let (nx, ny) = match bounds {
            Some((x0, y0, x1, y1)) => (x.clamp(x0, x1), y.clamp(y0, y1)),
            None => (x, y),
        };
        // An exact comparison is the right one here: the question is
        // whether this event changed the stored value at all, not whether
        // two computed positions are near each other.
        #[allow(clippy::float_cmp)]
        let moved = nx != self.x || ny != self.y;
        self.x = nx;
        self.y = ny;
        moved
    }
}

/// The union of every output's device rect, as `(x0, y0, x1_inclusive,
/// y1_inclusive)` in the pointer's float space. `None` when there are no
/// outputs, in which case the pointer is not clamped at all — there is
/// nothing to clamp it to and the next hotplug will place it.
#[must_use]
pub fn output_union(scene: &Scene) -> Option<(f64, f64, f64, f64)> {
    let mut union: Option<nitro_core::IRect> = None;
    for (_, rect, _) in scene.outputs() {
        union = Some(match union {
            Some(u) => u.union(&rect),
            None => rect,
        });
    }
    let u = union.filter(|u| !u.is_empty())?;
    Some((
        f64::from(u.x),
        f64::from(u.y),
        f64::from(u.right() - 1),
        f64::from(u.bottom() - 1),
    ))
}

/// The output a device-pixel point falls on.
#[must_use]
pub fn output_at(scene: &Scene, point: Point) -> Option<OutputId> {
    let (x, y) = (point.x.floor() as i32, point.y.floor() as i32);
    scene
        .outputs()
        .find(|(_, rect, _)| rect.contains(x, y))
        .map(|(id, _, _)| id)
}

/// What the pointer is over, after a move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerTarget {
    /// The window hit.
    pub window: WindowKey,
    /// The node hit inside it.
    pub hit: Hit,
    /// The position in the window's own coordinates.
    pub local: Point,
}

/// Hit-test the scene at the pointer's position.
///
/// The window-local position is *not* the node-local one the scene returns:
/// a client is told where the pointer is in its window's space, so it can
/// compare the coordinate against the layout it sent, whatever transforms
/// the intervening groups apply.
#[must_use]
pub fn hit(scene: &Scene, output: OutputId, point: Point) -> Option<PointerTarget> {
    let hit = scene.hit_test(output, point)?;
    let local = window_local(scene, hit.window, point).unwrap_or(hit.local);
    Some(PointerTarget {
        window: hit.window,
        hit,
        local,
    })
}

/// A device point in a window's own coordinate space.
#[must_use]
pub fn window_local(scene: &Scene, win: WindowKey, point: Point) -> Option<Point> {
    let root = scene.window_info(win).ok()?.root();
    let node = scene.node(root).ok()?;
    node.world_transform().invert().map(|t| t.apply(point))
}

/// Convert a libinput timestamp (`CLOCK_MONOTONIC` microseconds) to the
/// nanoseconds the protocol uses.
#[must_use]
pub fn time_ns(time_usec: u64) -> u64 {
    time_usec.saturating_mul(1_000)
}

/// The libinput-backed source.
///
/// Holds the `Libinput` context and, through the interface, the seat: every
/// device fd it opens is owned by [`nitro_seat`], so dropping this closes
/// them through the seat exactly like the DRM device.
pub struct LibinputSource {
    context: input::Libinput,
    /// Paths added at startup, for the log and for `resume`.
    devices: Vec<PathBuf>,
    suspended: bool,
}

impl std::fmt::Debug for LibinputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibinputSource")
            .field("devices", &self.devices)
            .field("suspended", &self.suspended)
            .finish_non_exhaustive()
    }
}

/// The `LibinputInterface` that routes device opening through the seat.
///
/// libinput asks for a path and gets a descriptor back. Everything else in
/// the process is forbidden to open `/dev/input/*`; this is the one place,
/// and it delegates to [`nitro_seat::Seat`], so the fds are the session's
/// and are revoked on a VT switch like any other.
///
/// The bookkeeping matters more than it looks. libinput hands back only
/// the raw descriptor on close, so the [`nitro_seat::Device`] it belongs to
/// has to be findable from that number — and it has to be *closed through
/// the seat*, not merely dropped on the floor. Leaving it open is what
/// makes `Libinput::resume` fail after a VT switch: libinput closed its
/// copies at suspend and asks for the same paths again, and libseat
/// refuses a device it still has open.
struct SeatInterface {
    seat: Rc<RefCell<nitro_seat::Seat>>,
    /// Devices currently open, paired with the raw descriptor libinput
    /// was given for each.
    open: Vec<(RawFd, nitro_seat::Device)>,
}

impl input::LibinputInterface for SeatInterface {
    fn open_restricted(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, i32> {
        // The flags libinput asks for (`O_RDWR | O_NONBLOCK`) are what
        // libseat opens with anyway; there is no way to pass them through
        // and no device where it matters.
        let device = self
            .seat
            .borrow_mut()
            .open_device(path)
            .map_err(|e| e.raw_os_error().unwrap_or(EACCES))?;
        let fd = device
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| e.raw_os_error().unwrap_or(EACCES))?;
        self.open.push((fd.as_raw_fd(), device));
        Ok(fd)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        let raw = fd.as_raw_fd();
        // Drop libinput's copy first, then give the seat's own fd back, so
        // the device is genuinely closed and can be reopened at resume.
        drop(fd);
        let Some(index) = self.open.iter().position(|(r, _)| *r == raw) else {
            // A descriptor we never handed out: nothing to close, and
            // nothing that can be done about it either.
            return;
        };
        let (_, device) = self.open.remove(index);
        if let Ok(mut seat) = self.seat.try_borrow_mut()
            && let Err(e) = seat.close_device(device)
        {
            warn!("closing an input device: {e}");
        }
        // A failed borrow means we are inside a seat dispatch, which
        // cannot happen from libinput; `device` then closes through its
        // own `Drop`, which routes to the same place.
    }
}

/// `EACCES`, the errno to report when we have nothing better. Spelling it
/// out avoids a `libc` dependency for one constant.
const EACCES: i32 = 13;

/// Devices found under `/dev/input`, sorted, without udev.
///
/// Only `event*` nodes: `mouse*` and `js*` are legacy interfaces for the
/// same hardware and libinput would reject them.
#[must_use]
pub fn event_devices(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    // `event10` must sort after `event9`, which a plain string sort gets
    // wrong; the order is cosmetic (it only affects the log) but a
    // surprising order in a log is a bug report waiting to happen.
    paths.sort_by_key(|p| {
        let n = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("event"))
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(u32::MAX);
        (n, p.clone())
    });
    paths
}

impl LibinputSource {
    /// Open every `/dev/input/event*` through the seat.
    ///
    /// Devices that fail to open are logged and skipped: one broken node
    /// must not cost the user their keyboard.
    #[must_use]
    pub fn open(seat: Rc<RefCell<nitro_seat::Seat>>, dir: &Path) -> Self {
        let mut context = input::Libinput::new_from_path(SeatInterface {
            seat,
            open: Vec::new(),
        });
        let mut added = Vec::new();
        for path in event_devices(dir) {
            let Some(name) = path.to_str() else {
                continue;
            };
            if context.path_add_device(name).is_some() {
                added.push(path);
            } else {
                debug!("{}: not an input device libinput wants", path.display());
            }
        }
        Self {
            context,
            devices: added,
            suspended: false,
        }
    }

    /// Whether any device was opened at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

impl InputSource for LibinputSource {
    fn poll_fds(&self) -> Vec<BorrowedFd<'_>> {
        vec![self.context.as_fd()]
    }

    fn dispatch(&mut self, out: &mut Vec<InputEvent>) {
        if let Err(e) = self.context.dispatch() {
            warn!("libinput dispatch: {e}");
            return;
        }
        for event in self.context.by_ref() {
            if let Some(converted) = convert(&event) {
                out.push(converted);
            }
        }
    }

    fn suspend(&mut self) {
        if !self.suspended {
            self.context.suspend();
            self.suspended = true;
        }
    }

    fn resume(&mut self) {
        if self.suspended {
            if self.context.resume().is_err() {
                warn!("libinput resume failed; input is gone until the next VT switch");
            }
            self.suspended = false;
        }
    }

    fn describe(&self) -> String {
        format!("libinput with {} device(s)", self.devices.len())
    }
}

/// Translate one libinput event, or `None` for the ones the server has no
/// use for in M1 (tablet, pad, gesture, switch, device add/remove).
fn convert(event: &input::Event) -> Option<InputEvent> {
    use input::event::keyboard::{KeyState, KeyboardEventTrait as _};
    use input::event::pointer::{
        Axis, ButtonState as LibButtonState, PointerEventTrait as _, PointerScrollEvent as _,
    };
    use input::event::touch::{TouchEventPosition as _, TouchEventSlot as _, TouchEventTrait as _};
    use input::event::{PointerEvent, TouchEvent};

    match event {
        input::Event::Keyboard(input::event::KeyboardEvent::Key(k)) => Some(InputEvent::Key {
            keycode: k.key(),
            pressed: k.key_state() == KeyState::Pressed,
            time_ns: time_ns(k.time_usec()),
        }),
        input::Event::Pointer(p) => match p {
            PointerEvent::Motion(m) => Some(InputEvent::PointerMotion {
                dx: m.dx(),
                dy: m.dy(),
                time_ns: time_ns(m.time_usec()),
            }),
            PointerEvent::MotionAbsolute(m) => Some(InputEvent::PointerAbsolute {
                // Transformed against a 1×1 box: the caller scales to the
                // output it lands on, which is the only place that knows
                // the geometry.
                x: m.absolute_x_transformed(1),
                y: m.absolute_y_transformed(1),
                time_ns: time_ns(m.time_usec()),
            }),
            PointerEvent::Button(b) => Some(InputEvent::PointerButton {
                button: b.button(),
                state: match b.button_state() {
                    LibButtonState::Pressed => ButtonState::Pressed,
                    LibButtonState::Released => ButtonState::Released,
                },
                time_ns: time_ns(b.time_usec()),
            }),
            PointerEvent::ScrollWheel(s) => Some(scroll(
                s.scroll_value(Axis::Horizontal),
                s.scroll_value(Axis::Vertical),
                AxisSource::Wheel,
                time_ns(s.time_usec()),
            )),
            PointerEvent::ScrollFinger(s) => Some(scroll(
                s.scroll_value(Axis::Horizontal),
                s.scroll_value(Axis::Vertical),
                AxisSource::Finger,
                time_ns(s.time_usec()),
            )),
            PointerEvent::ScrollContinuous(s) => Some(scroll(
                s.scroll_value(Axis::Horizontal),
                s.scroll_value(Axis::Vertical),
                AxisSource::Continuous,
                time_ns(s.time_usec()),
            )),
            // The deprecated `Axis` event is only emitted by libinput
            // versions before 1.19 and never alongside the `Scroll*` ones;
            // handling it too would double every scroll.
            _ => None,
        },
        input::Event::Touch(t) => match t {
            TouchEvent::Down(d) => Some(InputEvent::Touch {
                id: d.slot().unwrap_or(0).cast_signed(),
                phase: TouchPhase::Down,
                x: d.x_transformed(1),
                y: d.y_transformed(1),
                time_ns: time_ns(d.time_usec()),
            }),
            TouchEvent::Motion(m) => Some(InputEvent::Touch {
                id: m.slot().unwrap_or(0).cast_signed(),
                phase: TouchPhase::Move,
                x: m.x_transformed(1),
                y: m.y_transformed(1),
                time_ns: time_ns(m.time_usec()),
            }),
            TouchEvent::Up(u) => Some(InputEvent::Touch {
                id: u.slot().unwrap_or(0).cast_signed(),
                phase: TouchPhase::Up,
                x: 0.0,
                y: 0.0,
                time_ns: time_ns(u.time_usec()),
            }),
            TouchEvent::Cancel(c) => Some(InputEvent::Touch {
                id: c.slot().unwrap_or(0).cast_signed(),
                phase: TouchPhase::Cancel,
                x: 0.0,
                y: 0.0,
                time_ns: time_ns(c.time_usec()),
            }),
            // `Frame` marks the end of a sample; with one touch point per
            // event there is nothing to batch. The enum is
            // `non_exhaustive`, so future phases land here too, ignored
            // until the server knows what to do with them.
            _ => None,
        },
        _ => None,
    }
}

fn scroll(dx: f64, dy: f64, source: AxisSource, time_ns: u64) -> InputEvent {
    InputEvent::PointerAxis {
        dx: dx as f32,
        dy: dy as f32,
        source,
        time_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_core::{IRect, Rect, Size};
    use nitro_scene::{ClientId, Fill, Layer, NodeKind};

    fn scene_with_window(position: Point) -> (Scene, OutputId, WindowKey) {
        let mut scene = Scene::new();
        let output = OutputId(1);
        scene.add_output(output, IRect::new(0, 0, 200, 100), 1.0);
        let client = ClientId(1);
        let win = scene.create_window(client, "w", Size::new(80.0, 40.0), Layer::Normal);
        scene.place_window(win, Some(output), position).unwrap();
        let root = scene.window_info(win).unwrap().root();
        let rect = scene
            .create_node(client, NodeKind::Rect, root, None)
            .unwrap();
        scene
            .set_bounds(client, rect, Rect::new(0.0, 0.0, 80.0, 40.0))
            .unwrap();
        scene
            .set_fill(client, rect, Fill::Solid(nitro_core::Color::WHITE))
            .unwrap();
        let mut damage = nitro_core::Damage::new();
        scene.update(&mut nitro_scene::DamageSink::new(&mut [(
            output,
            &mut damage,
        )]));
        (scene, output, win)
    }

    #[test]
    fn the_pointer_is_clamped_to_the_union_of_the_outputs() {
        let (scene, _, _) = scene_with_window(Point::new(10.0, 10.0));
        let bounds = output_union(&scene).unwrap();
        assert_eq!(bounds, (0.0, 0.0, 199.0, 99.0));
        let mut p = Pointer::default();
        assert!(p.move_to(-50.0, 500.0, Some(bounds)));
        assert_eq!((p.x, p.y), (0.0, 99.0));
        assert!(p.move_to(1000.0, -1.0, Some(bounds)));
        assert_eq!((p.x, p.y), (199.0, 0.0));
        // Moving nowhere reports no change, so nothing is damaged.
        assert!(!p.move_to(199.0, 0.0, Some(bounds)));
    }

    #[test]
    fn hit_testing_reports_window_local_coordinates() {
        let (scene, output, win) = scene_with_window(Point::new(10.0, 20.0));
        let target = hit(&scene, output, Point::new(30.0, 40.0)).expect("inside the window");
        assert_eq!(target.window, win);
        assert_eq!(target.local, Point::new(20.0, 20.0));
        // Outside every window: nothing is hit.
        assert!(hit(&scene, output, Point::new(150.0, 90.0)).is_none());
    }

    #[test]
    fn a_scaled_output_maps_device_pixels_back_to_logical_units() {
        let mut scene = Scene::new();
        let output = OutputId(1);
        scene.add_output(output, IRect::new(0, 0, 400, 200), 2.0);
        let client = ClientId(1);
        let win = scene.create_window(client, "w", Size::new(80.0, 40.0), Layer::Normal);
        scene
            .place_window(win, Some(output), Point::new(10.0, 10.0))
            .unwrap();
        let root = scene.window_info(win).unwrap().root();
        let rect = scene
            .create_node(client, NodeKind::Rect, root, None)
            .unwrap();
        scene
            .set_bounds(client, rect, Rect::new(0.0, 0.0, 80.0, 40.0))
            .unwrap();
        scene
            .set_fill(client, rect, Fill::Solid(nitro_core::Color::WHITE))
            .unwrap();
        let mut damage = nitro_core::Damage::new();
        scene.update(&mut nitro_scene::DamageSink::new(&mut [(
            output,
            &mut damage,
        )]));
        // The window's top-left corner is at device (20, 20) because the
        // output scale is 2; a point 10 device pixels further in is 5
        // logical units into the window.
        let target = hit(&scene, output, Point::new(30.0, 30.0)).expect("inside");
        assert_eq!(target.local, Point::new(5.0, 5.0));
    }

    #[test]
    fn output_lookup_finds_the_output_under_a_point() {
        let mut scene = Scene::new();
        scene.add_output(OutputId(1), IRect::new(0, 0, 100, 100), 1.0);
        scene.add_output(OutputId(2), IRect::new(100, 0, 100, 100), 1.0);
        assert_eq!(output_at(&scene, Point::new(50.0, 50.0)), Some(OutputId(1)));
        assert_eq!(
            output_at(&scene, Point::new(150.0, 50.0)),
            Some(OutputId(2))
        );
        assert_eq!(output_at(&scene, Point::new(250.0, 50.0)), None);
        assert_eq!(output_union(&scene), Some((0.0, 0.0, 199.0, 99.0)));
    }

    #[test]
    fn the_fake_source_delivers_in_order_and_honours_suspend() {
        let handle = FakeInput::new().unwrap();
        let mut src = FakeSource::new(handle.clone());
        handle.push(InputEvent::PointerMotion {
            dx: 1.0,
            dy: 2.0,
            time_ns: 10,
        });
        handle.push(InputEvent::Key {
            keycode: 30,
            pressed: true,
            time_ns: 20,
        });
        let mut out = Vec::new();
        src.dispatch(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].time_ns(), 10);
        assert_eq!(out[1].time_ns(), 20);
        assert!(handle.is_drained());

        src.suspend();
        handle.push(InputEvent::Key {
            keycode: 30,
            pressed: false,
            time_ns: 30,
        });
        out.clear();
        src.dispatch(&mut out);
        assert!(out.is_empty(), "a suspended source delivers nothing");
        src.resume();
        handle.push(InputEvent::Key {
            keycode: 30,
            pressed: false,
            time_ns: 40,
        });
        src.dispatch(&mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn pushing_makes_the_source_fd_readable() {
        let handle = FakeInput::new().unwrap();
        let mut src = FakeSource::new(handle.clone());
        let readable = |src: &FakeSource| {
            let fds = src.poll_fds();
            let mut pfd = [rustix::event::PollFd::new(
                &fds[0],
                rustix::event::PollFlags::IN,
            )];
            rustix::event::poll(&mut pfd, Some(&rustix::fs::Timespec::default())).unwrap() == 1
        };
        assert!(!readable(&src), "an empty source never wakes the loop");
        handle.push(InputEvent::Key {
            keycode: 1,
            pressed: true,
            time_ns: 1,
        });
        assert!(readable(&src));
        let mut out = Vec::new();
        src.dispatch(&mut out);
        assert_eq!(out.len(), 1);
        assert!(!readable(&src), "draining clears the readiness");
    }

    #[test]
    fn libinput_microseconds_become_nanoseconds() {
        assert_eq!(time_ns(0), 0);
        assert_eq!(time_ns(1), 1_000);
        assert_eq!(time_ns(1_234_567), 1_234_567_000);
        // Saturating rather than wrapping: a bogus timestamp must not
        // produce a latency measured in negative centuries.
        assert_eq!(time_ns(u64::MAX), u64::MAX);
    }

    #[test]
    fn event_device_scan_sorts_numerically_and_skips_legacy_nodes() {
        let dir = std::env::temp_dir().join(format!("nitro-input-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["event10", "event2", "event1", "mouse0", "js0", "by-path"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let found: Vec<String> = event_devices(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, ["event1", "event2", "event10"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
