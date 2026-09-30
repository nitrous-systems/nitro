//! `nitro-video`: a native video player (#3906).
//!
//! The system `FFmpeg` demuxes and decodes ([`ffmpeg`], behind the
//! [`decode::VideoSource`] seam). A decode thread writes NV12 frames into a
//! ring of shared-memory buffers, and [`player`] presents them on a
//! `Surface` node, paced against the server's frame callbacks
//! ([`pacing`]), with nitro-ui controls over the video ([`controls`]).

pub mod controls;
pub mod decode;
pub mod ffmpeg;
pub mod pacing;
pub mod player;
