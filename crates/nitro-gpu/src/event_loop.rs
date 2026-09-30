//! The helper's event loop: one socket, the in-flight completion fences,
//! and a backend.
//!
//! It never waits on the GPU. A `Composite` is validated, handed to the
//! backend, and answered with the completion `sync_file` as soon as the
//! backend returns from the submit. The loop keeps a duplicate of each
//! frame's fence and polls it next to the socket: when it becomes readable
//! the frame is done, its slot is free again and its texture references
//! drop — which is what lets a deferred `Release` finish. No timers, no
//! busy waiting.
//!
//! Capture rings (protocol v3) sit beside the output ring, keyed by the
//! server's id, each with its own buffer-age damage. `FreeCaptureRing`
//! kills the id at once; the backend frees the slots once the last frame
//! drawn into them has signalled (or at teardown, after the drain).
//!
//! Every refused request is answered with an `Error` and the loop carries
//! on. The loop ends on EOF (the server went away), `Shutdown`, an
//! unrecoverable socket error, or — with [`Config::idle_exit`] — after the
//! helper has held nothing and heard nothing for that long.

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use std::time::{Duration, Instant};

use nitro_core::IRect;
use nitro_wire::{DecodeError, Framer, Socket, Writer};
use rustix::event::{PollFd, PollFlags, Timespec};

use crate::backend::{Backend, BackendError, Readback, RingId};
use crate::lifetime::TexTable;
use crate::proto::{
    CaptureComposite, Composite, ErrorCode, FromHelper, Layer, MAX_CAPTURE_RINGS, Message,
    PROTO_VERSION, ToHelper, XR24,
};
use crate::ring::DamageRing;
use crate::stats::Counters;
use crate::validate::{self, OutInfo};

/// Loop configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct Config {
    /// On-demand mode: exit once no texture is held, no frame is in
    /// flight and no message arrived for this long. `None` (the default)
    /// is always-on.
    pub idle_exit: Option<Duration>,
}

/// Why the loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The server closed the socket.
    Eof,
    /// The server sent `Shutdown`.
    Shutdown,
    /// [`Config::idle_exit`] elapsed.
    Idle,
}

/// A fatal loop error.
#[derive(Debug)]
pub enum LoopError {
    /// The socket failed.
    Wire(nitro_wire::Error),
    /// `poll` failed.
    Poll(rustix::io::Errno),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(e) => write!(f, "socket: {e}"),
            Self::Poll(e) => write!(f, "poll: {e}"),
        }
    }
}

impl std::error::Error for LoopError {}

impl From<nitro_wire::Error> for LoopError {
    fn from(e: nitro_wire::Error) -> Self {
        Self::Wire(e)
    }
}

/// A submitted frame whose fence has not signalled yet.
#[derive(Debug)]
struct InFlight {
    /// The ring slot it draws into, while that slot counts as busy.
    /// `None` once the output ring it drew into was reallocated.
    slot: Option<(RingId, usize)>,
    texs: Vec<u32>,
    fence: OwnedFd,
}

struct Output {
    ring: DamageRing,
    info: OutInfo,
}

/// The loop's state, generic over the backend.
pub struct Helper<B: Backend> {
    backend: B,
    texs: TexTable<B::Tex>,
    out: Option<Output>,
    /// Capture rings by id (v3).
    capture_rings: HashMap<u32, Output>,
    /// Freed capture rings whose slots wait for in-flight frames.
    dying: Vec<u32>,
    in_flight: Vec<InFlight>,
    counters: Counters,
    w: Writer,
    exit: Option<Exit>,
}

impl<B: Backend> Helper<B> {
    /// Wrap a backend.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            texs: TexTable::new(),
            out: None,
            capture_rings: HashMap::new(),
            dying: Vec::new(),
            in_flight: Vec::new(),
            counters: Counters::default(),
            w: Writer::new(),
            exit: None,
        }
    }

    /// The backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Run until EOF, `Shutdown` or idle exit. Everything still held is
    /// released through the backend before returning (after the GPU is
    /// done with it: the loop drains the in-flight fences first).
    ///
    /// # Errors
    /// A socket or `poll` failure.
    pub fn run(mut self, mut sock: Socket, cfg: Config) -> Result<Exit, LoopError> {
        let mut framer = Framer::new();
        let mut last_activity = Instant::now();
        let result = loop {
            if let Some(e) = self.exit {
                break Ok(e);
            }
            let idle_left = cfg.idle_exit.and_then(|d| {
                if self.texs.is_empty()
                    && self.in_flight.is_empty()
                    && self.capture_rings.is_empty()
                    && self.dying.is_empty()
                {
                    Some(d.saturating_sub(last_activity.elapsed()))
                } else {
                    None
                }
            });
            if idle_left == Some(Duration::ZERO) {
                break Ok(Exit::Idle);
            }
            let want_out = !self.w.is_empty();
            let (sock_ready, fences_ready) = {
                let mut fds = Vec::with_capacity(1 + self.in_flight.len());
                let mut flags = PollFlags::IN;
                if want_out {
                    flags |= PollFlags::OUT;
                }
                fds.push(PollFd::from_borrowed_fd(sock.as_fd(), flags));
                for f in &self.in_flight {
                    fds.push(PollFd::new(&f.fence, PollFlags::IN));
                }
                let ts = idle_left.map(crate::timespec);
                match rustix::event::poll(&mut fds, ts.as_ref()) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => break Err(LoopError::Poll(e)),
                }
                let sock_ready = !fds[0].revents().is_empty();
                let fences: Vec<bool> = fds[1..].iter().map(|f| !f.revents().is_empty()).collect();
                (sock_ready, fences)
            };
            if fences_ready.iter().any(|b| *b) {
                self.retire(&fences_ready);
            }
            if sock_ready {
                match sock.recv_into(&mut framer) {
                    Ok(_) => {}
                    Err(nitro_wire::Error::Closed) => break Ok(Exit::Eof),
                    Err(e) => break Err(e.into()),
                }
                last_activity = Instant::now();
                loop {
                    match framer.next_frame() {
                        Ok(Some(frame)) => {
                            let op = frame.op;
                            match ToHelper::decode(frame) {
                                Ok((msg, fds)) => self.handle(msg, fds),
                                Err(e) => self.refuse(op, 0, &proto_err(e)),
                            }
                        }
                        Ok(None) => break,
                        // The stream itself is broken: nothing after this
                        // point can be framed.
                        Err(e) => return Err(nitro_wire::Error::Decode(e).into()),
                    }
                }
            }
            if !self.w.is_empty() {
                match sock.send_all(&mut self.w) {
                    Ok(_) => {}
                    Err(nitro_wire::Error::Closed) => break Ok(Exit::Eof),
                    Err(e) => break Err(e.into()),
                }
            }
        };
        self.teardown();
        result
    }

    /// Poll the in-flight fences without blocking and retire the done ones.
    fn reap(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        let ready: Vec<bool> = {
            let mut fds: Vec<PollFd<'_>> = self
                .in_flight
                .iter()
                .map(|f| PollFd::new(&f.fence, PollFlags::IN))
                .collect();
            if rustix::event::poll(
                &mut fds,
                Some(&Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
            )
            .is_err()
            {
                return;
            }
            fds.iter().map(|f| !f.revents().is_empty()).collect()
        };
        self.retire(&ready);
    }

    fn retire(&mut self, ready: &[bool]) {
        let mut done = Vec::new();
        let mut keep = Vec::with_capacity(self.in_flight.len());
        for (i, f) in std::mem::take(&mut self.in_flight).into_iter().enumerate() {
            if ready.get(i).copied().unwrap_or(false) {
                done.push(f);
            } else {
                keep.push(f);
            }
        }
        self.in_flight = keep;
        for f in done {
            let freed = self.texs.finish(&f.texs);
            self.free(freed);
        }
        self.free_dead_rings();
    }

    /// Free every dying capture ring no in-flight frame draws into.
    fn free_dead_rings(&mut self) {
        let in_flight = &self.in_flight;
        let (dead, alive): (Vec<u32>, Vec<u32>) =
            std::mem::take(&mut self.dying).into_iter().partition(|id| {
                !in_flight
                    .iter()
                    .any(|f| matches!(f.slot, Some((RingId::Capture(r), _)) if r == *id))
            });
        self.dying = alive;
        for id in dead {
            self.backend.free_ring(RingId::Capture(id));
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per message; splitting hides the dispatch
    fn handle(&mut self, msg: ToHelper, fds: Vec<OwnedFd>) {
        let op = msg.op();
        match msg {
            ToHelper::Hello { version } => {
                if version == PROTO_VERSION {
                    self.send(
                        &FromHelper::HelloReply {
                            version: PROTO_VERSION,
                            info: self.backend.info(),
                        },
                        Vec::new(),
                    );
                } else {
                    self.refuse(
                        op,
                        u64::from(version),
                        &BackendError::with_code(
                            ErrorCode::Version,
                            format!("helper speaks {PROTO_VERSION}"),
                        ),
                    );
                }
            }
            ToHelper::ImportDmabuf(d) => {
                let id = d.id;
                let r = self.check_new_id(id).and_then(|()| {
                    let info = validate::dmabuf(&d, &self.backend.info())?;
                    let t = self.backend.import_dmabuf(&d, fds)?;
                    Ok((t, info))
                });
                self.finish_import(op, id, r);
            }
            ToHelper::ImportShadow(d) => {
                let id = d.id;
                let r = self.check_new_id(id).and_then(|()| {
                    let memfd = fds
                        .into_iter()
                        .next()
                        .ok_or_else(|| BackendError::with_code(ErrorCode::Protocol, "no memfd"))?;
                    let len = nitro_shm::sealed_len(&memfd).map_err(|e| {
                        BackendError::with_code(ErrorCode::BadBuffer, e.to_string())
                    })?;
                    let info = validate::shadow(&d, len)?;
                    let t = self.backend.import_shadow(&d, memfd)?;
                    Ok((t, info))
                });
                self.finish_import(op, id, r);
            }
            ToHelper::UploadDamage { id, rects } => {
                let r = match (self.texs.info(id), self.texs.get_mut(id)) {
                    (Some(info), Some(t)) => validate::upload(info, &rects)
                        .and_then(|rects| self.backend.upload_damage(t, &rects)),
                    _ => Err(BackendError::with_code(
                        ErrorCode::BadId,
                        format!("texture {id}"),
                    )),
                };
                if let Err(e) = r {
                    self.refuse(op, u64::from(id), &e);
                }
            }
            ToHelper::AllocOutputRing {
                n,
                w,
                h,
                fourcc,
                modifiers,
            } => self.alloc_ring(op, n, w, h, fourcc, &modifiers),
            ToHelper::Composite(c) => {
                let serial = c.serial;
                if let Err(e) = self.composite(RingId::Output, &c, fds) {
                    self.refuse(op, serial, &e);
                }
            }
            ToHelper::AllocCaptureRing {
                ring_id,
                n,
                w,
                h,
                fourcc,
                modifiers,
            } => self.alloc_capture_ring(op, ring_id, (n, w, h), fourcc, &modifiers),
            ToHelper::CaptureComposite(CaptureComposite { ring_id, frame }) => {
                let serial = frame.serial;
                if let Err(e) = self.composite(RingId::Capture(ring_id), &frame, fds) {
                    self.refuse(op, serial, &e);
                }
            }
            ToHelper::FreeCaptureRing { ring_id } => {
                if self.capture_rings.remove(&ring_id).is_some() {
                    self.dying.push(ring_id);
                    self.reap();
                    self.free_dead_rings();
                } else {
                    self.refuse(
                        op,
                        u64::from(ring_id),
                        &BackendError::with_code(ErrorCode::BadRing, format!("ring {ring_id}")),
                    );
                }
            }
            ToHelper::Release { id } => match self.texs.release(id) {
                Ok(Some(t)) => {
                    self.backend.release(t);
                    self.send(&FromHelper::Released { id }, Vec::new());
                }
                Ok(None) => {}
                Err(e) => self.refuse(op, u64::from(id), &e),
            },
            ToHelper::ReadBack { out_idx } => {
                let r = match &self.out {
                    Some(o) if (out_idx as usize) < o.info.n => Ok(o.info),
                    _ => Err(BackendError::with_code(
                        ErrorCode::NoRing,
                        format!("slot {out_idx}"),
                    )),
                }
                .and_then(|info| {
                    let rb = self.backend.readback(out_idx as usize)?;
                    Ok((info, rb))
                });
                match r {
                    Ok((info, rb)) => self.send(
                        &FromHelper::ReadBackReply {
                            out_idx,
                            w: info.w,
                            h: info.h,
                            stride: rb.stride,
                        },
                        vec![rb.memfd],
                    ),
                    Err(e) => self.refuse(op, u64::from(out_idx), &e),
                }
                self.reap();
            }
            ToHelper::Capture {
                serial,
                w,
                h,
                layers,
            } => {
                match self.capture(w, h, &layers) {
                    Ok(rb) => self.send(
                        &FromHelper::Captured {
                            serial,
                            w,
                            h,
                            stride: rb.stride,
                        },
                        vec![rb.memfd],
                    ),
                    Err(e) => self.refuse(op, serial, &e),
                }
                self.reap();
            }
            ToHelper::GetStats => {
                self.reap();
                let s = self.counters.snapshot(
                    self.texs.len(),
                    self.in_flight.len(),
                    self.backend.shadow_path(),
                );
                self.send(&FromHelper::Stats(s), Vec::new());
            }
            ToHelper::Shutdown => self.exit = Some(Exit::Shutdown),
        }
    }

    fn check_new_id(&self, id: u32) -> Result<(), BackendError> {
        if self.texs.contains(id) {
            Err(BackendError::with_code(
                ErrorCode::DuplicateId,
                format!("texture {id} exists"),
            ))
        } else {
            Ok(())
        }
    }

    fn finish_import(
        &mut self,
        op: u16,
        id: u32,
        r: Result<(B::Tex, validate::TexInfo), BackendError>,
    ) {
        match r {
            Ok((t, info)) => match self.texs.insert(id, t, info) {
                Ok(()) => {
                    self.counters.imports += 1;
                    self.send(&FromHelper::Imported { id }, Vec::new());
                }
                Err((e, t)) => {
                    self.backend.release(t);
                    self.refuse(op, u64::from(id), &e);
                }
            },
            Err(e) => self.refuse(op, u64::from(id), &e),
        }
    }

    #[allow(clippy::many_single_char_names)] // n, w, h: the protocol's names
    fn alloc_ring(&mut self, op: u16, n: u32, w: u32, h: u32, fourcc: u32, mods: &[u64]) {
        let r = validate::ring(n, w, h, fourcc, mods, &self.backend.info())
            .and_then(|req| Ok((self.backend.alloc_ring(RingId::Output, &req)?, req)));
        match r {
            Ok((ring, req)) => {
                // The old slots are gone: their frames' fences still
                // retire texture references, but no slot is busy.
                for f in &mut self.in_flight {
                    if matches!(f.slot, Some((RingId::Output, _))) {
                        f.slot = None;
                    }
                }
                self.out = Some(Output {
                    ring: DamageRing::new(req.n, w, h),
                    info: OutInfo { n: req.n, w, h },
                });
                let (slots, fds): (Vec<_>, Vec<_>) = ring.slots.into_iter().unzip();
                self.send(
                    &FromHelper::OutputRing {
                        w,
                        h,
                        fourcc,
                        modifier: ring.modifier,
                        slots,
                    },
                    fds,
                );
            }
            Err(e) => self.refuse(op, u64::from(n), &e),
        }
    }

    /// A v3 capture ring: a new id, validated, allocated; answered with
    /// `CaptureRing` + one dma-buf per slot.
    #[allow(clippy::many_single_char_names)] // n, w, h: the protocol's names
    fn alloc_capture_ring(
        &mut self,
        op: u16,
        ring_id: u32,
        (n, w, h): (u32, u32, u32),
        fourcc: u32,
        mods: &[u64],
    ) {
        let r = if self.capture_rings.contains_key(&ring_id) || self.dying.contains(&ring_id) {
            Err(BackendError::with_code(
                ErrorCode::DuplicateId,
                format!("ring {ring_id} exists"),
            ))
        } else if self.capture_rings.len() >= MAX_CAPTURE_RINGS {
            Err(BackendError::with_code(
                ErrorCode::TooMany,
                format!("{MAX_CAPTURE_RINGS} capture rings"),
            ))
        } else {
            validate::capture_ring(n, w, h, fourcc, mods, &self.backend.info()).and_then(|req| {
                Ok((
                    self.backend.alloc_ring(RingId::Capture(ring_id), &req)?,
                    req,
                ))
            })
        };
        match r {
            Ok((ring, req)) => {
                self.capture_rings.insert(
                    ring_id,
                    Output {
                        ring: DamageRing::new(req.n, w, h),
                        info: OutInfo { n: req.n, w, h },
                    },
                );
                let (slots, fds): (Vec<_>, Vec<_>) = ring.slots.into_iter().unzip();
                self.send(
                    &FromHelper::CaptureRing {
                        ring_id,
                        w,
                        h,
                        fourcc: XR24,
                        modifier: ring.modifier,
                        slots,
                    },
                    fds,
                );
            }
            Err(e) => self.refuse(op, u64::from(ring_id), &e),
        }
    }

    fn ring_state(&self, ring: RingId) -> Option<&Output> {
        match ring {
            RingId::Output => self.out.as_ref(),
            RingId::Capture(id) => self.capture_rings.get(&id),
        }
    }

    fn composite(
        &mut self,
        ring: RingId,
        c: &Composite,
        fds: Vec<OwnedFd>,
    ) -> Result<(), BackendError> {
        if let RingId::Capture(id) = ring
            && !self.capture_rings.contains_key(&id)
        {
            return Err(BackendError::with_code(
                ErrorCode::BadRing,
                format!("capture ring {id}"),
            ));
        }
        validate::composite(
            c,
            |id| self.texs.info(id),
            self.ring_state(ring).map(|o| o.info),
        )?;
        if fds.len() != c.fence_count() {
            return Err(BackendError::with_code(ErrorCode::Fences, "fence count"));
        }
        let slot = c.out_idx as usize;
        let target = Some((ring, slot));
        if self.in_flight.iter().any(|f| f.slot == target) {
            self.reap();
            if self.in_flight.iter().any(|f| f.slot == target) {
                return Err(BackendError::with_code(
                    ErrorCode::Busy,
                    format!("slot {slot} still in flight"),
                ));
            }
        }
        let Some(out) = self.ring_state(ring) else {
            return Err(BackendError::with_code(ErrorCode::NoRing, "no ring"));
        };
        let damage: Vec<IRect> = c.damage.clone();
        let clip = out.ring.clip_for(slot, &damage);
        let mut layers = Vec::with_capacity(c.layers.len());
        for l in &c.layers {
            // Validated above: every id is live.
            let Some(t) = self.texs.get(l.tex) else {
                return Err(BackendError::with_code(
                    ErrorCode::BadId,
                    "texture vanished",
                ));
            };
            layers.push((t, *l));
        }
        let t0 = Instant::now();
        let fence = self.backend.composite(ring, slot, &clip, &layers, fds)?;
        self.counters.submit(t0.elapsed());
        let ids: Vec<u32> = c.layers.iter().map(|l| l.tex).collect();
        let state = match ring {
            RingId::Output => self.out.as_mut(),
            RingId::Capture(id) => self.capture_rings.get_mut(&id),
        };
        if let Some(out) = state {
            out.ring.commit(slot, &damage);
        }
        self.texs.acquire(&ids);
        if let Ok(mine) = rustix::io::fcntl_dupfd_cloexec(&fence, 0) {
            self.in_flight.push(InFlight {
                slot: target,
                texs: ids,
                fence: mine,
            });
        } else {
            // Without our own copy we cannot tell when it is done: treat
            // the frame as finished now rather than leak its references.
            let freed = self.texs.finish(&ids);
            self.free(freed);
        }
        self.send(&FromHelper::Composited { serial: c.serial }, vec![fence]);
        Ok(())
    }

    /// A screenshot: synchronous, so every texture it samples is alive
    /// until it returns.
    fn capture(&mut self, w: u32, h: u32, ls: &[Layer]) -> Result<Readback, BackendError> {
        validate::capture(w, h, ls, |id| self.texs.info(id))?;
        let mut layers = Vec::with_capacity(ls.len());
        for l in ls {
            let Some(t) = self.texs.get(l.tex) else {
                return Err(BackendError::with_code(
                    ErrorCode::BadId,
                    "texture vanished",
                ));
            };
            layers.push((t, *l));
        }
        self.backend.capture(w, h, &layers)
    }

    fn free(&mut self, freed: Vec<(u32, B::Tex)>) {
        for (id, t) in freed {
            self.backend.release(t);
            self.send(&FromHelper::Released { id }, Vec::new());
        }
    }

    fn send(&mut self, msg: &FromHelper, fds: Vec<OwnedFd>) {
        if let Err(e) = msg.encode(&mut self.w, fds) {
            // Only a bug can get here (the helper built a message over a
            // limit); say so rather than go silent.
            let _ = FromHelper::Error {
                op: msg.op(),
                what: 0,
                code: ErrorCode::Protocol,
                msg: format!("helper could not encode reply: {e}"),
            }
            .encode(&mut self.w, Vec::new());
        }
    }

    fn refuse(&mut self, op: u16, what: u64, e: &BackendError) {
        self.counters.errors += 1;
        self.send(
            &FromHelper::Error {
                op,
                what,
                code: e.code,
                msg: e.msg.clone(),
            },
            Vec::new(),
        );
    }

    fn teardown(&mut self) {
        // Wait (bounded) for the GPU before freeing what it samples.
        for f in std::mem::take(&mut self.in_flight) {
            let mut fds = [PollFd::new(&f.fence, PollFlags::IN)];
            let _ = rustix::event::poll(
                &mut fds,
                Some(&Timespec {
                    tv_sec: 2,
                    tv_nsec: 0,
                }),
            );
            let _ = self.texs.finish(&f.texs);
        }
        let rings: Vec<u32> = self
            .capture_rings
            .drain()
            .map(|(id, _)| id)
            .chain(std::mem::take(&mut self.dying))
            .collect();
        for id in rings {
            self.backend.free_ring(RingId::Capture(id));
        }
        let texs: Vec<B::Tex> = self.texs.drain().collect();
        for t in texs {
            self.backend.release(t);
        }
    }
}

fn proto_err(e: DecodeError) -> BackendError {
    BackendError::with_code(ErrorCode::Protocol, format!("{e}"))
}

/// Run `backend` on `sock` until it ends. See [`Helper::run`].
///
/// # Errors
/// A socket or `poll` failure.
pub fn run<B: Backend>(sock: Socket, backend: B, cfg: Config) -> Result<Exit, LoopError> {
    Helper::new(backend).run(sock, cfg)
}
