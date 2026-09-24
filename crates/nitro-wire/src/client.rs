//! The client side: [`Connection`] (handshake, send, poll) and the
//! [`Transaction`] builder that writes mutations straight into the send
//! buffer.

use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::path::Path;

use nitro_core::{Color, IRect, Rect, Size, Transform};

use crate::VERSION;
use crate::codec::{FdQueue, Writer};
use crate::endpoint::Endpoint;
use crate::error::Error;
use crate::framing::Framer;
use crate::io::Socket;
use crate::msg::{
    AcceptDrop, BindKey, BufferDamage, ClientCaps, ClientMsg, CloseWindow, Commit, CreateBuffer,
    CreateNode, CreatePopup, CreateWindow, DestroyBuffer, DestroyNode, Fill, FinishDrag,
    FocusWindow, GrabKeyboard, Hello, ListOutputs, MeasureText, Outputs, Reparent, RepositionPopup,
    RequestFrame, RequestSelection, SendSelection, ServerMsg, SetAnchor, SetAppId, SetBorder,
    SetBounds, SetClip, SetCorners, SetCursor, SetExclusiveZone, SetFill, SetIcon, SetImage,
    SetLayer, SetOpacity, SetSelection, SetText, SetTransform, SetVisible, SetWindowLimits,
    SetWindowState, SetWindowStateFor, SetWindowTitle, StartDrag, StartMove, StartResize,
    UnbindKey, WindowList,
};
use crate::types::{
    Align, BufferId, CursorShape, DataSource, DragAction, Edge, Layer, NodeId, NodeKind,
    PopupAnchor, PopupGravity, WindowRef, WindowState, caps,
};

/// Where the shell socket lives; see [`crate::shell_socket_path`].
pub use crate::shell_socket_path;
/// Where the wire socket lives; see [`crate::socket_path`].
pub use crate::socket_path;

/// A connected, handshaken client.
///
/// Sending is buffered: [`Connection::send`] appends to the outgoing
/// buffer and [`Connection::flush`] pushes it at the socket. Receiving is
/// non-blocking: [`Connection::poll`] appends whatever has arrived to a
/// caller-owned `Vec`, so the event loop owns all the allocation.
///
/// Mutations go through [`Connection::tx`]; the one thing that does not is
/// [`Connection::measure_text`], which is a request answered at once
/// rather than at a commit. Both text ops are accepted whether or not the
/// server reported the `TEXT` capability; the bit
/// ([`has_caps(caps::TEXT)`](Connection::has_caps)) tells you whether the
/// text will actually be visible, not whether you may send it.
#[derive(Debug)]
pub struct Connection {
    socket: Socket,
    out: Writer,
    framer: Framer,
    /// Server name from `Welcome`.
    server_name: String,
    /// Capability bits from `Welcome`.
    caps: u32,
    /// Set once the peer hung up; reported after the last message.
    closed: bool,
    /// Every byte written to the socket since [`Connection::record_sent`]
    /// turned recording on. `None` (the default) costs nothing.
    sent: Option<Vec<u8>>,
}

impl Connection {
    /// Connect to the default socket path and run the handshake.
    ///
    /// Since M4-E1 the "path" may be a **remote** endpoint: a
    /// `NITRO_SOCKET` of `tcp://host:port` connects over TCP and the
    /// server's `Welcome` carries [`caps::REMOTE`](crate::types::caps::REMOTE).
    /// Nothing else about the connection differs, except that a message
    /// carrying a file descriptor cannot be sent on it
    /// ([`Error::RemoteNoFds`]) — see `docs/remote.md`.
    ///
    /// # Errors
    /// Connection failure, an unusable `NITRO_SOCKET`, a version
    /// mismatch, or a server that answered with
    /// [`Error`](crate::msg::Error).
    pub fn connect_default(name: &str) -> Result<Self, Error> {
        Self::connect_endpoint(&crate::endpoint()?, name)
    }

    /// Connect to `endpoint` and run the handshake.
    ///
    /// # Errors
    /// As [`Connection::connect`].
    pub fn connect_endpoint(endpoint: &Endpoint, name: &str) -> Result<Self, Error> {
        let socket = match endpoint {
            Endpoint::Unix(path) => Socket::connect(path)?,
            Endpoint::Tcp(addrs) => Socket::connect_tcp(addrs)?,
        };
        Self::with_socket(socket, name)
    }

    /// Connect to the default **shell** socket path and run the handshake.
    ///
    /// A connection made here is privileged: the `Welcome` carries
    /// [`caps::SHELL`](crate::types::caps::SHELL) and the shell ops are
    /// accepted. See `docs/shell.md`.
    ///
    /// **Never remote.** A `tcp://` in `NITRO_SHELL_SOCKET` is an error,
    /// not a connection: the privilege comes from having opened a `0700`
    /// path, and a TCP port proves nothing of the sort.
    ///
    /// # Errors
    /// As [`Connection::connect`], plus [`Error::BadEndpoint`] for a
    /// `tcp://` shell socket. In particular, a server that is not
    /// running — or one whose shell socket the caller cannot open — fails
    /// here rather than silently downgrading to an unprivileged connection.
    pub fn connect_shell(name: &str) -> Result<Self, Error> {
        Self::connect(&crate::shell_endpoint()?, name)
    }

    /// Connect to `path` and run the handshake.
    ///
    /// Blocks only for the handshake — the socket is non-blocking and the
    /// `Welcome` is waited for with `poll(2)`.
    ///
    /// # Errors
    /// Connection failure, a version mismatch, or a server that answered
    /// with [`Error`](crate::msg::Error).
    pub fn connect(path: &Path, name: &str) -> Result<Self, Error> {
        Self::with_socket(Socket::connect(path)?, name)
    }

    /// Run the handshake over an already-connected socket.
    ///
    /// # Errors
    /// As [`Connection::connect`].
    pub fn with_socket(socket: Socket, name: &str) -> Result<Self, Error> {
        let mut conn = Self {
            socket,
            out: Writer::new(),
            framer: Framer::new(),
            server_name: String::new(),
            caps: 0,
            closed: false,
            sent: None,
        };
        conn.send(&ClientMsg::Hello(Hello {
            version: VERSION,
            name: name.to_owned(),
        }))?;
        conn.flush_blocking()?;
        match conn.recv_blocking()? {
            ServerMsg::Welcome(w) => {
                if w.version != VERSION {
                    return Err(Error::Version {
                        ours: VERSION,
                        theirs: w.version,
                    });
                }
                conn.server_name = w.name;
                conn.caps = w.caps;
                Ok(conn)
            }
            ServerMsg::Error(e) => Err(Error::Rejected {
                code: e.code,
                msg: e.msg,
            }),
            other => Err(Error::Unexpected(other.name())),
        }
    }

    /// Server name reported in `Welcome`.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Capability bits from `Welcome`; see [`caps`](crate::types::caps).
    #[must_use]
    pub fn caps(&self) -> u32 {
        self.caps
    }

    /// Whether every capability bit in `bits` is set.
    #[must_use]
    pub fn has_caps(&self, bits: u32) -> bool {
        self.caps & bits == bits
    }

    /// Whether this connection is remote — the socket is TCP.
    ///
    /// The *transport's* answer, which is the one a sender needs before
    /// it builds a frame with a descriptor in it. The server's
    /// [`caps::REMOTE`](crate::types::caps::REMOTE) bit says the same
    /// thing from the other end, and the two agree; a client that trusts
    /// only the capability bit would still be right, but would not know
    /// until after the handshake.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.socket.is_remote()
    }

    /// The socket descriptor, for `epoll`.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }

    /// Whether bytes are still waiting to be written.
    #[must_use]
    pub fn has_pending_writes(&self) -> bool {
        !self.out.is_empty()
    }

    /// Queue a message. Nothing reaches the socket until [`flush`].
    ///
    /// [`flush`]: Connection::flush
    ///
    /// # Errors
    /// [`Error::Encode`] for a message that cannot be represented, or
    /// [`Error::RemoteNoFds`] for a message that needs a file descriptor
    /// on a **remote** connection.
    ///
    /// The remote refusal happens *here*, before the message is encoded,
    /// and that placement is the whole point: nothing is queued, nothing
    /// is written, and the connection is exactly as it was. A frame whose
    /// header declares descriptors that can never arrive would leave the
    /// receiver waiting for bytes that do not exist — a desynchronised
    /// stream is a dead connection, and "your image did not upload" is a
    /// much better outcome than that.
    pub fn send(&mut self, msg: &ClientMsg) -> Result<(), Error> {
        if self.socket.is_remote() && crate::server::needs_fd(msg.op()) {
            return Err(Error::RemoteNoFds);
        }
        msg.encode(&mut self.out)?;
        Ok(())
    }

    /// Queue a [`Commit`], ending the current transaction.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn commit(&mut self, serial: u32) -> Result<(), Error> {
        self.send(&ClientMsg::Commit(Commit { serial }))
    }

    /// Bind a server-global hotkey (needs `caps::SHELL`).
    ///
    /// `mods` is a [`mod_mask`](crate::types::mod_mask) bitmask; `keysym` 0
    /// asks for the bare-modifier tap. Answered with
    /// [`HotKey`](crate::msg::HotKey) events, not
    /// with an acknowledgement — a refusal is a fatal `Error`, like every
    /// other protocol error.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn bind_key(&mut self, id: u32, mods: u32, keysym: u32) -> Result<(), Error> {
        self.send(&ClientMsg::BindKey(BindKey { id, mods, keysym }))
    }

    /// Release a hotkey binding (needs `caps::SHELL`).
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn unbind_key(&mut self, id: u32) -> Result<(), Error> {
        self.send(&ClientMsg::UnbindKey(UnbindKey { id }))
    }

    /// Ask for the window list and subscribe to its changes (needs
    /// `caps::SHELL`).
    ///
    /// The answer is one [`WindowInfo`](crate::msg::WindowInfo) per window
    /// then a [`WindowListEnd`](crate::msg::WindowListEnd); afterwards
    /// changes arrive unasked.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn window_list(&mut self) -> Result<(), Error> {
        self.send(&ClientMsg::WindowList(WindowList))
    }

    /// Ask for the output list and subscribe to hotplug (needs
    /// `caps::SHELL`).
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn outputs(&mut self) -> Result<(), Error> {
        self.send(&ClientMsg::Outputs(Outputs))
    }

    /// Declare which server→client messages this client understands
    /// (M5-A).
    ///
    /// Send this once after the handshake, naming the subset of
    /// [`Connection::caps`] whose server→client messages you are ready to
    /// decode. The server must not push a message belonging to a bit you
    /// did not list, and a message you do not know would be a fatal
    /// [`DecodeError::UnknownOp`](crate::DecodeError::UnknownOp) in
    /// [`Connection::poll`] — which is exactly what this exists to
    /// prevent. See [`ClientCaps`] for the full rules.
    ///
    /// **The discovery rule is enforced here, not merely documented.**
    /// The op is behind no capability bit of its own, so sending it to a
    /// pre-M5-A server would earn an `UnknownOp` and kill the connection
    /// — the failure mode it was invented to prevent, running backwards.
    /// So this method sends nothing, and returns `Ok(())`, unless
    /// `Welcome.caps` carried at least one bit in
    /// [`caps::CAPS_M5_MASK`](crate::types::caps::CAPS_M5_MASK): a server
    /// advertising one of those necessarily knows the op, because the bits
    /// and the op arrived in the same change. A hand-rolled client that
    /// sends the frame anyway still dies, which is correct.
    ///
    /// `bits` is masked to what the server actually advertised: claiming
    /// to understand a message the server never offered is
    /// `Error { Protocol }`.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn client_caps(&mut self, bits: u32) -> Result<(), Error> {
        if self.caps & caps::CAPS_M5_MASK == 0 {
            return Ok(());
        }
        self.send(&ClientMsg::ClientCaps(ClientCaps {
            caps: bits & self.caps,
        }))
    }

    /// Ask for a named cursor shape (needs `caps::CURSOR`).
    ///
    /// Names no window on purpose: the cursor belongs to the *pointer*,
    /// and the server authorizes by asking whether this client holds
    /// pointer focus **now**. A client that does not is silently ignored
    /// rather than disconnected.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn set_cursor(&mut self, shape: CursorShape) -> Result<(), Error> {
        self.send(&ClientMsg::SetCursor(SetCursor { shape }))
    }

    /// Start an interactive window move (needs `caps::DRAG`).
    ///
    /// Honoured only while this client holds pointer focus with a button
    /// down; otherwise ignored.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn start_move(&mut self, window: NodeId) -> Result<(), Error> {
        self.send(&ClientMsg::StartMove(StartMove { window }))
    }

    /// Start an interactive window resize (needs `caps::DRAG`).
    ///
    /// `edges` is a [`resize_edges`](crate::types::resize_edges) bitmask;
    /// two bits are a corner and 0 lets the server choose. Same
    /// authorization as [`Connection::start_move`].
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn start_resize(&mut self, window: NodeId, edges: u8) -> Result<(), Error> {
        self.send(&ClientMsg::StartResize(StartResize { window, edges }))
    }

    /// Ask for the output list as an unprivileged client and subscribe to
    /// hotplug (needs `caps::OUTPUTS`).
    ///
    /// The answer is one [`OutputInfo`](crate::msg::OutputInfo) and one
    /// [`OutputWorkArea`](crate::msg::OutputWorkArea) per output, then an
    /// [`OutputsEnd`](crate::msg::OutputsEnd) — the *shell* block's
    /// messages, reachable here without the shell socket. The snapshot is
    /// complete at `OutputsEnd`.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn list_outputs(&mut self) -> Result<(), Error> {
        self.send(&ClientMsg::ListOutputs(ListOutputs))
    }

    /// Offer the clipboard selection (needs `caps::DATA`).
    ///
    /// Declares MIME types, never bytes: a paste earns a
    /// [`SelectionRequest`](crate::msg::SelectionRequest) that this client
    /// answers with [`Connection::send_selection`]. An empty `mimes`
    /// clears the selection.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn set_selection(&mut self, mimes: &[String]) -> Result<(), Error> {
        self.send(&ClientMsg::SetSelection(SetSelection {
            mimes: mimes.to_vec(),
        }))
    }

    /// Ask to read a selection in one MIME type (needs `caps::DATA`).
    ///
    /// `request` is the caller's own id, echoed in the
    /// [`SelectionData`](crate::msg::SelectionData) that answers it, and
    /// must not repeat one still outstanding. Exactly one answer always
    /// arrives — a failure is a descriptor already at EOF, not a silence
    /// — so no timeout is needed; read it **non-blocking**.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn request_selection(
        &mut self,
        request: u32,
        source: DataSource,
        mime: &str,
    ) -> Result<(), Error> {
        self.send(&ClientMsg::RequestSelection(RequestSelection {
            request,
            source,
            mime: mime.to_owned(),
        }))
    }

    /// Answer a [`SelectionRequest`](crate::msg::SelectionRequest) with a
    /// readable descriptor (needs `caps::DATA`).
    ///
    /// `request` is the **server's** id from the message being answered.
    /// `fd` is a sealed memfd or the read end of a pipe; one already at
    /// EOF means "I cannot serve that MIME type".
    ///
    /// # Errors
    /// As [`Connection::send`] — including [`Error::RemoteNoFds`], since
    /// this message carries a descriptor.
    pub fn send_selection(&mut self, request: u32, fd: OwnedFd) -> Result<(), Error> {
        self.send(&ClientMsg::SendSelection(SendSelection { request, fd }))
    }

    /// Start a drag (needs `caps::DATA`).
    ///
    /// Takes the whole [`StartDrag`] message rather than four loose
    /// arguments; build it with struct literal syntax. Honoured only while
    /// this client holds pointer focus with a button down.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn start_drag(&mut self, drag: StartDrag) -> Result<(), Error> {
        self.send(&ClientMsg::StartDrag(drag))
    }

    /// Say what this drop target would do with the offer over it (needs
    /// `caps::DATA`).
    ///
    /// An empty `mime` or [`DragAction::None`] rejects it.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn accept_drop(&mut self, action: DragAction, mime: &str) -> Result<(), Error> {
        self.send(&ClientMsg::AcceptDrop(AcceptDrop {
            action,
            mime: mime.to_owned(),
        }))
    }

    /// End a drag this client started, after its
    /// [`DragFinished`](crate::msg::DragFinished) (needs `caps::DATA`).
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn finish_drag(&mut self) -> Result<(), Error> {
        self.send(&ClientMsg::FinishDrag(FinishDrag))
    }

    /// Give keyboard focus to another client's window (needs `caps::SHELL`).
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn focus_window(&mut self, window: WindowRef) -> Result<(), Error> {
        self.send(&ClientMsg::FocusWindow(FocusWindow { window }))
    }

    /// Ask another client's window to close (needs `caps::SHELL`).
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn close_window(&mut self, window: WindowRef) -> Result<(), Error> {
        self.send(&ClientMsg::CloseWindow(CloseWindow { window }))
    }

    /// Put another client's window into a state (needs `caps::SHELL`).
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn set_window_state_for(
        &mut self,
        window: WindowRef,
        state: WindowState,
    ) -> Result<(), Error> {
        self.send(&ClientMsg::SetWindowStateFor(SetWindowStateFor {
            window,
            state,
        }))
    }

    /// Ask the server to measure a string; the answer is a
    /// [`TextMeasured`](crate::msg::TextMeasured) with the same
    /// `request`, and it is NOT tied to a commit.
    ///
    /// Always accepted: a server without the `TEXT` capability
    /// ([`has_caps(caps::TEXT)`](Connection::has_caps)) answers a
    /// well-formed zero-width measurement. The bit tells you whether text
    /// would be visible, i.e. whether it is worth laying out for.
    ///
    /// # Errors
    /// As [`Connection::send`].
    pub fn measure_text(&mut self, m: MeasureText) -> Result<(), Error> {
        self.send(&ClientMsg::MeasureText(m))
    }

    /// Push queued bytes at the socket.
    ///
    /// Returns `true` when everything was written; `false` means the
    /// socket is full and the caller should wait for writability and call
    /// `flush` again.
    ///
    /// # Errors
    /// [`Error::Closed`] or the underlying errno.
    pub fn flush(&mut self) -> Result<bool, Error> {
        let Some(sent) = self.sent.as_mut() else {
            return self.socket.send_all(&mut self.out);
        };
        // What went is the queue's prefix: a partial write leaves the
        // unsent suffix queued, and it is recorded by the flush that
        // sends it.
        let queued = self.out.bytes().to_vec();
        let result = self.socket.send_all(&mut self.out);
        let written = queued.len() - self.out.bytes().len();
        sent.extend_from_slice(&queued[..written]);
        result
    }

    /// Turn recording of every byte this connection writes on or off,
    /// clearing what was recorded.
    ///
    /// A test facility. Every message a client sends, including the
    /// synchronous measure requests, leaves through [`Connection::flush`],
    /// so this is the one place that can answer "did this string ever
    /// leave the process?". `nitro-ui`'s secret text field is the case
    /// that needs the answer.
    pub fn record_sent(&mut self, on: bool) {
        self.sent = on.then(Vec::new);
    }

    /// The bytes written since [`Connection::record_sent`] turned
    /// recording on: empty when it is off.
    #[must_use]
    pub fn sent_bytes(&self) -> &[u8] {
        self.sent.as_deref().unwrap_or(&[])
    }

    /// Start a transaction builder writing into the send buffer.
    #[must_use]
    pub fn tx(&mut self) -> Transaction<'_> {
        Transaction {
            conn: self,
            error: None,
        }
    }

    /// Whether the peer has closed its end. Messages already received are
    /// still delivered by [`Connection::poll`] first.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Append every message that has arrived to `out`, without blocking.
    ///
    /// Returns the number of messages appended. A peer hangup is reported
    /// *after* everything it sent before closing: the call that drains the
    /// last message succeeds, and the next one returns [`Error::Closed`].
    ///
    /// Like [`ClientStream::read`](crate::server::ClientStream::read) this
    /// stops after roughly [`READ_BUDGET`](crate::server::READ_BUDGET)
    /// bytes so a flood cannot hold the caller's event loop; frames are
    /// decoded each pass, so nothing is left buffered unnecessarily. The
    /// socket stays readable and the next wakeup continues.
    ///
    /// It also applies the same [`MAX_PENDING_FDS`](crate::MAX_PENDING_FDS)
    /// rule, for the same reason and with the same two halves: since M5-A
    /// the *server* sends descriptors ([`Keymap`](crate::msg::Keymap),
    /// [`SelectionData`](crate::msg::SelectionData)), so a buggy or
    /// hostile server can park them in a client's framer exactly as a
    /// client could in the server's. At the cap the loop yields **if there
    /// is a frame to drain** — decoding is what claims descriptors — and
    /// is fatal if there is not, because then they belong to no frame and
    /// never will. See `docs/wire.md` § Receive-side limits.
    ///
    /// # Errors
    /// [`Error::Closed`] on hangup, [`Error::Decode`] on a malformed
    /// frame — both fatal.
    pub fn poll(&mut self, out: &mut Vec<ServerMsg>) -> Result<usize, Error> {
        let before = out.len();
        let mut read = 0usize;
        // Drain everything already framed, then read more until EAGAIN.
        loop {
            while let Some(frame) = self.framer.next_frame()? {
                let mut fds = FdQueue::from_vec(frame.fds);
                out.push(ServerMsg::decode(frame.op, &frame.payload, &mut fds)?);
            }
            if self.closed || read >= crate::server::READ_BUDGET {
                break;
            }
            if self.framer.pending_fds() >= crate::MAX_PENDING_FDS {
                // The frames above have all been drained, so if the framer
                // still holds one it is incomplete and more bytes will
                // finish it: yield and let the next wakeup continue. If it
                // holds none, these descriptors belong to no frame and
                // never will — that is the flood, and it is fatal.
                if self.framer.has_frame() {
                    break;
                }
                return Err(Error::Decode(crate::DecodeError::UnexpectedFd));
            }
            match self.socket.recv_into(&mut self.framer) {
                Ok(Some(n)) => read += n,
                Ok(None) => break,
                Err(Error::Closed) => self.closed = true,
                Err(e) => return Err(e),
            }
        }
        let got = out.len() - before;
        if got == 0 && self.closed {
            return Err(Error::Closed);
        }
        Ok(got)
    }

    /// Write the whole send buffer, waiting for writability as needed.
    fn flush_blocking(&mut self) -> Result<(), Error> {
        while !self.flush()? {
            wait(self.socket.as_fd(), rustix::event::PollFlags::OUT)?;
        }
        Ok(())
    }

    /// Wait for and return exactly one server message.
    fn recv_blocking(&mut self) -> Result<ServerMsg, Error> {
        loop {
            if let Some(frame) = self.framer.next_frame()? {
                let mut fds = FdQueue::from_vec(frame.fds);
                return ServerMsg::decode(frame.op, &frame.payload, &mut fds).map_err(Into::into);
            }
            if self.socket.recv_into(&mut self.framer)?.is_none() {
                wait(self.socket.as_fd(), rustix::event::PollFlags::IN)?;
            }
        }
    }
}

/// Block until `fd` is ready for `events`.
fn wait(fd: BorrowedFd<'_>, events: rustix::event::PollFlags) -> Result<(), Error> {
    let mut fds = [rustix::event::PollFd::new(&fd, events)];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// A batch of mutations, ended by [`Transaction::commit`].
///
/// Every method queues one message; nothing is sent before `commit` (and
/// `commit` only writes the bytes — [`Connection::flush`] puts them on the
/// socket). The builder holds no tree state: ids are the caller's, exactly
/// as the protocol intends. An encode error is remembered and returned by
/// [`Transaction::commit`] or [`Transaction::finish`].
///
/// It covers the whole mutation vocabulary: windows, the node tree,
/// style, buffers, and text content and style through
/// [`Transaction::set_text`] — which is accepted whether or not the server
/// reports [`caps::TEXT`](crate::types::caps::TEXT)
/// ([`Connection::has_caps`]); that bit says whether the text will be
/// visible.
#[derive(Debug)]
pub struct Transaction<'a> {
    conn: &'a mut Connection,
    error: Option<Error>,
}

/// Queue `msg`, remembering the first error.
macro_rules! push {
    ($self:ident, $msg:expr) => {{
        if $self.error.is_none()
            && let Err(e) = $self.conn.send(&$msg.into())
        {
            $self.error = Some(e);
        }
        $self
    }};
}

impl Transaction<'_> {
    /// Create a top-level window.
    #[must_use]
    pub fn create_window(self, id: NodeId, title: &str, size: Size, layer: Layer) -> Self {
        self.create_window_with(id, title, size, layer, 0)
    }

    /// Create a top-level window with explicit
    /// [`window_flags`](crate::types::window_flags) bits.
    #[must_use]
    pub fn create_window_with(
        mut self,
        id: NodeId,
        title: &str,
        size: Size,
        layer: Layer,
        flags: u32,
    ) -> Self {
        push!(
            self,
            CreateWindow {
                id,
                size,
                layer,
                flags,
                title: title.to_owned(),
            }
        )
    }

    /// Ask the server to put a window into a state (needs `caps::WM`).
    #[must_use]
    pub fn set_window_state(mut self, window: NodeId, state: WindowState) -> Self {
        push!(self, SetWindowState { window, state })
    }

    /// Declare a window's content size limits; a zero component means "no
    /// limit" (needs `caps::WM`).
    #[must_use]
    pub fn set_window_limits(mut self, window: NodeId, min: Size, max: Size) -> Self {
        push!(self, SetWindowLimits { window, min, max })
    }

    /// Set a window's application id, for the shell's window list (needs
    /// `caps::WM`).
    #[must_use]
    pub fn set_app_id(mut self, window: NodeId, app_id: &str) -> Self {
        push!(
            self,
            SetAppId {
                window,
                app_id: app_id.to_owned(),
            }
        )
    }

    /// Move one of this client's windows to another stacking layer (needs
    /// `caps::SHELL`).
    #[must_use]
    pub fn set_layer(mut self, window: NodeId, layer: Layer) -> Self {
        push!(self, SetLayer { window, layer })
    }

    /// Reserve `px` logical pixels along `edge` of the window's output
    /// (needs `caps::SHELL`); `px` 0 releases the zone.
    #[must_use]
    pub fn set_exclusive_zone(mut self, window: NodeId, edge: Edge, px: u32) -> Self {
        push!(self, SetExclusiveZone { window, edge, px })
    }

    /// Anchor a window to its output's edges (needs `caps::SHELL`).
    /// `edges` is a bitmask from [`anchor`](crate::types::anchor).
    #[must_use]
    pub fn set_anchor(mut self, window: NodeId, edges: u8, margin: u32) -> Self {
        push!(
            self,
            SetAnchor {
                window,
                edges,
                margin,
            }
        )
    }

    /// Take or release a keyboard grab on one of this client's windows
    /// (needs `caps::SHELL`).
    #[must_use]
    pub fn grab_keyboard(mut self, window: NodeId, on: bool) -> Self {
        push!(self, GrabKeyboard { window, on })
    }

    /// Retitle a window.
    #[must_use]
    pub fn set_window_title(mut self, window: NodeId, title: &str) -> Self {
        push!(
            self,
            SetWindowTitle {
                window,
                title: title.to_owned(),
            }
        )
    }

    /// Ask for the next frame deadline on `window`.
    #[must_use]
    pub fn request_frame(mut self, window: NodeId) -> Self {
        push!(self, RequestFrame { window })
    }

    /// Create a node of any kind, appended to `parent`.
    #[must_use]
    pub fn create_node(mut self, id: NodeId, kind: NodeKind, parent: NodeId) -> Self {
        push!(
            self,
            CreateNode {
                id,
                kind,
                parent,
                before: NodeId::NONE,
            }
        )
    }

    /// Create a group under `parent`.
    #[must_use]
    pub fn create_group(self, id: NodeId, parent: NodeId) -> Self {
        self.create_node(id, NodeKind::Group, parent)
    }

    /// Create a rect node under `parent` with its bounds set.
    #[must_use]
    pub fn create_rect(self, id: NodeId, parent: NodeId, rect: Rect) -> Self {
        self.create_node(id, NodeKind::Rect, parent)
            .bounds(id, rect)
    }

    /// Create an image node under `parent` with its bounds set.
    #[must_use]
    pub fn create_image(self, id: NodeId, parent: NodeId, rect: Rect) -> Self {
        self.create_node(id, NodeKind::Image, parent)
            .bounds(id, rect)
    }

    /// Create an icon node under `parent` with its bounds set.
    #[must_use]
    pub fn create_icon(self, id: NodeId, parent: NodeId, rect: Rect) -> Self {
        self.create_node(id, NodeKind::Icon, parent)
            .bounds(id, rect)
    }

    /// Destroy a node and its subtree.
    #[must_use]
    pub fn destroy_node(mut self, id: NodeId) -> Self {
        push!(self, DestroyNode { id })
    }

    /// Move a node under `parent`, before `before` (or append).
    #[must_use]
    pub fn reparent(mut self, id: NodeId, parent: NodeId, before: NodeId) -> Self {
        push!(self, Reparent { id, parent, before })
    }

    /// Set a node's bounds.
    #[must_use]
    pub fn bounds(mut self, id: NodeId, rect: Rect) -> Self {
        push!(self, SetBounds { id, rect })
    }

    /// Set a group's transform.
    #[must_use]
    pub fn transform(mut self, id: NodeId, transform: Transform) -> Self {
        push!(self, SetTransform { id, transform })
    }

    /// Show or hide a subtree.
    #[must_use]
    pub fn visible(mut self, id: NodeId, visible: bool) -> Self {
        push!(self, SetVisible { id, visible })
    }

    /// Set a node's opacity.
    #[must_use]
    pub fn opacity(mut self, id: NodeId, opacity: f32) -> Self {
        push!(self, SetOpacity { id, opacity })
    }

    /// Clip a group's children to its bounds.
    #[must_use]
    pub fn clip(mut self, id: NodeId, clip: bool) -> Self {
        push!(self, SetClip { id, clip })
    }

    /// Set a node's fill.
    #[must_use]
    pub fn fill(mut self, id: NodeId, fill: Fill) -> Self {
        push!(self, SetFill { id, fill })
    }

    /// Fill a node with a solid colour.
    #[must_use]
    pub fn fill_solid(self, id: NodeId, color: Color) -> Self {
        self.fill(id, Fill::Solid(color))
    }

    /// Set a node's corner radius.
    #[must_use]
    pub fn corners(mut self, id: NodeId, radius: f32) -> Self {
        push!(self, SetCorners { id, radius })
    }

    /// Set a node's border.
    #[must_use]
    pub fn border(mut self, id: NodeId, width: f32, color: Color) -> Self {
        push!(self, SetBorder { id, width, color })
    }

    /// Set a text node's content and style.
    ///
    /// Fills in the rest of [`SetText`]: weight 400, upright, no width
    /// limit, no wrapping, [`Align::Left`]. Use
    /// [`Transaction::set_text_full`] for the other fields. Always
    /// accepted: without the `TEXT` capability the server shapes to an
    /// empty run, so the bit says whether the text will be visible.
    #[must_use]
    pub fn set_text(
        mut self,
        id: NodeId,
        family: &str,
        size_px: f32,
        color: Color,
        text: &str,
    ) -> Self {
        push!(
            self,
            SetText {
                node: id,
                size_px,
                weight: 400,
                italic: false,
                max_width: 0.0,
                wrap: false,
                align: Align::Left,
                color,
                family: family.to_owned(),
                text: text.to_owned(),
            }
        )
    }

    /// Full form: every field of [`SetText`], built with struct literal
    /// syntax.
    #[must_use]
    pub fn set_text_full(mut self, m: SetText) -> Self {
        push!(self, m)
    }

    /// Put a named symbolic icon on an `Icon` node.
    ///
    /// The client never sends pixels: the server owns the artwork and
    /// rasterises it at the output's scale, tinted with the palette role
    /// `role` names. An empty `name` clears the node; an unknown one is
    /// a non-fatal `Error { BadIcon }`. See `docs/icons.md`.
    #[must_use]
    pub fn set_icon(mut self, id: NodeId, name: &str, size: f32, role: u8) -> Self {
        push!(
            self,
            SetIcon {
                node: id,
                size,
                role,
                name: name.to_owned(),
            }
        )
    }

    /// Register a shared-memory buffer, passing `fd`.
    ///
    /// Takes the whole [`CreateBuffer`] message rather than seven loose
    /// arguments; build it with struct literal syntax.
    #[must_use]
    pub fn create_buffer(mut self, buffer: CreateBuffer) -> Self {
        push!(self, buffer)
    }

    /// Release a buffer id.
    #[must_use]
    pub fn destroy_buffer(mut self, id: BufferId) -> Self {
        push!(self, DestroyBuffer { id })
    }

    /// Announce changed buffer contents.
    #[must_use]
    pub fn buffer_damage(mut self, id: BufferId, rects: Vec<IRect>) -> Self {
        push!(self, BufferDamage { id, rects })
    }

    /// Point an image node at a buffer region.
    #[must_use]
    pub fn image(mut self, id: NodeId, buffer: BufferId, src: IRect) -> Self {
        push!(self, SetImage { id, buffer, src })
    }

    /// Create a popup — a menu or tooltip anchored to a rectangle of its
    /// parent (needs `caps::POPUP`).
    ///
    /// Takes the whole [`CreatePopup`] message rather than eight loose
    /// arguments; build it with struct literal syntax, as
    /// [`Transaction::create_buffer`] and [`Transaction::set_text_full`]
    /// do.
    ///
    /// A **scene mutation**, and on the transaction for that reason: the
    /// popup must appear atomically with the content committed into it,
    /// or a menu shows one frame empty. The server answers with a
    /// [`Configure`](crate::msg::Configure) carrying the geometry it
    /// actually gave, constrained as `constraint` allows.
    #[must_use]
    pub fn create_popup(mut self, popup: CreatePopup) -> Self {
        push!(self, popup)
    }

    /// Move an existing popup to a new anchor (needs `caps::POPUP`).
    ///
    /// Answered with another [`Configure`](crate::msg::Configure). There
    /// is deliberately no reposition token — see [`RepositionPopup`].
    #[must_use]
    pub fn reposition_popup(
        mut self,
        id: NodeId,
        anchor_rect: IRect,
        anchor: PopupAnchor,
        gravity: PopupGravity,
        constraint: u32,
    ) -> Self {
        push!(
            self,
            RepositionPopup {
                id,
                anchor_rect,
                anchor,
                gravity,
                constraint,
            }
        )
    }

    /// Queue the [`Commit`] that ends this transaction.
    ///
    /// # Errors
    /// The first encode error the builder hit, if any.
    pub fn commit(mut self, serial: u32) -> Result<(), Error> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        self.conn.commit(serial)
    }

    /// End the builder without committing (the mutations stay queued and
    /// become part of the next [`Commit`]).
    ///
    /// # Errors
    /// The first encode error the builder hit, if any.
    pub fn finish(mut self) -> Result<(), Error> {
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
