//! The in-process test harness (feature `test-support`).
//!
//! A [`Harness`] starts a real `nitro-server` on the fake backend in a
//! thread of the test process, connects a real client to it, and runs the
//! app's [`Ui`] **on the test thread** — there is no `App::run` loop and
//! nothing to race with. The test pumps: it injects synthetic pointer and
//! key events through the server's fake `InputSource`, drains the socket,
//! flushes the tree and takes screenshots.
//!
//! What that buys is that every assertion is about the real thing. A
//! click really travels evdev-code → server hit test → wire → widget; a
//! repaint really produces the mutations it claims to; and
//! [`Harness::shot`] really shows the pixels the rasterizer wrote.
//!
//! ```no_run
//! use nitro_ui::test::Harness;
//! use nitro_ui::widgets::{column, label};
//! use nitro_ui::build::ContainerBuilder as _;
//!
//! let mut h = Harness::new("demo", (), |ui| {
//!     ui.build(column().gap(8.0).child(label("Hello")))
//! });
//! h.settle();
//! let shot = h.shot();
//! assert_eq!(shot.width, 320);
//! ```

use std::time::{Duration, Instant};

use nitro_core::{Point, Rect, Size};
use nitro_server::input::InputEvent;
use nitro_server::test_support::{Image, TestServer, wait_for};
use nitro_wire::client::Connection;
use nitro_wire::types::ButtonState;

use crate::app::App;
use crate::arena::WidgetId;
use crate::event::button;
use crate::theme::Theme;
use crate::ui::Ui;
use crate::widget::Widget;
use crate::wire::Mutation;

/// Default output size the harness runs the server at.
pub const OUTPUT: (u32, u32) = (320, 240);

/// A server, a client and a widget tree, all in this process.
pub struct Harness<S> {
    server: TestServer,
    ui: Ui<S>,
    state: S,
    /// Where the window landed on the output; input is injected in output
    /// coordinates, so a test that says "click this widget" needs it.
    origin: Point,
    time_ns: u64,
    /// The app's introspection socket, once a test has asked for one.
    socket: Option<crate::introspect::Socket>,
}

impl<S: 'static> Harness<S> {
    /// Start a server, connect, build the tree and open a window sized to
    /// the root.
    ///
    /// # Panics
    /// If the server does not come up or the connection fails.
    pub fn new(name: &str, state: S, build: impl FnOnce(&mut Ui<S>) -> WidgetId) -> Self {
        Self::with(name, state, None, Theme::default(), build)
    }

    /// As [`Harness::new`], with an explicit window size.
    ///
    /// # Panics
    /// As [`Harness::new`].
    pub fn sized(
        name: &str,
        state: S,
        size: Size,
        build: impl FnOnce(&mut Ui<S>) -> WidgetId,
    ) -> Self {
        Self::with(name, state, Some(size), Theme::default(), build)
    }

    /// The full form: size and theme both explicit.
    ///
    /// # Panics
    /// As [`Harness::new`].
    pub fn with(
        name: &str,
        state: S,
        size: Option<Size>,
        theme: Theme,
        build: impl FnOnce(&mut Ui<S>) -> WidgetId,
    ) -> Self {
        Self::with_options(name, state, size, theme, true, build)
    }

    /// As [`Harness::with`], with the window backdrop under control too.
    ///
    /// # Panics
    /// As [`Harness::new`].
    pub fn with_options(
        name: &str,
        state: S,
        size: Option<Size>,
        theme: Theme,
        backdrop: bool,
        build: impl FnOnce(&mut Ui<S>) -> WidgetId,
    ) -> Self {
        let server = TestServer::start(name, OUTPUT.0, OUTPUT.1);
        // Park the pointer in a corner: the server puts it in the middle
        // of the output, where it would contaminate every pixel
        // assertion and hover every widget under it.
        server.push_input(InputEvent::PointerAbsolute {
            x: 0.999,
            y: 0.999,
            time_ns: 1_000_000,
        });
        let conn = Connection::connect(server.wire_path(), name).expect("wire connect");
        let mut app = App::with_connection(conn, name).theme(theme);
        if !backdrop {
            app = app.transparent();
        }
        if let Some(s) = size {
            app = app.size(s);
        }
        let ui = app.build(build).expect("build the tree");
        let mut h = Self {
            server,
            ui,
            state,
            origin: Point::ZERO,
            time_ns: 2_000_000,
            socket: None,
        };
        // `shot` over the introspection socket screenshots *this*
        // harness's server, not whatever `$NITRO_CONTROL` happens to
        // name in the test runner's environment.
        let control = h.server.control_path().to_path_buf();
        h.ui.set_control_path(control);
        h.settle();
        // Keys only reach a focused window, and focus follows the click.
        // A harness runs one window and a test that sends a key means
        // that window, so it is focused up front rather than through a
        // synthetic click that would disturb the state under test.
        h.server.focus_window();
        h.settle();
        h
    }

    /// The widget tree.
    pub fn ui(&mut self) -> &mut Ui<S> {
        &mut self.ui
    }

    /// The app state.
    pub fn state(&self) -> &S {
        &self.state
    }

    /// The app state, mutably.
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    /// The server, for control requests and statistics.
    pub fn server(&self) -> &TestServer {
        &self.server
    }

    /// Borrow a widget by type.
    ///
    /// # Panics
    /// If the id is stale or names a different type.
    pub fn widget<W: Widget<S>>(&self, id: WidgetId) -> &W {
        self.ui.widget::<W>(id).expect("widget")
    }

    /// A widget's bounds in window coordinates.
    #[must_use]
    pub fn bounds(&self, id: WidgetId) -> Rect {
        self.ui.window_bounds(id)
    }

    /// Whether the server has fonts, and so whether text draws at all.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.ui.has_text()
    }

    /// Open this app's introspection socket in a fresh directory and
    /// return its path.
    ///
    /// The harness runs the `Ui` on the test thread, so the socket is
    /// served by [`Harness::settle`] the way the app loop serves it:
    /// between events, with the tree settled. A test therefore drives
    /// `hey` (or a raw socket) from *another* thread and pumps here.
    ///
    /// # Panics
    /// If the socket cannot be bound.
    pub fn open_socket(&mut self, name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nitro-hey-test-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(format!("{name}.{}.sock", std::process::id()));
        let socket = crate::introspect::Socket::bind_at(&path).expect("bind app socket");
        self.socket = Some(socket);
        path
    }

    /// The directory [`Harness::open_socket`] put the socket in, which is
    /// what `hey` would be pointed at with `NITRO_APPS_DIR`.
    #[must_use]
    pub fn socket_dir(&self) -> Option<std::path::PathBuf> {
        self.socket
            .as_ref()
            .and_then(|s| s.path().parent().map(std::path::Path::to_path_buf))
    }

    /// Accept and serve one round of introspection requests, exactly as
    /// the app loop does.
    pub fn serve_socket(&mut self) {
        if let Some(s) = &mut self.socket {
            s.accept();
            s.serve(&mut self.ui, &mut self.state);
        }
    }

    /// Pump the tree and the introspection socket until `f` is true or
    /// ten seconds pass.
    ///
    /// A test that drives the socket from another thread needs this: the
    /// request only runs when this thread serves it.
    ///
    /// # Panics
    /// On timeout.
    pub fn pump_socket_until(&mut self, what: &str, mut f: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump();
            self.serve_socket();
            let _ = self.ui.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(2));
        }
        // One last round, so the reply the condition observed is written.
        self.serve_socket();
    }

    /// Drain the socket, dispatch events and flush the tree, until
    /// nothing more arrives and the server has gone quiet.
    ///
    /// # Panics
    /// On a wire failure, or if the server never settles.
    pub fn settle(&mut self) {
        for _ in 0..64 {
            self.pump();
            self.serve_socket();
            let sent = self.ui.flush().expect("flush");
            if !sent && !self.drain_pending() {
                break;
            }
        }
        self.server.settle();
        self.pump();
        self.serve_socket();
        let _ = self.ui.flush().expect("flush");
        self.server.settle();
        self.locate_window();
    }

    /// One non-blocking drain-and-dispatch pass.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn pump(&mut self) -> usize {
        self.ui.pump(&mut self.state).expect("pump")
    }

    /// Whether anything arrived in a short window. Used to decide whether
    /// `settle` has anything left to do.
    fn drain_pending(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(20);
        while Instant::now() < deadline {
            if self.pump() > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// Find where the server placed our window.
    ///
    /// Since M3 the server decorates windows and places them
    /// centred-cascade inside the work area, so "the first window is at
    /// the origin" is no longer true — and the harness's coordinates are
    /// *window* coordinates, which have to be shifted onto the output. The
    /// server tells us exactly where the content landed in every
    /// `Configure`, so use that rather than re-deriving the policy here.
    fn locate_window(&mut self) {
        self.origin = self.ui.window_position();
    }

    // -- input --------------------------------------------------------

    /// Move the pointer to `pos`, in window coordinates.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn move_pointer(&mut self, pos: Point) {
        self.time_ns += 1_000_000;
        // An absolute device reports in its own unit square; the server
        // scales that onto the first output, so window coordinates have
        // to go back through the same division.
        self.server.push_input(InputEvent::PointerAbsolute {
            x: f64::from(self.origin.x + pos.x) / f64::from(OUTPUT.0),
            y: f64::from(self.origin.y + pos.y) / f64::from(OUTPUT.1),
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Press and release the left button at `pos` (window coordinates).
    ///
    /// # Panics
    /// On a wire failure.
    pub fn click_at(&mut self, pos: Point) {
        self.move_pointer(pos);
        self.press(button::LEFT);
        self.release(button::LEFT);
    }

    /// Click the centre of `id`.
    ///
    /// # Panics
    /// On a wire failure, or if the widget has no area.
    pub fn click(&mut self, id: WidgetId) {
        let b = self.bounds(id);
        assert!(!b.is_empty(), "widget {id} has no bounds to click");
        self.click_at(Point::new(b.x + b.w / 2.0, b.y + b.h / 2.0));
    }

    /// Press a pointer button where the pointer is.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn press(&mut self, button: u32) {
        self.time_ns += 1_000_000;
        self.server.push_input(InputEvent::PointerButton {
            button,
            state: ButtonState::Pressed,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Release a pointer button.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn release(&mut self, button: u32) {
        self.time_ns += 1_000_000;
        self.server.push_input(InputEvent::PointerButton {
            button,
            state: ButtonState::Released,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Scroll the wheel by `notches` where the pointer is; positive is
    /// up, as the wire reports it.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn wheel(&mut self, notches: f32) {
        self.time_ns += 1_000_000;
        self.server.push_input(InputEvent::PointerAxis {
            dx: 0.0,
            dy: notches,
            source: nitro_wire::types::AxisSource::Wheel,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Press and release an evdev keycode.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn key(&mut self, keycode: u32) {
        self.key_down(keycode);
        self.key_up(keycode);
    }

    /// Inject a key press **without** settling.
    ///
    /// [`Harness::key`] pumps until everything has been consumed, which
    /// is what a test usually wants; this is for a test that needs to
    /// look at the tree between the event arriving and the dust
    /// settling.
    pub fn send_key(&mut self, keycode: u32) {
        self.time_ns += 1_000_000;
        self.server.push_input(InputEvent::Key {
            keycode,
            pressed: true,
            time_ns: self.time_ns,
        });
    }

    /// Press a key.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn key_down(&mut self, keycode: u32) {
        self.time_ns += 1_000_000;
        self.server.push_input(InputEvent::Key {
            keycode,
            pressed: true,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Release a key.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn key_up(&mut self, keycode: u32) {
        self.time_ns += 1_000_000;
        self.server.push_input(InputEvent::Key {
            keycode,
            pressed: false,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Press a key with a modifier held down, releasing both.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn key_with(&mut self, modifier: u32, keycode: u32) {
        self.key_down(modifier);
        self.key(keycode);
        self.key_up(modifier);
    }

    /// Resize the window, which the server answers with a `Configure`.
    ///
    /// Goes the short way — the client asks for a new size, so this is
    /// the equivalent of the server deciding to reconfigure us. The
    /// widget-facing path is identical: `Ui::dispatch` on a `Configure`.
    ///
    /// # Panics
    /// On a wire failure.
    pub fn configure(&mut self, size: Size) {
        let msg = nitro_wire::msg::ServerMsg::Configure(nitro_wire::msg::Configure {
            window: crate::ui::WINDOW,
            size,
            position: nitro_core::Point::ZERO,
            scale: self.ui.scale(),
            output: 0,
        });
        self.ui.dispatch(&mut self.state, &msg);
        self.settle();
    }

    // -- observation --------------------------------------------------

    /// A screenshot of this harness's **window**, cropped out of the
    /// output: pixel `(0, 0)` is the window's top-left content corner, so
    /// a test's coordinates are window coordinates everywhere.
    ///
    /// Since M3 the server decorates windows and places them
    /// centred-cascade, so the window is no longer at the output's origin
    /// and a raw output screenshot would make every pixel assertion depend
    /// on the placement policy. [`Harness::output_shot`] is still there for
    /// a test that wants the desktop around the window.
    ///
    /// # Panics
    /// If the control socket answers with an error.
    #[must_use]
    pub fn shot(&self) -> Image {
        let img = self.output_shot();
        let size = self.ui.window_size();
        let x0 = self.origin.x.max(0.0) as u32;
        let y0 = self.origin.y.max(0.0) as u32;
        let w = (size.w.max(0.0) as u32).min(img.width.saturating_sub(x0));
        let h = (size.h.max(0.0) as u32).min(img.height.saturating_sub(y0));
        let stride = w * 4;
        let mut data = Vec::with_capacity((stride * h) as usize);
        for y in 0..h {
            let src = ((y + y0) * img.stride + x0 * 4) as usize;
            data.extend_from_slice(&img.data[src..src + stride as usize]);
        }
        Image {
            width: w,
            height: h,
            stride,
            data,
        }
    }

    /// A screenshot of the whole output, decorations and desktop included.
    ///
    /// # Panics
    /// If the control socket answers with an error.
    #[must_use]
    pub fn output_shot(&self) -> Image {
        self.server.shot()
    }

    /// Whether any pixel inside `rect` (window coordinates) differs from
    /// `background`. That is how a test asserts "there is text here"
    /// without depending on which font the box has installed.
    ///
    /// # Panics
    /// If the screenshot cannot be taken.
    #[must_use]
    pub fn has_ink(&self, rect: Rect, background: u32) -> bool {
        self.ink_count(rect, background) > 0
    }

    /// Count the pixels inside `rect` that differ from `background`.
    ///
    /// # Panics
    /// If the screenshot cannot be taken.
    #[must_use]
    pub fn ink_count(&self, rect: Rect, background: u32) -> usize {
        let img = self.shot();
        let r = rect.round_out().intersect(&nitro_core::IRect::new(
            0,
            0,
            img.width.cast_signed(),
            img.height.cast_signed(),
        ));
        let mut n = 0;
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                if img.pixel(x as u32, y as u32) & 0x00ff_ffff != background & 0x00ff_ffff {
                    n += 1;
                }
            }
        }
        n
    }

    /// Start recording every mutation the tree sends.
    pub fn tap(&mut self) {
        self.ui.tap(true);
    }

    /// The mutations recorded since [`Harness::tap`] or the last
    /// [`Harness::clear_tap`].
    #[must_use]
    pub fn mutations(&self) -> &[Mutation] {
        self.ui.mutations()
    }

    /// Forget the recorded mutations, leaving the tap on.
    pub fn clear_tap(&mut self) {
        self.ui.clear_mutations();
    }

    /// Commits sent since the tree was created.
    #[must_use]
    pub fn commits(&self) -> u32 {
        self.ui.commit_count()
    }

    /// Assert that nothing is sent for `ms` milliseconds: the idle
    /// property, checked from the outside.
    ///
    /// # Panics
    /// If a commit is sent in that window.
    pub fn assert_idle(&mut self, ms: u64) {
        let before = self.ui.commit_count();
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            self.pump();
            let sent = self.ui.flush().expect("flush");
            assert!(!sent, "a settled tree committed while idle");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            self.ui.commit_count(),
            before,
            "a settled tree committed while idle"
        );
    }

    /// Wait for a condition, polling the server. See
    /// [`nitro_server::test_support::wait_for`].
    ///
    /// # Panics
    /// On timeout.
    pub fn wait_for(&mut self, what: &str, mut f: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Stop the server. Called by `Drop` too; this is the form that
    /// reports a server error.
    ///
    /// # Panics
    /// If the server returned an error.
    pub fn quit(mut self) {
        self.server.quit();
    }
}

/// Wait for something outside a harness.
///
/// # Panics
/// On timeout.
pub fn until(what: &str, f: impl FnMut() -> bool) {
    wait_for(what, f);
}
