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

use crate::backend::{Backend, BackendError, Readback, Ring, RingRequest};
use crate::proto::{
    AR24, DeviceInfo, DmabufDesc, FormatMod, Layer, MOD_I915_X_TILED, MOD_LINEAR, NV12, ShadowDesc,
    ShadowPath, SlotLayout, XR24,
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
    /// `alloc_output_ring(n, modifier chosen)`.
    Ring(usize, u64),
    /// `composite(slot, clip, layers' texture ids, acquire fence count)`.
    Composite(usize, Vec<IRect>, Vec<u32>, usize),
    /// `release(id)`.
    Release(u32),
    /// `readback(slot)`.
    Readback(usize),
}

/// Shared state; the test keeps a clone of the handle.
#[derive(Debug, Default)]
pub struct FakeState {
    /// Every call, in order.
    pub calls: Vec<Call>,
    /// Write ends of the fences not yet signalled, oldest first.
    pub pending: Vec<OwnedFd>,
    /// Born-signalled fences.
    pub auto_signal: bool,
    /// Make the next import fail with a backend error.
    pub fail_next_import: bool,
}

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
            sampleable: vec![lin(XR24), lin(AR24), lin(NV12)],
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

    fn alloc_output_ring(&mut self, req: &RingRequest) -> Result<Ring, BackendError> {
        let modifier = req.modifiers[0];
        self.record(Call::Ring(req.n, modifier));
        self.ring = Some((req.w, req.h));
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
        out_idx: usize,
        clip: &[IRect],
        layers: &[(&u32, Layer)],
        acquire: Vec<OwnedFd>,
    ) -> Result<OwnedFd, BackendError> {
        self.record(Call::Composite(
            out_idx,
            clip.to_vec(),
            layers.iter().map(|(t, _)| **t).collect(),
            acquire.len(),
        ));
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
            .map_err(|e| err(&e.to_string()))?;
        let mut s = self.state();
        if !s.auto_signal {
            s.pending.push(w);
        }
        Ok(r)
    }

    fn release(&mut self, t: u32) {
        self.record(Call::Release(t));
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
