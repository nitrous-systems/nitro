//! `nitro-video`: a native video player (#3906).
//!
//! MP4/H.264 demuxed in-process ([`mp4`], [`annexb`]), decoded by an
//! `ffmpeg` child ([`ffmpeg`]) behind the [`decode::Decoder`] seam, and
//! presented as NV12 frames on a `Surface` node ([`player`]), paced by
//! [`pacing`], with nitro-ui controls over the video ([`controls`]).

pub mod annexb;
pub mod decode;
pub mod ffmpeg;
pub mod mp4;
