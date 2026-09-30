//! A GPU-less [`Backend`] for tests: it records every call, and its
//! completion "fences" are pipes the test signals by hand, so tests control
//! exactly when a frame finishes.
//!
//! A fence is the read end of a pipe: it becomes readable (`POLLIN`) once
//! the write end is written to or closed. With [`FakeBackend::auto_signal`]
//! every fence is born signalled.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard};

use nitro_core::IRect;

use crate::backend::{Backend, BackendError, Readback, Ring, RingId, RingRequest};
use crate::proto::{
    AR24, DeviceInfo, DmabufDesc, FormatMod, Layer, MOD_I915_X_TILED, MOD_I915_Y_TILED, MOD_LINEAR,
    NV12, ShadowDesc, ShadowPath, SlotLayout, XR24,
};

/// One recorded backend call.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `import_dmabuf(id, fd count)`.
    ImportDmabuf(u32, usize),
    /// `import_shadow(id)`.
    ImportShadow(u32),
    /// `upload_damage(id, rects)`.
    Upload(u32, Vec<IRect>),
    /// `alloc_ring(Output, n, modifier chosen)`.
    Ring(usize, u64),
    /// `composite(Output, slot, clip, layers' texture ids, acquire fence count)`.
    Composite(usize, Vec<IRect>, Vec<u32>, usize),
    /// `alloc_ring(Capture(id), n, modifier chosen)` (v3).
    CaptureRing(u32, usize, u64),
    /// `composite(Capture(id), slot, clip, layers' texture ids, acquire
    /// fence count)` (v3).
    CaptureComposite(u32, usize, Vec<IRect>, Vec<u32>, usize),
    /// `free_ring(ring)`.
    FreeRing(RingId),
    /// `release(id)`.
    Release(u32),
    /// `readback(slot)`.
    Readback(usize),
    /// `capture(w, h, layers' texture ids)`.
    Capture(u32, u32, Vec<u32>),
}

/// Shared state; the test keeps a clone of the handle.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent test knobs
pub struct FakeState {
    /// Every call, in order.
    pub calls: Vec<Call>,
    /// Write ends of the fences not yet signalled, oldest first.
    pub pending: Vec<OwnedFd>,
    /// Born-signalled fences.
    pub auto_signal: bool,
    /// Make the next import fail with a backend error.
    pub fail_next_import: bool,
    /// Make the next composite fail with a backend error.
    pub fail_next_composite: bool,
    /// While set, `composite` does not return: the helper hangs mid-frame
    /// (tests of the server's hang detection). Clear it to let go.
    pub stall: bool,
    /// Make the next capture fail with a backend error.
    pub fail_next_capture: bool,
    /// What `capture` fills every layer's `dst` with, `[b, g, r, x]`
    /// (the fake samples nothing). All zero means the default
    /// [`CAPTURE_COLOR`].
    pub capture_color: [u8; 4],
    /// The layers of the last composite, bottom to top.
    pub last_layers: Vec<Layer>,
    /// A tiled AR24 pair among the sampleable ones (#3952).
    pub sample_tiled_ar24: bool,
    /// Make the next ring allocation fail with a backend error.
    pub fail_next_ring: bool,
}

/// The fake's capture colour unless [`FakeState::capture_color`] is set.
pub const CAPTURE_COLOR: [u8; 4] = [0x90, 0x60, 0x30, 0xff];

/// The fake. Its textures are just their ids.
#[derive(Debug, Clone, Default)]
pub struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
    ring: Option<(u32, u32)>,
}

impl FakeBackend {
    /// A fake whose fences stay pending until [`FakeBackend::signal`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake whose fences are signalled at birth.
    #[must_use]
    pub fn auto_signal() -> Self {
        let f = Self::default();
        f.state().auto_signal = true;
        f
    }

    /// The shared state.
    ///
    /// # Panics
    /// If a thread panicked while holding the lock.
    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake state poisoned")
    }

    /// Signal the oldest pending fence. Returns whether there was one.
    pub fn signal(&self) -> bool {
        let mut s = self.state();
        if s.pending.is_empty() {
            false
        } else {
            drop(s.pending.remove(0));
            true
        }
    }

    /// Recorded calls so far.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    fn record(&self, c: Call) {
        self.state().calls.push(c);
    }
}

fn err(msg: &str) -> BackendError {
    BackendError::new(msg)
}

impl Backend for FakeBackend {
    type Tex = u32;

    fn info(&self) -> DeviceInfo {
        let lin = |fourcc| FormatMod {
            fourcc,
            modifier: MOD_LINEAR,
        };
        DeviceInfo {
            device: "fake".into(),
            driver: "fake".into(),
            sampleable: {
                let mut v = vec![
                    lin(XR24),
                    lin(AR24),
                    lin(NV12),
                    // What a VA-API decoder and Chromium hand over (#3962).
                    FormatMod {
                        fourcc: NV12,
                        modifier: MOD_I915_Y_TILED,
                    },
                    FormatMod {
                        fourcc: XR24,
                        modifier: MOD_I915_Y_TILED,
                    },
                ];
                if self.state().sample_tiled_ar24 {
                    v.push(FormatMod {
                        fourcc: AR24,
                        modifier: MOD_I915_X_TILED,
                    });
                }
                v
            },
            render: vec![
                lin(XR24),
                FormatMod {
                    fourcc: XR24,
                    modifier: MOD_I915_X_TILED,
                },
            ],
        }
    }

    fn shadow_path(&self) -> ShadowPath {
        ShadowPath::Staging
    }

    fn import_dmabuf(&mut self, d: &DmabufDesc, fds: Vec<OwnedFd>) -> Result<u32, BackendError> {
        if std::mem::take(&mut self.state().fail_next_import) {
            return Err(err("fake import failure"));
        }
        self.record(Call::ImportDmabuf(d.id, fds.len()));
        Ok(d.id)
    }

    fn import_shadow(&mut self, d: &ShadowDesc, _memfd: OwnedFd) -> Result<u32, BackendError> {
        if std::mem::take(&mut self.state().fail_next_import) {
            return Err(err("fake import failure"));
        }
        self.record(Call::ImportShadow(d.id));
        Ok(d.id)
    }

    fn upload_damage(&mut self, t: &mut u32, rects: &[IRect]) -> Result<(), BackendError> {
        self.record(Call::Upload(*t, rects.to_vec()));
        Ok(())
    }

    fn alloc_ring(&mut self, ring: RingId, req: &RingRequest) -> Result<Ring, BackendError> {
        let modifier = req.modifiers[0];
        self.record(match ring {
            RingId::Output => Call::Ring(req.n, modifier),
            RingId::Capture(id) => Call::CaptureRing(id, req.n, modifier),
        });
        if std::mem::take(&mut self.state().fail_next_ring) {
            return Err(err("fake ring failure"));
        }
        if ring == RingId::Output {
            self.ring = Some((req.w, req.h));
        }
        let pitch = req.w * 4;
        let size = u64::from(pitch) * u64::from(req.h);
        let mut slots = Vec::with_capacity(req.n);
        for _ in 0..req.n {
            let fd = nitro_shm::create_sealed("nitro-gpu-fake-slot", size)
                .map_err(|e| err(&e.to_string()))?;
            slots.push((
                SlotLayout {
                    offset: 0,
                    pitch,
                    size,
                },
                fd,
            ));
        }
        Ok(Ring { modifier, slots })
    }

    fn composite(
        &mut self,
        ring: RingId,
        slot: usize,
        clip: &[IRect],
        layers: &[(&u32, Layer)],
        acquire: Vec<OwnedFd>,
    ) -> Result<OwnedFd, BackendError> {
        self.state().last_layers = layers.iter().map(|(_, l)| *l).collect();
        let texs = layers.iter().map(|(t, _)| **t).collect();
        self.record(match ring {
            RingId::Output => Call::Composite(slot, clip.to_vec(), texs, acquire.len()),
            RingId::Capture(id) => {
                Call::CaptureComposite(id, slot, clip.to_vec(), texs, acquire.len())
            }
        });
        if std::mem::take(&mut self.state().fail_next_composite) {
            return Err(err("fake composite failure"));
        }
        while self.state().stall {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
            .map_err(|e| err(&e.to_string()))?;
        let mut s = self.state();
        if !s.auto_signal {
            s.pending.push(w);
        }
        Ok(r)
    }

    fn free_ring(&mut self, ring: RingId) {
        self.record(Call::FreeRing(ring));
        if ring == RingId::Output {
            self.ring = None;
        }
    }

    fn release(&mut self, t: u32) {
        self.record(Call::Release(t));
    }

    fn capture(
        &mut self,
        w: u32,
        h: u32,
        layers: &[(&u32, Layer)],
    ) -> Result<Readback, BackendError> {
        self.record(Call::Capture(
            w,
            h,
            layers.iter().map(|(t, _)| **t).collect(),
        ));
        if std::mem::take(&mut self.state().fail_next_capture) {
            return Err(err("fake capture failure"));
        }
        while self.state().stall {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let color = match self.state().capture_color {
            [0, 0, 0, 0] => CAPTURE_COLOR,
            c => c,
        };
        let stride = w as usize * 4;
        let mut px = vec![0u8; stride * h as usize];
        for (_, l) in layers {
            let d = l.dst;
            for y in d.y.max(0)..d.bottom().min(h.cast_signed()) {
                for x in d.x.max(0)..d.right().min(w.cast_signed()) {
                    let o = y as usize * stride + x as usize * 4;
                    px[o..o + 4].copy_from_slice(&color);
                }
            }
        }
        let memfd = nitro_shm::memfd_with("nitro-gpu-fake-capture", &px)
            .map_err(|e| err(&e.to_string()))?;
        Ok(Readback {
            memfd,
            stride: w * 4,
        })
    }

    fn readback(&mut self, out_idx: usize) -> Result<Readback, BackendError> {
        self.record(Call::Readback(out_idx));
        let (w, h) = self.ring.ok_or_else(|| err("no ring"))?;
        let memfd = nitro_shm::create_sealed("nitro-gpu-fake-readback", u64::from(w * 4 * h))
            .map_err(|e| err(&e.to_string()))?;
        Ok(Readback {
            memfd,
            stride: w * 4,
        })
    }
}
