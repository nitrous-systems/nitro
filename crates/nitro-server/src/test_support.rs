//! In-process server for another crate's tests, behind the
//! `test-support` feature.
//!
//! `tests/fake_loop.rs` grew a `Harness` that starts [`run`](crate::run)
//! on a thread with a [`FakeBackend`](nitro_kms::FakeBackend) and a
//! [`FakeInput`](crate::input::FakeInput), and talks to it over the v0
//! control socket. `nitro-ui`'s test harness needs exactly the same
//! thing — a real server, a real wire socket, synthetic input and
//! screenshots — so the useful half of it lives here instead of being
//! copied.
//!
//! It is behind a feature because it pulls the control-socket client and
//! a thread into anything that links it, and no shipped binary wants
//! either.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A screenshot: `XRGB8888` pixels, as [`TestServer::shot`] returns
/// them. Re-exported so a consumer needs no `nitro-kms` dependency of
/// its own.
pub use nitro_kms::Image;

use crate::input::{FakeInput, InputEvent};
use crate::{BackendKind, Config, run};

/// How long any wait in here is allowed to take. A test that hangs tells
/// you nothing.
const DEADLINE: Duration = Duration::from_secs(10);

/// A server running on a thread, on the fake backend.
///
/// Dropping it quits the server and removes its socket directory, so a
/// test that panics still leaves nothing behind.
#[derive(Debug)]
pub struct TestServer {
    dir: PathBuf,
    control_path: PathBuf,
    wire_path: PathBuf,
    input: FakeInput,
    thread: Option<JoinHandle<Result<(), crate::Error>>>,
}

impl TestServer {
    /// Start a server with one `width × height` output, with its sockets
    /// in a fresh directory under the system temporary directory named
    /// after `name` and this process.
    ///
    /// # Panics
    /// If the `eventfd` for the fake input source cannot be created, or
    /// the server does not come up within ten seconds.
    #[must_use]
    pub fn start(name: &str, width: u32, height: u32) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nitro-ui-test-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let control_path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(width, height, &control_path);
        config.backend = BackendKind::Fake { width, height };
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let s = Self {
            dir,
            control_path,
            wire_path,
            input,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&s.control_path).is_ok()
        });
        wait_for("the wire socket", || s.wire_path.exists());
        s
    }

    /// The wire socket clients connect to.
    #[must_use]
    pub fn wire_path(&self) -> &Path {
        &self.wire_path
    }

    /// The control socket path.
    #[must_use]
    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    /// The synthetic input source: push [`InputEvent`]s at it and the
    /// server's loop wakes exactly as it would for libinput.
    #[must_use]
    pub fn input(&self) -> &FakeInput {
        &self.input
    }

    /// Push one synthetic input event.
    pub fn push_input(&self, event: InputEvent) {
        self.input.push(event);
    }

    /// A control request whose answer is a bare status line, with no
    /// body: `quit` and `focus`. [`TestServer::request`] would block
    /// waiting for the blank line that terminates a body, and these have
    /// none.
    ///
    /// # Panics
    /// If the connection closes before the status line.
    pub fn request_line(&self, req: &str) -> String {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
    }

    /// Give keyboard focus to the topmost window: ours, since a
    /// `TestServer` runs one client.
    ///
    /// # Panics
    /// If the server refuses — which means there is no window yet.
    pub fn focus_window(&self) {
        let reply = self.request_line("focus\n");
        assert_eq!(reply, "ok", "focus refused: {reply}");
    }

    /// A control request whose answer is a status line plus a body
    /// terminated by a blank line (`outputs`, `stats`).
    ///
    /// # Panics
    /// If the connection closes mid-reply.
    #[must_use]
    pub fn request(&self, req: &str) -> Vec<String> {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut lines = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = c.read_line(&mut line).unwrap();
            assert!(n > 0, "connection closed mid-reply");
            let l = line.trim_end_matches('\n').to_owned();
            if l.is_empty() {
                break;
            }
            lines.push(l);
        }
        lines
    }

    /// One statistic from `stats`.
    ///
    /// # Panics
    /// If the server does not report `key`.
    #[must_use]
    pub fn stat(&self, key: &str) -> u64 {
        let lines = self.request("stats\n");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
            .parse()
            .unwrap()
    }

    /// Whether the server found any fonts, and so whether `Text` nodes
    /// will actually draw. A test that asserts on glyph pixels has to
    /// check this: a box with no fonts installed is a legitimate
    /// configuration.
    #[must_use]
    pub fn has_fonts(&self) -> bool {
        self.stat("fonts") > 0
    }

    /// A screenshot of the front buffer.
    ///
    /// # Panics
    /// If the control socket answers with an error.
    #[must_use]
    pub fn shot(&self) -> Image {
        let mut c = self.connect();
        c.get_mut().write_all(b"shot\n").unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let header = header.trim_end();
        let fields: Vec<u32> = header
            .strip_prefix("ok ")
            .unwrap_or_else(|| panic!("shot failed: {header}"))
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let (width, height, stride) = (fields[0], fields[1], fields[2]);
        let mut data = vec![0u8; (stride * height) as usize];
        c.read_exact(&mut data).unwrap();
        Image {
            width,
            height,
            stride,
            data,
        }
    }

    /// Wait until the server has finished reacting to whatever just
    /// happened: nothing in flight and the frame counter has stopped.
    ///
    /// Not "wait for N more frames": an idle server deliberately stops
    /// flipping, so counting frames would hang the moment the thing under
    /// test settled.
    ///
    /// # Panics
    /// If the server never goes quiet.
    pub fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let lines = self.request("stats\n");
            let value = |key: &str| -> u64 {
                lines
                    .iter()
                    .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let frames = value("frames");
            if value("flips_pending") == 0 && frames == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = frames;
            std::thread::sleep(Duration::from_millis(5));
            stable >= 3
        });
    }

    /// Stop the server and wait for its thread.
    ///
    /// # Panics
    /// If the server returned an error or did not stop.
    pub fn quit(&mut self) {
        if let Some(result) = self.shutdown() {
            result.expect("server returned an error");
        }
    }

    /// Ask the server to quit and join its thread. `None` when it was
    /// already stopped.
    ///
    /// The reply line **is** read before the connection is dropped: the
    /// server answers `ok` and only then tears the loop down, and closing
    /// the control socket first would race that against its own hangup
    /// handling.
    fn shutdown(&mut self) -> Option<Result<(), crate::Error>> {
        let t = self.thread.take()?;
        if let Ok(c) = UnixStream::connect(&self.control_path) {
            let _ = c.set_read_timeout(Some(DEADLINE));
            let mut c = BufReader::new(c);
            if c.get_mut().write_all(b"quit\n").is_ok() {
                let mut line = String::new();
                let _ = c.read_line(&mut line);
            }
        }
        wait_for("the server thread to stop", || t.is_finished());
        let result = t.join().expect("server thread panicked");
        let _ = std::fs::remove_dir_all(&self.dir);
        Some(result)
    }

    fn connect(&self) -> BufReader<UnixStream> {
        let s = UnixStream::connect(&self.control_path).expect("connect");
        s.set_read_timeout(Some(DEADLINE)).unwrap();
        BufReader::new(s)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // A test that panicked still has to stop its server, or the
        // harness leaks a thread and a socket directory per failure.
        // The result is dropped: a panic is already being reported and a
        // second one from `Drop` would abort the process.
        let _ = self.shutdown();
    }
}

/// Poll `f` until it is true, panicking after ten seconds.
///
/// # Panics
/// On timeout, naming `what`.
pub fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}
