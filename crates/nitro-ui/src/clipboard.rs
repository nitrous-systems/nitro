//! The system clipboard: offer, serve, request and read the selection.
//!
//! The server's clipboard (M5-H, `caps::DATA`) never carries bytes on the
//! socket. The owner *offers* MIME types; a paste earns the owner a
//! `SelectionRequest`, which it answers with a readable descriptor; the
//! requester gets that descriptor in a `SelectionData` and reads it. This
//! module is the toolkit's side of all four steps, so an app writes
//!
//! ```ignore
//! ui.set_clipboard_text("hello")?;
//! ui.read_clipboard(&[TEXT_MIME, PLAIN_MIME], |state, ui, got| { … });
//! ```
//!
//! and never touches a descriptor.
//!
//! **Reads are asynchronous and never block.** The callback runs later —
//! from the same [`Ui::pump`] that delivered the answer when the owner
//! handed over a memfd (the common case), or when the descriptor becomes
//! readable, or with `None` after [`READ_TIMEOUT_MS`], because a hostile
//! owner can hand over a pipe that never reaches EOF. A transfer larger
//! than [`MAX_CLIPBOARD_BYTES`] is dropped and answered `None`. The
//! callback is *always* deferred, even when the answer is known at once
//! (nothing offered, no matching type), so a caller has one code path.
//!
//! **Setting the clipboard needs keyboard focus.** The server disconnects
//! a client that sends `SetSelection` without it, so
//! [`Ui::set_clipboard`] checks first and answers `Ok(false)` instead of
//! sending.
//!
//! **Without `DATA`** — a remote link, or a server that predates it — the
//! clipboard is app-local: what [`Ui::set_clipboard`] stores is what
//! [`Ui::read_clipboard`] reads back, so copy and paste inside one app
//! keep working.

use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};

use nitro_wire::msg::{
    ClientMsg, RequestSelection, SelectionData, SelectionOffer, SelectionRequest, SendSelection,
    SetSelection,
};
use nitro_wire::types::{DataSource, NodeId, caps};

use crate::error::Error;
use crate::ui::{TimerId, Ui};

/// UTF-8 text, the type every text copy offers first.
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
/// Plain text without a charset, offered second for older readers.
pub const PLAIN_MIME: &str = "text/plain";
/// A list of URIs (RFC 2483), one per CRLF-terminated line.
pub const URI_LIST_MIME: &str = "text/uri-list";
/// The largest transfer [`Ui::read_clipboard`] accepts, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
/// How long a read may take before it is abandoned and answered `None`.
pub const READ_TIMEOUT_MS: u64 = 5_000;
/// Requests the server keeps outstanding per client; the next one would
/// be answered at EOF anyway, so it is answered `None` locally.
pub const MAX_OUTSTANDING: usize = 16;

/// What a clipboard read delivers: the MIME type read and its bytes, or
/// `None` when there was nothing (no offer, no matching type, an empty or
/// failed transfer, a timeout).
pub type Contents = Option<(String, Vec<u8>)>;

type ReadFn<S> = Box<dyn FnOnce(&mut S, &mut Ui<S>, Contents)>;

/// One `RequestSelection` of ours, from the request to the end of the read.
struct Request<S> {
    /// Our id, echoed in `SelectionData`.
    id: u32,
    mime: String,
    /// `None` once the caller has been answered (a timeout), while the
    /// server still holds the id: reusing it would be fatal.
    cb: Option<ReadFn<S>>,
    timer: Option<TimerId>,
    /// The descriptor, once `SelectionData` arrived; non-blocking.
    fd: Option<OwnedFd>,
    /// The event-loop token the descriptor is registered under.
    token: u64,
    buf: Vec<u8>,
}

/// The clipboard state a [`Ui`] carries.
pub(crate) struct Clipboard<S> {
    /// Whether `DATA` was listed in `ClientCaps`.
    pub(crate) enabled: bool,
    /// The types on offer: the last `SelectionOffer`, or the local store.
    offer: Vec<String>,
    /// What this app serves while it owns the selection.
    served: Vec<(String, Vec<u8>)>,
    /// `SelectionOffer`s still expected as the echo of our own
    /// `SetSelection`. An offer arriving with none expected means someone
    /// else took the selection.
    echoes: u32,
    /// The window of ours holding keyboard focus, if any.
    pub(crate) focus: Option<NodeId>,
    next_id: u32,
    requests: Vec<Request<S>>,
}

impl<S> Clipboard<S> {
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            offer: Vec::new(),
            served: Vec::new(),
            echoes: 0,
            focus: None,
            next_id: 0,
            requests: Vec::new(),
        }
    }

    /// Descriptors still being read, for the event loop's `epoll` set.
    pub(crate) fn fds(&self) -> impl Iterator<Item = (u64, BorrowedFd<'_>)> {
        self.requests
            .iter()
            .filter_map(|r| r.fd.as_ref().map(|fd| (r.token, fd.as_fd())))
    }

    pub(crate) fn owns_token(&self, token: u64) -> bool {
        self.requests
            .iter()
            .any(|r| r.fd.is_some() && r.token == token)
    }
}

/// How far a read got.
enum Progress {
    /// Would block; more later.
    Pending,
    /// At EOF: done.
    Eof,
    /// An error, or past the size cap.
    Failed,
}

/// Read what is available from a non-blocking descriptor.
fn read_some(fd: &OwnedFd, buf: &mut Vec<u8>) -> Progress {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match rustix::io::read(fd, &mut chunk) {
            Ok(0) => return Progress::Eof,
            Ok(n) => {
                if buf.len() + n > MAX_CLIPBOARD_BYTES {
                    return Progress::Failed;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(rustix::io::Errno::AGAIN) => return Progress::Pending,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return Progress::Failed,
        }
    }
}

/// Keep only MIME types the server accepts (non-empty, ASCII, short),
/// first occurrence wins.
pub(crate) fn clean_items(items: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::with_capacity(items.len());
    for (mime, bytes) in items {
        let ok = !mime.is_empty() && mime.len() <= 255 && mime.is_ascii();
        if ok && !out.iter().any(|(m, _)| *m == mime) {
            out.push((mime, bytes));
        }
    }
    out.truncate(32);
    out
}

impl<S: 'static> Ui<S> {
    /// Whether the system clipboard is reachable: the server has `DATA`
    /// and this app listed it. Without it the clipboard is app-local.
    #[must_use]
    pub fn has_clipboard(&self) -> bool {
        self.clipboard.enabled && self.wire().conn().has_caps(caps::DATA)
    }

    /// Whether one of this app's windows holds keyboard focus, as the
    /// server last said.
    #[must_use]
    pub fn has_keyboard_focus(&self) -> bool {
        self.clipboard.focus.is_some()
    }

    /// Put `items` on the clipboard: `(mime, bytes)` pairs, most
    /// preferred first. Empty clears the selection.
    ///
    /// Answers `Ok(false)`, sending nothing, when no window of this app
    /// holds keyboard focus: the server refuses a background clipboard
    /// write, fatally. Without `DATA` the items go to the app-local
    /// clipboard and this is always `Ok(true)`.
    ///
    /// # Errors
    /// A wire failure.
    pub fn set_clipboard(&mut self, items: Vec<(String, Vec<u8>)>) -> Result<bool, Error> {
        let items = clean_items(items);
        let mimes: Vec<String> = items.iter().map(|(m, _)| m.clone()).collect();
        if !self.has_clipboard() {
            self.clipboard.offer = mimes;
            self.clipboard.served = items;
            return Ok(true);
        }
        if self.clipboard.focus.is_none() {
            return Ok(false);
        }
        self.wire_mut()
            .send_now(&ClientMsg::SetSelection(SetSelection {
                mimes: mimes.clone(),
            }))?;
        self.clipboard.echoes += 1;
        // Eagerly, so a paste right behind the copy sees it before the
        // echo arrives.
        self.clipboard.offer = mimes;
        self.clipboard.served = items;
        Ok(true)
    }

    /// Put `text` on the clipboard as [`TEXT_MIME`] and [`PLAIN_MIME`].
    ///
    /// # Errors
    /// As [`Ui::set_clipboard`].
    pub fn set_clipboard_text(&mut self, text: &str) -> Result<bool, Error> {
        let bytes = text.as_bytes().to_vec();
        self.set_clipboard(vec![
            (TEXT_MIME.to_owned(), bytes.clone()),
            (PLAIN_MIME.to_owned(), bytes),
        ])
    }

    /// The MIME types currently on offer, most preferred first; empty
    /// when there is nothing to paste.
    #[must_use]
    pub fn clipboard_mimes(&self) -> &[String] {
        &self.clipboard.offer
    }

    /// Read the clipboard in the first of `wanted` that is on offer.
    ///
    /// `cb` runs later, never from inside this call, with the type read
    /// and its bytes, or `None` (nothing matching, an empty or failed
    /// transfer, a timeout after [`READ_TIMEOUT_MS`]). See the module docs.
    pub fn read_clipboard(
        &mut self,
        wanted: &[&str],
        cb: impl FnOnce(&mut S, &mut Ui<S>, Contents) + 'static,
    ) {
        let mime = wanted
            .iter()
            .find(|w| self.clipboard.offer.iter().any(|m| m == **w))
            .map(|m| (*m).to_owned());
        let Some(mime) = mime else {
            self.defer(move |s, ui| cb(s, ui, None));
            return;
        };
        if !self.has_clipboard() {
            let got = self
                .clipboard
                .served
                .iter()
                .find(|(m, _)| *m == mime)
                .filter(|(_, b)| !b.is_empty())
                .map(|(m, b)| (m.clone(), b.clone()));
            self.defer(move |s, ui| cb(s, ui, got));
            return;
        }
        self.read_selection(DataSource::Clipboard, mime, cb);
    }

    /// Send a `RequestSelection` for `source` in `mime` and track the
    /// read: the same id allocation, cap, timeout and non-blocking
    /// descriptor handling for the clipboard and a drop (`crate::dnd`).
    /// `cb` is always called exactly once, never from inside this call.
    pub(crate) fn read_selection(
        &mut self,
        source: DataSource,
        mime: String,
        cb: impl FnOnce(&mut S, &mut Ui<S>, Contents) + 'static,
    ) {
        let waiting = self
            .clipboard
            .requests
            .iter()
            .filter(|r| r.fd.is_none())
            .count();
        if waiting >= MAX_OUTSTANDING {
            self.defer(move |s, ui| cb(s, ui, None));
            return;
        }
        // A wrapping id that skips every one still in use: reusing an
        // outstanding id is fatal.
        let mut id = self.clipboard.next_id;
        loop {
            id = id.wrapping_add(1);
            if !self.clipboard.requests.iter().any(|r| r.id == id) {
                break;
            }
        }
        self.clipboard.next_id = id;
        if let Err(e) = self
            .wire_mut()
            .send_now(&ClientMsg::RequestSelection(RequestSelection {
                request: id,
                source,
                mime: mime.clone(),
            }))
        {
            eprintln!("nitro-ui: selection request: {e}");
            self.defer(move |s, ui| cb(s, ui, None));
            return;
        }
        let timer = self.set_timer(READ_TIMEOUT_MS, move |s, ui| {
            ui.clipboard_timeout(s, id);
        });
        self.clipboard.requests.push(Request {
            id,
            mime,
            cb: Some(Box::new(cb)),
            timer: Some(timer),
            fd: None,
            token: 0,
            buf: Vec::new(),
        });
    }

    /// Reads still in flight, answered or not.
    #[must_use]
    pub fn clipboard_reads_pending(&self) -> usize {
        self.clipboard
            .requests
            .iter()
            .filter(|r| r.cb.is_some())
            .count()
    }

    // -- dispatch -------------------------------------------------------

    pub(crate) fn clipboard_offer(&mut self, o: &SelectionOffer) {
        let c = &mut self.clipboard;
        if c.echoes > 0 {
            c.echoes -= 1;
        } else {
            // Someone else owns the selection now.
            c.served.clear();
        }
        c.offer.clone_from(&o.mimes);
    }

    pub(crate) fn clipboard_serve(&mut self, r: &SelectionRequest) {
        let bytes: &[u8] = if r.source == DataSource::Clipboard {
            self.clipboard
                .served
                .iter()
                .find(|(m, _)| *m == r.mime)
                .map_or(&[], |(_, b)| b.as_slice())
        } else {
            // A drop target reading our drag (`crate::dnd`); empty once
            // it has finished.
            self.drag_bytes(&r.mime)
        };
        // A zero-length memfd is at EOF: "cannot serve that".
        let fd = match nitro_shm::memfd_sealed_readonly("nitro-ui-clipboard", bytes) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("nitro-ui: clipboard memfd: {e}");
                return;
            }
        };
        if let Err(e) = self
            .wire_mut()
            .send_now(&ClientMsg::SendSelection(SendSelection {
                request: r.request,
                fd,
            }))
        {
            eprintln!("nitro-ui: clipboard answer: {e}");
        }
    }

    pub(crate) fn clipboard_data(&mut self, d: &SelectionData) {
        let Some(i) = self
            .clipboard
            .requests
            .iter()
            .position(|r| r.id == d.request && r.fd.is_none())
        else {
            return;
        };
        if self.clipboard.requests[i].cb.is_none() {
            // Timed out already; the server has now let go of the id.
            self.clipboard.requests.remove(i);
            return;
        }
        let fd = rustix::io::fcntl_dupfd_cloexec(d.fd.as_fd(), 0).and_then(|fd| {
            let flags = rustix::fs::fcntl_getfl(&fd)?;
            rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
            Ok(fd)
        });
        match fd {
            Ok(fd) => {
                let token = self.alloc_fd_token();
                let r = &mut self.clipboard.requests[i];
                r.fd = Some(fd);
                r.token = token;
            }
            Err(e) => {
                eprintln!("nitro-ui: clipboard descriptor: {e}");
                // An empty "read" that fails at once on the next poll.
                let r = self.clipboard.requests.remove(i);
                if let Some(t) = r.timer {
                    self.cancel_timer(&t);
                }
                if let Some(cb) = r.cb {
                    self.defer(move |s, ui| cb(s, ui, None));
                }
            }
        }
    }

    /// Advance every clipboard read that has a descriptor, answering the
    /// ones that finished. Never blocks. [`Ui::pump`] calls it, and the
    /// app loop when a clipboard descriptor becomes readable.
    pub fn poll_clipboard(&mut self, state: &mut S) {
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.clipboard.requests.len() {
            let r = &mut self.clipboard.requests[i];
            let Some(fd) = &r.fd else {
                i += 1;
                continue;
            };
            match read_some(fd, &mut r.buf) {
                Progress::Pending => i += 1,
                Progress::Eof => done.push((self.clipboard.requests.remove(i), true)),
                Progress::Failed => done.push((self.clipboard.requests.remove(i), false)),
            }
        }
        for (mut r, ok) in done {
            if let Some(fd) = r.fd.take() {
                self.retire_fd(r.token, fd);
            }
            if let Some(t) = r.timer {
                self.cancel_timer(&t);
            }
            let got = (ok && !r.buf.is_empty()).then_some((r.mime, r.buf));
            if let Some(cb) = r.cb {
                cb(state, self, got);
            }
        }
    }

    /// A read's deadline passed: answer `None` and drop the descriptor.
    fn clipboard_timeout(&mut self, state: &mut S, id: u32) {
        let Some(i) = self.clipboard.requests.iter().position(|r| r.id == id) else {
            return;
        };
        let r = &mut self.clipboard.requests[i];
        r.timer = None;
        let cb = r.cb.take();
        if r.fd.is_some() {
            // The server let go of the id when it answered.
            let mut r = self.clipboard.requests.remove(i);
            if let Some(fd) = r.fd.take() {
                self.retire_fd(r.token, fd);
            }
        }
        if let Some(cb) = cb {
            cb(state, self, None);
        }
    }
}
