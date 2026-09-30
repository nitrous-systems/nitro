//! Screen recording in the server (#676 B): per-capture state, the GPU
//! helper's capture rings, pacing, and the wire ops.
//!
//! A client that listed `caps::CAPTURE` sends `CaptureStart{output,
//! max_fps}`. If [`Server::capture_permitted`] lets it (the one hook the
//! permission prompt of #676 C replaces), the server asks the helper for
//! a capture ring (`AllocCaptureRing`, 3 LINEAR-first XR24 slots of the
//! output's size) and hands the slots to the client as `CaptureBuffers`.
//! From then on, after a flip of that output (and on a release, a fence,
//! or a rate timer), when something changed and the rate allows and a
//! slot is free, the helper composites the output's shadow plus the
//! Surfaces the shadow has holes for (on a plane or drawn by the helper)
//! into the slot (`CaptureComposite`), and the client gets
//! `CaptureFrame{slot, damage, fence}`. The slot is the client's until
//! `CaptureRelease`.
//!
//! **Never waits.** No free slot drops the frame (counted); its damage is
//! kept for the next one. At most one frame per capture is in flight.
//!
//! **No shadow hazard.** The helper does not read the shadow itself but a
//! per-capture **snapshot** (a sealed memfd of the output's size, imported
//! like the mode-2 shadow): at submit the frame's damage rects are copied
//! from the shadow into it (write-only row copies, `Shadow::stream_to`),
//! and it is only written again once that frame's fence signalled. Raster
//! never waits for a recording. The first design delayed the output's
//! raster while a capture frame read the shadow; measured on box1 at
//! 3840×2160@30 it cut display fps (26.0–29.8 against 30.0), so it was
//! replaced by this copy (`docs/budget.md` § Screen recording).
//!
//! **No CPU path.** Without the helper (`gpu.helper = off`, given up, or
//! unable to start) a start is answered `CaptureStopped{Unsupported}`: a
//! CPU copy is a full-output memcpy per frame (14 MB at 1440p), which the
//! footprint rule rules out.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use nitro_core::{Damage, IRect};
use nitro_gpu::proto;
use nitro_kms::OutputId as KmsOutputId;
use nitro_scene::{BufferKey, NodeKey};
use nitro_wire::msg::{self, ServerMsg};
use nitro_wire::types::{CaptureStopReason as Reason, ErrorCode, caps, format};

use crate::{
    EpollPoll, Server, TOK_GPU, TOK_GPU_FENCE_BASE, debug, gpu, gpu_rects, info, monotonic_ns,
    planes,
};

/// Slots per capture ring: one with the client, one being drawn, one
/// spare.
pub const RING_SLOTS: u32 = proto::MIN_CAPTURE_RING as u32;
/// Captures alive at once (the helper's capture-ring limit).
pub const MAX_CAPTURES: usize = proto::MAX_CAPTURE_RINGS;

/// What one ring slot is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Nothing reads it; the serial last drawn into it, if any.
    Free(Option<u64>),
    /// A `CaptureComposite` into it is out.
    Submitted(u64),
    /// The client has it (a `CaptureFrame` went out).
    WithClient(u64),
}

/// The frame of a capture that is out (at most one).
#[derive(Debug)]
struct Reading {
    serial: u64,
    slot: usize,
    damage: Vec<IRect>,
    time_ns: u64,
    sent: Instant,
}

/// One capture.
#[derive(Debug)]
struct Capture {
    token: u64,
    id: u32,
    output: KmsOutputId,
    size: (u32, u32),
    /// Minimum frame interval, ns.
    interval_ns: u64,
    ring_id: u32,
    /// `AllocCaptureRing` is out.
    requested: bool,
    /// Empty until the ring arrived.
    slots: Vec<Slot>,
    /// The ring's bytes.
    bytes: u64,
    /// The snapshot of the output's shadow the helper samples, and its
    /// texture id there.
    snap: Option<(Snapshot, u32)>,
    /// Changed since the last frame, output pixels.
    damage: Damage,
    /// What the last frame's Surface layers showed.
    last_layers: Vec<(NodeKey, BufferKey, IRect)>,
    /// When the last frame was submitted (monotonic ns).
    last_ns: Option<u64>,
    /// A flip happened since the last frame (layers may have changed).
    flipped: bool,
    reading: Option<Reading>,
}

/// The server's captures and counters.
#[derive(Debug, Default)]
pub struct Captures {
    list: Vec<Capture>,
    next_ring: u32,
    /// Serials of frames of captures stopped while the frame was out:
    /// their answer only returns borrows.
    orphans: Vec<u64>,
    /// Counters for `stats`.
    pub stats: crate::stats::CaptureStats,
}

impl Captures {
    /// Whether no capture is alive.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Ring bytes allocated right now.
    #[must_use]
    pub fn ring_bytes(&self) -> u64 {
        self.list.iter().map(|c| c.bytes).sum()
    }
}

impl Server {
    /// Whether `token` may speak the capture ops, disconnecting it if not
    /// (`CAPTURE` not listed in `ClientCaps`; a remote link never has it).
    pub(crate) fn capture_allowed(&mut self, token: u64, name: &str) -> bool {
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        if client.client_caps & caps::CAPTURE != 0 {
            return true;
        }
        self.disconnect(
            token,
            Some((
                0,
                ErrorCode::Protocol,
                format!("{name} needs `CAPTURE` listed in ClientCaps"),
            )),
        );
        false
    }

    /// The permission hook: may this client record? Minimal gate (#676 B):
    /// `NITRO_CAPTURE_ALLOW=1` in the server's environment allows every
    /// local client; otherwise nobody. #676 C replaces this with the
    /// allow-list and the shell's prompt.
    pub(crate) fn capture_permitted(&self, token: u64) -> Result<(), Reason> {
        let _ = token;
        if self.capture_allow {
            Ok(())
        } else {
            Err(Reason::Denied)
        }
    }

    fn capture_send_stopped(&mut self, token: u64, capture_id: u32, reason: Reason) {
        debug!("capture {capture_id} of {token:#x}: stopped ({reason:?})");
        if let Some(c) = self.wire_clients.get_mut(&token) {
            c.send(&ServerMsg::CaptureStopped(msg::CaptureStopped {
                capture_id,
                reason,
            }));
        }
    }

    fn capture_check(&self, token: u64, m: &msg::CaptureStart) -> Result<usize, Reason> {
        self.capture_permitted(token)?;
        if self.lock.is_locked() {
            return Err(Reason::Locked);
        }
        let index = self
            .outputs
            .iter()
            .position(|o| o.scene_id.0 == m.output)
            .ok_or(Reason::OutputGone)?;
        if (m.format != 0 && m.format != format::XR24)
            || !self.gpu.enabled()
            || self.gpu.state == gpu::State::GaveUp
            || self.outputs[index].shadow.is_none()
            || self.captures.list.len() >= MAX_CAPTURES
        {
            return Err(Reason::Unsupported);
        }
        Ok(index)
    }

    /// `CaptureStart`, at receipt. Returns whether the client survives.
    pub(crate) fn capture_start(&mut self, token: u64, m: &msg::CaptureStart) -> bool {
        if self
            .captures
            .list
            .iter()
            .any(|c| c.token == token && c.id == m.capture_id)
        {
            self.disconnect(
                token,
                Some((
                    0,
                    ErrorCode::Protocol,
                    format!("CaptureStart: capture {} is live", m.capture_id),
                )),
            );
            return false;
        }
        let index = match self.capture_check(token, m) {
            Ok(i) => i,
            Err(r) => {
                self.capture_send_stopped(token, m.capture_id, r);
                return true;
            }
        };
        let o = &self.outputs[index];
        let fps = if m.max_fps == 0 {
            1_000_000_000 / u64::from(o.refresh_ns.max(1))
        } else {
            u64::from(m.max_fps)
        }
        .max(1);
        self.captures.next_ring = self.captures.next_ring.wrapping_add(1).max(1);
        info!(
            "capture {} of client {token:#x}: {} {}x{} at ≤{fps} fps",
            m.capture_id, o.kms_id, o.width, o.height
        );
        self.captures.list.push(Capture {
            token,
            id: m.capture_id,
            output: o.kms_id,
            size: (o.width, o.height),
            interval_ns: 1_000_000_000 / fps,
            ring_id: self.captures.next_ring,
            requested: false,
            slots: Vec::new(),
            bytes: 0,
            snap: None,
            damage: Damage::new(),
            last_layers: Vec::new(),
            last_ns: None,
            flipped: false,
            reading: None,
        });
        self.captures.stats.started += 1;
        if self.gpu.ready() {
            self.captures_on_ready();
        } else {
            if !self.gpu.running() && self.gpu.state == gpu::State::Off {
                self.gpu_spawn();
            }
            if !self.gpu.running() && self.gpu.state != gpu::State::Backoff {
                let i = self.captures.list.len() - 1;
                self.capture_stop(i, Some(Reason::Unsupported));
            }
        }
        true
    }

    /// `CaptureRelease`: the slot is free again.
    pub(crate) fn capture_release(&mut self, token: u64, m: msg::CaptureRelease) -> bool {
        if let Some(c) = self
            .captures
            .list
            .iter_mut()
            .find(|c| c.token == token && c.id == m.capture_id)
            && let Some(s) = c.slots.get_mut(usize::from(m.slot))
            && let Slot::WithClient(serial) = *s
        {
            *s = Slot::Free(Some(serial));
            self.captures_pump(None);
        }
        true
    }

    /// `CaptureStop` from the client.
    pub(crate) fn capture_stop_request(&mut self, token: u64, capture_id: u32) -> bool {
        if let Some(i) = self
            .captures
            .list
            .iter()
            .position(|c| c.token == token && c.id == capture_id)
        {
            self.capture_stop(i, Some(Reason::Client));
        }
        true
    }

    /// End capture `i`: the ring and the shadow import go back to the
    /// helper, and the client hears `reason` (`None`: it is gone).
    fn capture_stop(&mut self, i: usize, reason: Option<Reason>) {
        let c = self.captures.list.remove(i);
        let poll = EpollPoll(&self.epoll);
        if c.requested || !c.slots.is_empty() {
            self.gpu.send(
                &poll,
                TOK_GPU,
                &nitro_gpu::ToHelper::FreeCaptureRing { ring_id: c.ring_id },
                Vec::new(),
            );
        }
        if let Some((_, id)) = &c.snap {
            let id = *id;
            self.gpu.send(
                &poll,
                TOK_GPU,
                &nitro_gpu::ToHelper::Release { id },
                Vec::new(),
            );
        }
        if let Some(r) = &c.reading
            && matches!(c.slots.get(r.slot), Some(Slot::Submitted(_)))
        {
            // Its `Composited` still comes: that keeps the borrows' fence.
            self.captures.orphans.push(r.serial);
        }
        if let Some(r) = reason {
            self.capture_send_stopped(c.token, c.id, r);
        }
        let poll = EpollPoll(&self.epoll);
        info!(
            "capture {} of client {:#x} stopped ({})",
            c.id,
            c.token,
            reason.map_or("client gone", |r| match r {
                Reason::Client => "client",
                Reason::Denied => "denied",
                Reason::Locked => "locked",
                Reason::OutputGone => "output gone",
                Reason::HelperLost => "helper lost",
                Reason::Revoked => "revoked",
                Reason::Unsupported => "unsupported",
            })
        );
        if self.captures.list.is_empty() && self.gpu.owner.is_none() {
            // Textures imported for the recording go, so an on-demand
            // helper can idle-exit again.
            self.gpu.release_all(&poll, TOK_GPU);
        }
    }

    /// Stop every capture with `reason`.
    pub(crate) fn captures_stop_all(&mut self, reason: Reason) {
        while !self.captures.list.is_empty() {
            self.capture_stop(0, Some(reason));
        }
        self.captures.orphans.clear();
        self.gpu.capture_due = None;
    }

    /// A client went away: its captures end without a message.
    pub(crate) fn capture_client_gone(&mut self, token: u64) {
        while let Some(i) = self.captures.list.iter().position(|c| c.token == token) {
            self.capture_stop(i, None);
        }
    }

    /// The output set changed: captures of outputs that are gone end.
    pub(crate) fn captures_outputs_changed(&mut self) {
        while let Some(i) = self.captures.list.iter().position(|c| {
            !self
                .outputs
                .iter()
                .any(|o| o.kms_id == c.output && (o.width, o.height) == c.size)
        }) {
            self.capture_stop(i, Some(Reason::OutputGone));
        }
    }

    /// The helper is going away deliberately (VT switch, reconfigure): the
    /// rings go with it. Captures pause and get a new ring (a second
    /// `CaptureBuffers`) when it is back; with `gpu.helper = off` they
    /// end.
    pub(crate) fn captures_pause(&mut self) {
        if !self.gpu.enabled() {
            self.captures_stop_all(Reason::Unsupported);
            return;
        }
        for c in &mut self.captures.list {
            c.requested = false;
            c.slots.clear();
            c.bytes = 0;
            c.snap = None;
            c.reading = None;
            c.last_layers.clear();
            c.last_ns = None;
        }
        self.captures.orphans.clear();
        self.gpu.capture_due = None;
    }

    /// The helper answered `Hello`: ask for the rings that are missing.
    pub(crate) fn captures_on_ready(&mut self) {
        let poll = EpollPoll(&self.epoll);
        for c in &mut self.captures.list {
            if c.requested || !c.slots.is_empty() {
                continue;
            }
            self.captures.next_ring = self.captures.next_ring.wrapping_add(1).max(1);
            c.ring_id = self.captures.next_ring;
            c.requested = self.gpu.send(
                &poll,
                TOK_GPU,
                &nitro_gpu::ToHelper::AllocCaptureRing {
                    ring_id: c.ring_id,
                    n: RING_SLOTS,
                    w: c.size.0,
                    h: c.size.1,
                    fourcc: proto::XR24,
                    // Empty: every modifier the helper renders, LINEAR first.
                    modifiers: Vec::new(),
                },
                Vec::new(),
            );
        }
    }

    /// `CaptureRing`: hand the slots to the client.
    pub(crate) fn capture_ring(
        &mut self,
        ring_id: u32,
        size: (u32, u32),
        (fourcc, modifier): (u32, u64),
        slots: Vec<(proto::SlotLayout, OwnedFd)>,
    ) {
        let Some(i) = self
            .captures
            .list
            .iter()
            .position(|c| c.requested && c.ring_id == ring_id)
        else {
            // Stopped meanwhile; its `FreeCaptureRing` follows the alloc.
            return;
        };
        let c = &mut self.captures.list[i];
        c.requested = false;
        if size != c.size || slots.is_empty() || slots.len() > msg::MAX_CAPTURE_SLOTS {
            self.capture_stop(i, Some(Reason::Unsupported));
            return;
        }
        c.bytes = slots.iter().map(|(l, _)| l.size).sum();
        c.slots = vec![Slot::Free(None); slots.len()];
        c.damage.clear();
        c.damage
            .add(IRect::new(0, 0, size.0.cast_signed(), size.1.cast_signed()));
        c.last_layers.clear();
        c.last_ns = None;
        c.flipped = true;
        info!(
            "capture {}: ring of {} ({}x{}, modifier {modifier:#x}, {} KiB)",
            c.id,
            slots.len(),
            size.0,
            size.1,
            c.bytes / 1024
        );
        let (token, capture_id) = (c.token, c.id);
        let m = msg::CaptureBuffers {
            capture_id,
            width: size.0,
            height: size.1,
            format: fourcc,
            modifier,
            slots: slots
                .into_iter()
                .map(|(l, fd)| nitro_wire::types::CaptureSlot {
                    fd,
                    offset: l.offset,
                    pitch: l.pitch,
                    size: l.size,
                })
                .collect(),
        };
        if let Some(client) = self.wire_clients.get_mut(&token) {
            client.send(&ServerMsg::CaptureBuffers(m));
        }
        self.captures_pump(None);
    }

    /// The helper could not allocate a ring.
    pub(crate) fn capture_ring_failed(&mut self, ring_id: u32) {
        if let Some(i) = self
            .captures
            .list
            .iter()
            .position(|c| c.requested && c.ring_id == ring_id)
        {
            self.captures.list[i].requested = false;
            self.capture_stop(i, Some(Reason::Unsupported));
        }
    }

    /// `Composited` for a capture frame: the client gets the slot. Gives
    /// the fence back when `serial` is not a capture frame.
    pub(crate) fn capture_composited(&mut self, serial: u64, fence: OwnedFd) -> Option<OwnedFd> {
        let poll = EpollPoll(&self.epoll);
        if let Some(p) = self.captures.orphans.iter().position(|s| *s == serial) {
            self.captures.orphans.swap_remove(p);
            self.gpu.capture_done(serial);
            self.gpu.keep_fence(
                &poll,
                TOK_GPU_FENCE_BASE + serial,
                serial,
                &fence,
                Instant::now(),
            );
            return None;
        }
        let Some(c) = self
            .captures
            .list
            .iter_mut()
            .find(|c| c.reading.as_ref().is_some_and(|r| r.serial == serial))
        else {
            return Some(fence);
        };
        let r = c.reading.as_ref()?;
        self.gpu.capture_done(serial);
        self.gpu
            .keep_fence(&poll, TOK_GPU_FENCE_BASE + serial, serial, &fence, r.sent);
        let slot = r.slot;
        c.slots[slot] = Slot::WithClient(serial);
        let m = msg::CaptureFrame {
            capture_id: c.id,
            #[allow(clippy::cast_possible_truncation)] // ≤ MAX_CAPTURE_SLOTS
            slot: slot as u8,
            time_ns: r.time_ns,
            damage: r.damage.clone(),
            fence,
        };
        let token = c.token;
        if !self.gpu.fence_pending(serial) {
            c.reading = None;
        }
        self.captures.stats.frames += 1;
        if let Some(client) = self.wire_clients.get_mut(&token) {
            client.send(&ServerMsg::CaptureFrame(m));
        }
        None
    }

    /// The helper refused a capture frame. Returns whether it was one.
    pub(crate) fn capture_frame_failed(&mut self, serial: u64, code: proto::ErrorCode) -> bool {
        self.gpu.capture_done(serial);
        self.gpu.borrows.done(serial);
        if let Some(p) = self.captures.orphans.iter().position(|s| *s == serial) {
            self.captures.orphans.swap_remove(p);
            return true;
        }
        let Some(c) = self
            .captures
            .list
            .iter_mut()
            .find(|c| c.reading.as_ref().is_some_and(|r| r.serial == serial))
        else {
            return false;
        };
        debug!("capture {} frame {serial} refused: {code:?}", c.id);
        if let Some(r) = c.reading.take() {
            c.slots[r.slot] = Slot::Free(None);
            for d in r.damage {
                c.damage.add(d);
            }
        }
        c.last_layers.clear();
        self.captures.stats.drops += 1;
        true
    }

    /// A helper fence signalled: a capture frame no longer reads the
    /// shadow.
    pub(crate) fn capture_fence(&mut self, serial: u64) {
        let mut any = false;
        for c in &mut self.captures.list {
            if let Some(r) = c.reading.as_ref()
                && r.serial == serial
                && matches!(c.slots[r.slot], Slot::WithClient(_))
            {
                self.captures
                    .stats
                    .gpu_us
                    .push(r.sent.elapsed().as_micros() as u64);
                c.reading = None;
                any = true;
            }
        }
        if any {
            self.captures_pump(None);
        }
    }

    /// `rects` of output `id` were rasterized.
    pub(crate) fn capture_damage(&mut self, id: KmsOutputId, rects: &[IRect]) {
        for c in &mut self.captures.list {
            if c.output == id {
                for r in rects {
                    c.damage.add(*r);
                }
            }
        }
    }

    /// Output `id` flipped: capture what it shows now (`on_flip`, after
    /// the paint). A wanted frame with no free slot counts as a drop.
    pub(crate) fn captures_on_flip(&mut self, id: KmsOutputId) {
        if self.captures.list.is_empty() {
            return;
        }
        for c in &mut self.captures.list {
            if c.output == id {
                c.flipped = true;
            }
        }
        self.captures_pump(Some(id));
    }

    /// The helper's timer fired: a rate-held frame may be due.
    pub(crate) fn captures_on_timer(&mut self) {
        self.captures_pump(None);
    }

    /// Submit every frame that is wanted, allowed and has a slot.
    /// `drops_for`: count a drop for that output's captures when the slot
    /// is what is missing.
    fn captures_pump(&mut self, drops_for: Option<KmsOutputId>) {
        if !self.active || !self.gpu.ready() {
            return;
        }
        let now = monotonic_ns();
        let mut i = 0;
        while i < self.captures.list.len() {
            let n = self.captures.list.len();
            let drop = drops_for == Some(self.captures.list[i].output);
            self.capture_pump_one(i, now, drop);
            // A stop removed it: the next one is at `i` now.
            if self.captures.list.len() == n {
                i += 1;
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::many_single_char_names)] // One frame's steps in order.
    fn capture_pump_one(&mut self, i: usize, now: u64, on_flip: bool) {
        let count_drop = on_flip;
        let c = &self.captures.list[i];
        if c.requested || c.slots.is_empty() || c.reading.is_some() {
            return;
        }
        let Some(index) = self.outputs.iter().position(|o| o.kms_id == c.output) else {
            self.capture_stop(i, Some(Reason::OutputGone));
            return;
        };
        let o = &self.outputs[index];
        if (o.width, o.height) != c.size {
            self.capture_stop(i, Some(Reason::OutputGone));
            return;
        }
        if !o
            .shadow
            .as_ref()
            .is_some_and(crate::frame::Shadow::is_complete)
        {
            return;
        }
        // Rate: a frame at most every `interval`, with half a refresh of
        // slack so 30 fps on a 60 Hz output is every other flip.
        // The slack applies at a flip only: the rate timer fires at the
        // full interval, or it would add frames between flips.
        if let Some(last) = c.last_ns {
            let full = last + c.interval_ns;
            let due = if on_flip {
                full.saturating_sub(u64::from(o.refresh_ns / 2))
            } else {
                full
            };
            if now < due {
                if c.flipped || !c.damage.is_empty() {
                    let at = Instant::now() + Duration::from_nanos(full.saturating_sub(now));
                    self.gpu.capture_due = Some(self.gpu.capture_due.map_or(at, |t| t.min(at)));
                    self.gpu.arm();
                }
                return;
            }
        }
        // The Surfaces the shadow has holes for: on a plane, or drawn by
        // the helper in mode 2. CPU-drawn ones are in the shadow already.
        self.gpu_export_scanouts();
        let mut items = std::mem::take(&mut self.paint_items);
        let mut layers = Vec::new();
        self.gpu_layers(index, &mut items, &mut layers);
        items.clear();
        self.paint_items = items;
        let d = &self.outputs[index].decision;
        layers.retain(|l| {
            d.places(l.node) || (d.mode == planes::Mode::Gpu && d.gpu.contains(&l.node))
        });
        let shown: Vec<(NodeKey, BufferKey, IRect)> =
            layers.iter().map(|l| (l.node, l.key, l.dst)).collect();
        let bounds = self.outputs[index].bounds();
        let c = &mut self.captures.list[i];
        c.flipped = false;
        for l in &shown {
            if !c.last_layers.contains(l) {
                c.damage.add(l.2);
            }
        }
        for l in &c.last_layers {
            if !shown.contains(l) {
                c.damage.add(l.2);
            }
        }
        c.last_layers.clone_from(&shown);
        if c.damage.is_empty() {
            return;
        }
        let gpu = &self.gpu;
        let Some(slot) = c
            .slots
            .iter()
            .position(|s| matches!(s, Slot::Free(l) if l.is_none_or(|l| !gpu.fence_pending(l))))
        else {
            if count_drop {
                self.captures.stats.drops += 1;
            }
            return;
        };
        let damage = gpu_rects(self.captures.list[i].damage.rects(), bounds);
        let Some(shadow_id) = self.capture_snapshot(i, index, &damage) else {
            return;
        };
        let poll = EpollPoll(&self.epoll);
        let mut below = Vec::with_capacity(layers.len() + 1);
        let mut above = Vec::new();
        let mut keys = Vec::new();
        for l in &layers {
            let Some(tex) = self.gpu.texture(&poll, TOK_GPU, l.key, l.encoding, l.range) else {
                continue;
            };
            let layer = proto::Layer {
                tex,
                src: l.src,
                dst: l.dst,
                blend: l.blend,
            };
            if l.blend == proto::Blend::Opaque {
                below.push(layer);
            } else {
                above.push(layer);
            }
            if !keys.contains(&l.key) {
                keys.push(l.key);
            }
        }
        below.push(proto::Layer {
            tex: shadow_id,
            src: [0.0, 0.0, bounds.w as f32, bounds.h as f32],
            dst: bounds,
            blend: proto::Blend::PremulOver,
        });
        below.extend(above);
        let c = &mut self.captures.list[i];
        // A staging-path helper copies the changed snapshot rects now;
        // with udmabuf it is a no-op.
        self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::UploadDamage {
                id: shadow_id,
                rects: damage.clone(),
            },
            Vec::new(),
        );
        let serial = self.gpu.serial();
        let sent = self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::CaptureComposite(proto::CaptureComposite {
                ring_id: c.ring_id,
                frame: proto::Composite {
                    serial,
                    out_idx: slot as u32,
                    damage: damage.clone(),
                    layers: below,
                    fence_mask: 0,
                },
            }),
            Vec::new(),
        );
        if !sent {
            return;
        }
        self.gpu.borrows.add(serial, keys);
        self.gpu.capture_sent(serial);
        let time_ns = match self.outputs[index].last_vblank_ns {
            0 => now,
            t => t,
        };
        let c = &mut self.captures.list[i];
        c.slots[slot] = Slot::Submitted(serial);
        c.damage.clear();
        c.last_ns = Some(now);
        c.reading = Some(Reading {
            serial,
            slot,
            damage,
            time_ns,
            sent: Instant::now(),
        });
    }

    /// The helper's texture for capture `i`'s snapshot of output
    /// `index`, with `rects` copied in from the shadow first. A snapshot is
    /// (re)made and imported when there is none or the shadow's geometry
    /// changed; a new one is filled whole. Only called with no frame of
    /// this capture out, so the helper is not reading it.
    fn capture_snapshot(&mut self, i: usize, index: usize, rects: &[IRect]) -> Option<u32> {
        let shadow = self.outputs[index].shadow.as_ref()?;
        let geom = (shadow.width(), shadow.height(), shadow.stride());
        let fresh = !matches!(&self.captures.list[i].snap, Some((s, _)) if s.geom == geom);
        if fresh {
            let snap = match Snapshot::new(geom) {
                Ok(s) => s,
                Err(e) => {
                    crate::warn!("capture: snapshot: {e}");
                    return None;
                }
            };
            let fd = snap.fd.try_clone().ok()?;
            let poll = EpollPoll(&self.epoll);
            if let Some((_, old)) = self.captures.list[i].snap.take() {
                self.gpu.send(
                    &poll,
                    TOK_GPU,
                    &nitro_gpu::ToHelper::Release { id: old },
                    Vec::new(),
                );
            }
            let sid = self.gpu.tex_id();
            let desc = proto::ShadowDesc {
                id: sid,
                w: geom.0,
                h: geom.1,
                stride: geom.2,
                fourcc: proto::AR24,
            };
            if !self.gpu.send(
                &poll,
                TOK_GPU,
                &nitro_gpu::ToHelper::ImportShadow(desc),
                vec![fd],
            ) {
                return None;
            }
            self.captures.list[i].snap = Some((snap, sid));
        }
        let shadow = self.outputs[index].shadow.as_ref()?;
        let (snap, sid) = self.captures.list[i].snap.as_mut()?;
        let all = [IRect::new(0, 0, geom.0.cast_signed(), geom.1.cast_signed())];
        let copy = if fresh { &all[..] } else { rects };
        let t = Instant::now();
        let mut dst = nitro_kms::BufferMut {
            width: geom.0,
            height: geom.1,
            stride: geom.2,
            data: snap.map.as_bytes_mut(),
        };
        shadow.stream_to(&mut dst, copy);
        let sid = *sid;
        self.captures
            .stats
            .copy_us
            .push(t.elapsed().as_micros() as u64);
        Some(sid)
    }

    /// The `capture_*` lines of `stats`.
    pub(crate) fn capture_stats(&self, pairs: &mut Vec<(&'static str, u64)>) {
        let s = &self.captures.stats;
        pairs.push(("capture_active", self.captures.list.len() as u64));
        pairs.push(("capture_started", s.started));
        pairs.push(("capture_frames", s.frames));
        pairs.push(("capture_drops", s.drops));
        pairs.push(("capture_rings_bytes", self.captures.ring_bytes()));
        pairs.push(("capture_gpu_us", s.gpu_us.mean()));
        pairs.push(("capture_gpu_us_max", s.gpu_us.max()));
        pairs.push(("capture_copy_us", s.copy_us.mean()));
        pairs.push((
            "capture_snapshot_bytes",
            self.captures
                .list
                .iter()
                .filter_map(|c| c.snap.as_ref())
                .map(|(s, _)| s.len as u64)
                .sum(),
        ));
    }
}

/// A sealed memfd copy of an output's shadow that one capture's frames
/// sample: the shadow itself stays the rasterizer's alone.
#[derive(Debug)]
struct Snapshot {
    fd: OwnedFd,
    map: nitro_shm::MappingMut,
    geom: (u32, u32, u32),
    len: usize,
}

impl Snapshot {
    fn new(geom: (u32, u32, u32)) -> Result<Self, String> {
        const PAGE: usize = 4096;
        let len = geom.2 as usize * geom.1 as usize;
        let padded = len.max(1).div_ceil(PAGE) * PAGE;
        let fd =
            nitro_shm::create_sealed("nitro-capture", padded as u64).map_err(|e| e.to_string())?;
        let map = nitro_shm::MappingMut::map_mut(std::os::fd::AsFd::as_fd(&fd), padded)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            fd,
            map,
            geom,
            len: padded,
        })
    }
}
