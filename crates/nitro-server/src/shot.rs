//! `shot` with what is really on screen (#3962).
//!
//! The shadow holds everything the CPU composited. What it does not hold:
//! Surfaces on a display plane or composited by the GPU helper (holes,
//! alpha 0), and Surfaces whose buffer the CPU cannot read (tiled, drawn
//! as placeholder grey). A shot repaints those rects of a **copy** of the
//! shadow with every CPU-readable Surface drawn from its buffer, clears
//! the rest to holes, and fills those from a helper `Capture` (the
//! helper samples tiled and compressed buffers) — or, when the helper
//! cannot, with [`frame::HOLE_PLACEHOLDER`] and a reason in the metadata.
//!
//! A shot that needs the helper is answered when the helper has: the
//! control client waits (`Client::waiting`), the loop does not. Nothing
//! outlives the reply: the image copy is dropped, textures imported only
//! for the shot are released, and the helper frees its capture target
//! before it answers.

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::time::Instant;

use nitro_core::{IRect, Rect};
use nitro_gpu::proto;
use nitro_kms::Image;
use nitro_raster::Canvas;
use nitro_scene::{BufferKey, PaintItem, PaintKind, SurfaceRef};

use crate::cursor::Cursor;
use crate::protocol::{self, ShotMeta, ShotReason, ShotRequest};
use crate::{EpollPoll, Server, TOK_GPU, frame, gpu, gpu_encoding, gpu_range, info, warn};

/// Shots waiting for the helper at once; more are answered with the
/// placeholder (`reason=busy`).
pub const MAX_PENDING: usize = 4;

/// A Surface the helper could draw into the capture.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    key: BufferKey,
    /// Output-local.
    dst: IRect,
    src: [f32; 4],
    encoding: proto::ColorEncoding,
    range: proto::ColorRange,
}

/// A shot waiting for the helper.
#[derive(Debug)]
pub struct Pending {
    token: u64,
    name: String,
    /// `None`: waiting for the helper to answer `Hello`.
    serial: Option<u64>,
    image: Image,
    meta: ShotMeta,
    want_meta: bool,
    /// Not sent yet.
    cands: Vec<Candidate>,
    /// Destinations of the layers in the capture.
    sent: Vec<IRect>,
    /// Their buffers (borrowed against `BufferReleased`).
    keys: Vec<BufferKey>,
    /// Textures imported for this shot only.
    imported: Vec<BufferKey>,
    started: Instant,
}

impl Pending {
    fn placeholder(&mut self, n: usize, reason: ShotReason) {
        if n == 0 {
            return;
        }
        self.meta.placeholder += u32::try_from(n).unwrap_or(u32::MAX);
        if self.meta.reason == ShotReason::None {
            self.meta.reason = reason;
        }
    }

    /// Everything not captured yet becomes the placeholder.
    fn give_up(&mut self, reason: ShotReason) {
        let n = self.cands.len() + self.sent.len();
        self.placeholder(n, reason);
        self.cands.clear();
        self.sent.clear();
    }
}

/// The server's shot state and counters.
#[derive(Debug, Default)]
pub struct Shots {
    pending: Vec<Pending>,
    count: u64,
    last_us: u64,
    cpu: u64,
    helper: u64,
    placeholders: u64,
}

impl Server {
    /// Answer a `shot`: the reply, or `None` when it waits for the helper
    /// (answered later through `control_reply`).
    #[allow(clippy::too_many_lines)] // One shot's steps in order.
    pub(crate) fn shot(&mut self, token: u64, req: &ShotRequest) -> Option<Vec<u8>> {
        let started = Instant::now();
        let id = match self.shot_output(req.output.as_deref()) {
            Ok(id) => id,
            Err(reply) => return Some(reply),
        };
        let index = self.outputs.iter().position(|o| {
            o.kms_id == id && o.shadow.as_ref().is_some_and(frame::Shadow::is_complete)
        });
        let scene_id = index.map(|i| self.outputs[i].scene_id);
        let orect = scene_id
            .and_then(|s| self.scene.output_info(s))
            .map(|(r, _)| r);
        let (Some(index), Some(scene_id), Some(orect)) = (index, scene_id, orect) else {
            // No shadow to repaint (yet): the front buffer, holes grey.
            let meta = ShotMeta::default();
            return Some(match self.backend.read_front(id) {
                Ok(img) => protocol::shot_reply_meta(&Self::honest(img), req.meta.then_some(&meta)),
                Err(e) => protocol::err_reply(&e.to_string()),
            });
        };
        let name = self
            .backend
            .outputs()
            .iter()
            .find(|o| o.id == id)
            .map_or_else(String::new, |o| o.name.clone());
        let mut image = self.outputs[index].shadow.as_ref()?.image();
        let mut meta = ShotMeta::default();
        let mut items = std::mem::take(&mut self.paint_items);
        items.clear();
        self.scene.paint_list(scene_id, &orect, &mut items);
        let mut repaint: Vec<IRect> = Vec::new();
        let mut readable: HashSet<BufferKey> = HashSet::new();
        let mut bracket: Vec<BufferKey> = Vec::new();
        let mut cands: Vec<Candidate> = Vec::new();
        let mut placeholders: Vec<ShotReason> = Vec::new();
        for item in &items {
            let (size, hole) = match item.kind {
                PaintKind::Hole { size, .. } => (size, true),
                PaintKind::Surface { size, .. } => (size, false),
                _ => continue,
            };
            let Some(content) = self
                .scene
                .node(item.node)
                .ok()
                .and_then(nitro_scene::Node::surface)
                .and_then(|s| s.content)
            else {
                continue;
            };
            let vis = item.bounds.intersect(&item.clip).intersect(&orect);
            if vis.is_empty() {
                continue;
            }
            meta.surfaces += 1;
            let local = vis.translate(-orect.x, -orect.y);
            let cpu = self
                .scene
                .buffer(content.buffer)
                .is_ok_and(nitro_scene::Buffer::cpu_readable)
                && self.shot_fence_ok(content.buffer);
            if cpu {
                meta.cpu += 1;
                readable.insert(content.buffer);
                if hole {
                    // On a plane or composited by the helper: the shadow
                    // has a hole there.
                    repaint.push(local);
                    if self.unbracketed.contains(&content.buffer) {
                        bracket.push(content.buffer);
                    }
                }
                continue;
            }
            repaint.push(local);
            if let Some(c) = self.shot_candidate(item, size, content, orect) {
                cands.push(c);
            } else {
                placeholders.push(if !self.gpu.enabled() {
                    ShotReason::HelperOff
                } else if self.gpu.state == gpu::State::GaveUp {
                    ShotReason::HelperUnavailable
                } else {
                    ShotReason::Unsupported
                });
            }
        }
        let mut cursor = self.cursor_state(scene_id);
        if !req.cursor && cursor.visible {
            repaint.push(Cursor::rect_scaled(
                cursor.x,
                cursor.y,
                cursor.shape,
                cursor.scale,
            ));
            cursor.visible = false;
        }
        if !repaint.is_empty() {
            for k in &bracket {
                self.dmabuf_sync(*k, true);
            }
            let origin = self.output_origin(scene_id);
            let (w, h, stride) = (image.width, image.height, image.stride);
            let mut canvas = Canvas::new(&mut image.data, w, h, stride);
            frame::paint_shot_region(
                &mut canvas,
                &self.scene,
                &mut self.text,
                &mut self.icons,
                scene_id,
                origin,
                &repaint,
                (&self.cursor, cursor),
                &mut items,
                &self.palette,
                &|k| readable.contains(&k),
            );
            for k in &bracket {
                self.dmabuf_sync(*k, false);
            }
        }
        items.clear();
        self.paint_items = items;
        let mut p = Pending {
            token,
            name,
            serial: None,
            image,
            meta,
            want_meta: req.meta,
            cands: Vec::new(),
            sent: Vec::new(),
            keys: Vec::new(),
            imported: Vec::new(),
            started,
        };
        for r in placeholders {
            p.placeholder(1, r);
        }
        // One layer per Surface; the topmost win, as in mode 2.
        if cands.len() > proto::MAX_LAYERS {
            let extra = cands.len() - proto::MAX_LAYERS;
            cands.drain(..extra);
            p.placeholder(extra, ShotReason::TooManyLayers);
        }
        p.cands = cands;
        if p.cands.is_empty() {
            return Some(self.shot_finish(p, None));
        }
        if self.shots.pending.len() >= MAX_PENDING {
            p.give_up(ShotReason::Busy);
            return Some(self.shot_finish(p, None));
        }
        if self.gpu.ready() {
            if self.shot_send(&mut p) {
                self.shots.pending.push(p);
                return None;
            }
            return Some(self.shot_finish(p, None));
        }
        // Not running: start it for the shot (on demand, or restarting
        // an always-on one that is off); it is answered on `Ready`.
        if !self.gpu.running() && self.gpu.state == gpu::State::Off {
            self.gpu_spawn();
        }
        if !self.gpu.running() {
            p.give_up(ShotReason::HelperUnavailable);
            return Some(self.shot_finish(p, None));
        }
        self.shots.pending.push(p);
        None
    }

    /// Whether the CPU may read `key` now: bracketed at latch, or an
    /// early-latched buffer (#3938) whose fence has signalled. Never
    /// waits.
    fn shot_fence_ok(&self, key: BufferKey) -> bool {
        if !self.unbracketed.contains(&key) {
            return true;
        }
        let Ok(b) = self.scene.buffer(key) else {
            return false;
        };
        let Some(fd) = b.fence_fd() else {
            return true;
        };
        let mut fds = [rustix::event::PollFd::from_borrowed_fd(
            fd,
            rustix::event::PollFlags::IN,
        )];
        let zero = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        matches!(rustix::event::poll(&mut fds, Some(&zero)), Ok(n) if n > 0)
    }

    /// `item` as a capture layer, if the helper could draw it (its dma-buf
    /// is known, the helper samples it, the transform is an axis-aligned
    /// scale). Placement as `gpu_layers`.
    fn shot_candidate(
        &self,
        item: &PaintItem,
        size: (f32, f32),
        content: SurfaceRef,
        orect: IRect,
    ) -> Option<Candidate> {
        let t = item.transform;
        if !t.is_axis_aligned() || t.a <= 0.0 || t.d <= 0.0 {
            return None;
        }
        let src = self.gpu.sources.get(&content.buffer)?;
        if self.gpu.refused(content.buffer)
            || (self.gpu.info.is_some() && !self.gpu.samples(src.desc.fourcc, src.desc.modifier))
        {
            return None;
        }
        let dst = t
            .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1))
            .round_out();
        let visible = dst.intersect(&item.clip).intersect(&orect);
        if visible.is_empty() || dst.is_empty() || content.src.is_empty() {
            return None;
        }
        let fx = content.src.w as f32 / dst.w as f32;
        let fy = content.src.h as f32 / dst.h as f32;
        Some(Candidate {
            key: content.buffer,
            dst: visible.translate(-orect.x, -orect.y),
            src: [
                content.src.x as f32 + (visible.x - dst.x) as f32 * fx,
                content.src.y as f32 + (visible.y - dst.y) as f32 * fy,
                visible.w as f32 * fx,
                visible.h as f32 * fy,
            ],
            encoding: gpu_encoding(content.color.matrix),
            range: gpu_range(content.color.range),
        })
    }

    /// Import what the capture samples and send it. False if nothing
    /// went out (every candidate is then a placeholder).
    fn shot_send(&mut self, p: &mut Pending) -> bool {
        let poll = EpollPoll(&self.epoll);
        let mut layers = Vec::with_capacity(p.cands.len());
        for c in std::mem::take(&mut p.cands) {
            let had = self.gpu.has_tex(c.key);
            let Some(tex) = self.gpu.texture(&poll, TOK_GPU, c.key, c.encoding, c.range) else {
                let r = if self.gpu.refused(c.key) {
                    ShotReason::HelperRefused
                } else {
                    ShotReason::Unsupported
                };
                p.placeholder(1, r);
                continue;
            };
            if !had {
                p.imported.push(c.key);
            }
            layers.push(proto::Layer {
                tex,
                src: c.src,
                dst: c.dst,
                blend: proto::Blend::Opaque,
            });
            p.sent.push(c.dst);
            p.keys.push(c.key);
        }
        if layers.is_empty() {
            return false;
        }
        let serial = self.gpu.serial();
        let msg = nitro_gpu::ToHelper::Capture {
            serial,
            w: p.image.width,
            h: p.image.height,
            layers,
        };
        if !self.gpu.send(&poll, TOK_GPU, &msg, Vec::new()) {
            p.give_up(ShotReason::HelperUnavailable);
            self.shot_release(p);
            return false;
        }
        // The buffers stay the client's until the capture is answered.
        self.gpu.borrows.add(serial, p.keys.clone());
        self.gpu.capture_sent(serial);
        p.serial = Some(serial);
        true
    }

    /// Give back what a shot held: its borrows, its capture's hang timer,
    /// and every texture imported only for it that mode 2 does not use.
    fn shot_release(&mut self, p: &Pending) {
        if let Some(s) = p.serial {
            self.gpu.borrows.done(s);
            self.gpu.capture_done(s);
        }
        let in_use: HashSet<BufferKey> = if self.gpu.owner.is_some() {
            self.outputs
                .iter()
                .flat_map(|o| {
                    o.gpu_layers
                        .iter()
                        .filter(|l| o.decision.gpu.contains(&l.node))
                        .map(|l| l.key)
                })
                .collect()
        } else {
            HashSet::new()
        };
        let poll = EpollPoll(&self.epoll);
        for k in &p.imported {
            if !in_use.contains(k) {
                self.gpu.release_tex(&poll, TOK_GPU, *k);
            }
        }
        self.send_gpu_releases();
    }

    /// Fill the holes (from the capture where it drew, grey elsewhere),
    /// count, log, and format the reply.
    fn shot_finish(&mut self, mut p: Pending, cap: Option<(&[u8], u32)>) -> Vec<u8> {
        let sent = std::mem::take(&mut p.sent);
        if cap.is_some() {
            p.meta.helper += u32::try_from(sent.len()).unwrap_or(u32::MAX);
        } else {
            p.placeholder(sent.len(), ShotReason::HelperUnavailable);
        }
        frame::fill_holes(&mut p.image, |x, y| {
            let (xi, yi) = (x.cast_signed(), y.cast_signed());
            if let Some((px, stride)) = cap
                && sent
                    .iter()
                    .any(|r| xi >= r.x && xi < r.right() && yi >= r.y && yi < r.bottom())
            {
                let o = y as usize * stride as usize + x as usize * 4;
                if let Some(s) = px.get(o..o + 3) {
                    return [s[0], s[1], s[2]];
                }
            }
            frame::HOLE_PLACEHOLDER
        });
        let us = p.started.elapsed().as_micros() as u64;
        let m = p.meta;
        let s = &mut self.shots;
        s.count += 1;
        s.last_us = us;
        s.cpu += u64::from(m.cpu);
        s.helper += u64::from(m.helper);
        s.placeholders += u64::from(m.placeholder);
        if m.helper > 0 || m.placeholder > 0 {
            info!(
                "shot {}: {} surfaces, {} cpu, {} helper, {} placeholder, {} ms",
                p.name,
                m.surfaces,
                m.cpu,
                m.helper,
                m.placeholder,
                us / 1000
            );
        }
        if m.placeholder > 0 {
            warn!(
                "shot {}: {} surface(s) shown as placeholder ({})",
                p.name,
                m.placeholder,
                m.reason.as_str()
            );
        }
        protocol::shot_reply_meta(&p.image, p.want_meta.then_some(&m))
    }

    /// The helper answered a shot's capture.
    pub(crate) fn shot_captured(
        &mut self,
        serial: u64,
        (w, h, stride): (u32, u32, u32),
        memfd: OwnedFd,
    ) {
        let Some(i) = self
            .shots
            .pending
            .iter()
            .position(|p| p.serial == Some(serial))
        else {
            return;
        };
        let mut p = self.shots.pending.remove(i);
        self.shot_release(&p);
        let token = p.token;
        let fits =
            (w, h) == (p.image.width, p.image.height) && u64::from(stride) >= u64::from(w) * 4;
        let map = fits
            .then(|| nitro_shm::Mapping::map(memfd, stride as usize * h as usize).ok())
            .flatten();
        let bytes = if let Some(m) = &map {
            self.shot_finish(p, Some((m.as_bytes(), stride)))
        } else {
            p.give_up(ShotReason::HelperRefused);
            self.shot_finish(p, None)
        };
        drop(map);
        self.control_reply(token, bytes);
    }

    /// The helper refused a shot's capture (or an import it needed).
    pub(crate) fn shot_capture_failed(&mut self, serial: u64, code: proto::ErrorCode) {
        let Some(i) = self
            .shots
            .pending
            .iter()
            .position(|p| p.serial == Some(serial))
        else {
            return;
        };
        let mut p = self.shots.pending.remove(i);
        crate::debug!("shot capture {serial} refused: {code:?}");
        self.shot_release(&p);
        p.give_up(ShotReason::HelperRefused);
        let token = p.token;
        let bytes = self.shot_finish(p, None);
        self.control_reply(token, bytes);
    }

    /// The helper answered `Hello`: send the captures that waited for it.
    pub(crate) fn shots_on_ready(&mut self) {
        let waiting: Vec<Pending> = {
            let (w, keep) = std::mem::take(&mut self.shots.pending)
                .into_iter()
                .partition(|p| p.serial.is_none());
            self.shots.pending = keep;
            w
        };
        for mut p in waiting {
            if self.shot_send(&mut p) {
                self.shots.pending.push(p);
            } else {
                let token = p.token;
                let bytes = self.shot_finish(p, None);
                self.control_reply(token, bytes);
            }
        }
    }

    /// The helper is gone, paused or hung: answer every waiting shot with
    /// the placeholder.
    pub(crate) fn shots_fail_all(&mut self, reason: ShotReason) {
        for mut p in std::mem::take(&mut self.shots.pending) {
            self.shot_release(&p);
            p.give_up(reason);
            let token = p.token;
            let bytes = self.shot_finish(p, None);
            self.control_reply(token, bytes);
        }
    }

    /// The `shot*` lines of `stats`.
    pub(crate) fn shot_stats(&self, pairs: &mut Vec<(&'static str, u64)>) {
        let s = &self.shots;
        pairs.push(("shots", s.count));
        pairs.push(("shot_us", s.last_us));
        pairs.push(("shot_cpu_surfaces", s.cpu));
        pairs.push(("shot_helper_captures", s.helper));
        pairs.push(("shot_placeholders", s.placeholders));
        pairs.push(("shot_pending", s.pending.len() as u64));
    }
}
