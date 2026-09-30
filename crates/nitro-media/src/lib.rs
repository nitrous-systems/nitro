//! `nitro-media`: the media library behind `nitro-video` (and, later,
//! `nitro-amp`). Design: `docs/media.md`.
//!
//! | module | what |
//! |---|---|
//! | [`frame`] | NV12 layout, stream info, frame buffers (shm slot or dma-buf), hardware facts |
//! | [`source`] | [`VideoSource`], the trait a video backend implements |
//! | [`fake`] | [`SyntheticDecoder`], a source that makes frames up (tests, `--synthetic`) |
//! | [`node`] | the vocabulary: node, port, link request, format with wildcards, buffers, clock |
//! | [`proto`] | the app ↔ decode-helper messages on `nitro-wire` framing |
//! | [`validate`] | helper replies checked as hostile input |
//!
//! Phase 1 (#3988): the library only. `nitro-video` still runs `FFmpeg`
//! in-process behind [`VideoSource`]; the helper process arrives in
//! phase 2. This crate links no C and has no `unsafe`.

pub mod fake;
pub mod frame;
pub mod node;
pub mod proto;
pub mod source;
pub mod validate;

pub use fake::SyntheticDecoder;
pub use frame::{
    DmabufDesc, DmabufFrame, DmabufPlane, FrameBuf, HwDec, HwInfo, Matrix, Nv12Layout, StreamInfo,
};
pub use node::{
    AudioFormat, Buffers, Choice, Clock, ClockPoint, Direction, Format, LinkOutcome, LinkRequest,
    Micros, Node, NodeId, NodeKind, Pixel, Port, RunMode, Sample, VideoFormat,
};
pub use source::VideoSource;
