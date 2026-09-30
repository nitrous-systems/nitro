//! The node vocabulary: `PipeWire`'s model, in nitro-media's words.
//!
//! A [`Node`] is a handle that may live in the app ([`NodeKind::Local`]:
//! WAV, synthetic, tests), in a decode helper ([`NodeKind::Helper`]) or in
//! the `PipeWire` graph ([`NodeKind::Graph`]). Nodes have [`Port`]s with one
//! [`Format`] each; a [`LinkRequest`] asks to connect two ports and may be
//! refused or rerouted ([`LinkOutcome`]), because in `PipeWire` the session
//! manager decides routing. See `docs/media.md` ("Vocabulary").
//!
//! Phase 1 defines the types; the helper protocol (`crate::proto`) and the
//! remote source use them from phase 2 on.

use crate::frame::{HwDec, Nv12Layout};

/// Microseconds of media time.
pub type Micros = i64;

/// A node's id: unique per app for local and helper nodes, `PipeWire`'s
/// global id for graph nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Where a node lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// In the app's own process (WAV, synthetic, tests).
    Local,
    /// In one of the app's decode helpers.
    Helper,
    /// In the `PipeWire` graph (a device, another app's stream).
    Graph,
}

/// A handle to a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Its id.
    pub id: NodeId,
    /// Where it lives.
    pub kind: NodeKind,
    /// A name for people (`PipeWire`'s `node.description`, a file name).
    pub name: String,
}

/// Which way a port's data flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Into the node.
    In,
    /// Out of the node.
    Out,
}

/// One port of a node, with its format (wildcards until negotiated).
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    /// The node it belongs to.
    pub node: NodeId,
    /// Index among the node's ports of this direction.
    pub index: u32,
    /// In or out.
    pub direction: Direction,
    /// What it carries.
    pub format: Format,
}

/// A request to connect an output port to an input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkRequest {
    /// The output: node and port index.
    pub from: (NodeId, u32),
    /// The input: node and port index.
    pub to: (NodeId, u32),
}

/// What became of a [`LinkRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// Linked as asked.
    Linked,
    /// Refused, with the reason.
    Refused(String),
    /// The session manager linked the output somewhere else.
    Rerouted {
        /// Where it went.
        to: (NodeId, u32),
    },
}

/// A format field: a wildcard, or one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Choice<T> {
    /// Anything the other side offers.
    #[default]
    Any,
    /// Exactly this.
    Is(T),
}

impl<T: Copy + PartialEq> Choice<T> {
    /// The intersection of two choices: `Any` yields to the other, two
    /// values must agree. `None` when they conflict.
    #[must_use]
    pub fn meet(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::Any, o) | (o, Self::Any) => Some(o),
            (Self::Is(a), Self::Is(b)) => (a == b).then_some(Self::Is(a)),
        }
    }

    /// The value, when fixed.
    #[must_use]
    pub fn fixed(self) -> Option<T> {
        match self {
            Self::Any => None,
            Self::Is(v) => Some(v),
        }
    }
}

/// A pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pixel {
    /// NV12: luma plane, then interleaved `CbCr` at half resolution.
    Nv12,
}

/// A sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sample {
    /// Interleaved 32-bit float.
    F32,
    /// Planar 32-bit float.
    F32P,
    /// Interleaved signed 16-bit.
    S16,
}

/// A video port's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    /// Pixel format.
    pub pixel: Choice<Pixel>,
    /// Width × height in pixels.
    pub size: Choice<(u32, u32)>,
    /// How to decode and present (a preference, not negotiated).
    pub hw: HwDec,
}

/// An audio port's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    /// Sample format.
    pub sample: Choice<Sample>,
    /// Frames per second.
    pub rate: Choice<u32>,
    /// Channel count.
    pub channels: Choice<u8>,
}

/// What a port carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Video frames.
    Video(VideoFormat),
    /// Audio samples.
    Audio(AudioFormat),
}

impl VideoFormat {
    /// Field-wise [`Choice::meet`]; `hw` is taken from `self`.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Option<Self> {
        Some(Self {
            pixel: self.pixel.meet(other.pixel)?,
            size: self.size.meet(other.size)?,
            hw: self.hw,
        })
    }

    /// Whether every field is fixed.
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.pixel.fixed().is_some() && self.size.fixed().is_some()
    }
}

impl AudioFormat {
    /// Field-wise [`Choice::meet`].
    #[must_use]
    pub fn meet(&self, other: &Self) -> Option<Self> {
        Some(Self {
            sample: self.sample.meet(other.sample)?,
            rate: self.rate.meet(other.rate)?,
            channels: self.channels.meet(other.channels)?,
        })
    }

    /// Whether every field is fixed.
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.sample.fixed().is_some()
            && self.rate.fixed().is_some()
            && self.channels.fixed().is_some()
    }
}

impl Format {
    /// The intersection of two formats; `None` for different media or
    /// conflicting fields.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Option<Self> {
        match (self, other) {
            (Self::Video(a), Self::Video(b)) => a.meet(b).map(Self::Video),
            (Self::Audio(a), Self::Audio(b)) => a.meet(b).map(Self::Audio),
            _ => None,
        }
    }

    /// Whether negotiation is complete (no wildcard left).
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        match self {
            Self::Video(v) => v.is_fixed(),
            Self::Audio(a) => a.is_fixed(),
        }
    }
}

/// How buffers move between two linked ports; recycled, never reallocated
/// per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffers {
    /// A sealed memfd of `slots` NV12 frames the consumer allocates.
    MemfdRing {
        /// Frames in the ring.
        slots: u32,
        /// Each frame's layout.
        layout: Nv12Layout,
    },
    /// The producer's own dma-bufs, at most `max` of them.
    DmabufPool {
        /// Most surfaces the consumer registers.
        max: u32,
    },
}

/// One reading of a clock: at monotonic `mono_ns`, media time `media_us`
/// was being played, advancing at `rate`, and is heard `delay_ns` later
/// (`PipeWire`'s `spa_io_position` plus the stream's delay).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClockPoint {
    /// `CLOCK_MONOTONIC` nanoseconds of the reading.
    pub mono_ns: u64,
    /// Media position at `mono_ns`, microseconds.
    pub media_us: Micros,
    /// Media seconds per real second (1.0 when playing, 0.0 paused).
    pub rate: f64,
    /// How long after `mono_ns` that position reaches the listener.
    pub delay_ns: u64,
}

impl ClockPoint {
    /// The media time being heard at monotonic `mono_ns`.
    #[must_use]
    pub fn heard_at(&self, mono_ns: u64) -> Micros {
        let dt_ns = mono_ns as f64 - self.mono_ns as f64 - self.delay_ns as f64;
        self.media_us + (dt_ns * self.rate / 1000.0).round() as i64
    }
}

/// A read-only clock (the helper's clock page, or a local one).
pub trait Clock {
    /// The latest reading.
    fn now(&self) -> ClockPoint;
}

/// How a source runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Against a clock: late frames may be dropped.
    #[default]
    Realtime,
    /// Every frame, as fast as the consumer takes them (export, analysis).
    Offline,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(pixel: Choice<Pixel>, size: Choice<(u32, u32)>) -> Format {
        Format::Video(VideoFormat {
            pixel,
            size,
            hw: HwDec::Auto,
        })
    }

    #[test]
    fn choices_meet_through_wildcards() {
        assert_eq!(Choice::Any.meet(Choice::Is(3)), Some(Choice::Is(3)));
        assert_eq!(Choice::Is(3).meet(Choice::Any), Some(Choice::Is(3)));
        assert_eq!(Choice::<u8>::Any.meet(Choice::Any), Some(Choice::Any));
        assert_eq!(Choice::Is(3).meet(Choice::Is(3)), Some(Choice::Is(3)));
        assert_eq!(Choice::Is(3).meet(Choice::Is(4)), None);
        assert_eq!(Choice::Is(7).fixed(), Some(7));
        assert_eq!(Choice::<u8>::Any.fixed(), None);
    }

    #[test]
    fn formats_meet_field_by_field() {
        let want = video(Choice::Is(Pixel::Nv12), Choice::Any);
        let have = video(Choice::Any, Choice::Is((640, 360)));
        let got = want.meet(&have).unwrap();
        assert_eq!(got, video(Choice::Is(Pixel::Nv12), Choice::Is((640, 360))));
        assert!(got.is_fixed());
        assert!(!want.is_fixed());
        let other = video(Choice::Any, Choice::Is((320, 180)));
        assert_eq!(got.meet(&other), None, "sizes conflict");
        let audio = Format::Audio(AudioFormat {
            sample: Choice::Is(Sample::F32),
            rate: Choice::Any,
            channels: Choice::Is(2),
        });
        assert_eq!(want.meet(&audio), None, "video never meets audio");
        let dev = Format::Audio(AudioFormat {
            sample: Choice::Any,
            rate: Choice::Is(48_000),
            channels: Choice::Any,
        });
        assert!(audio.meet(&dev).unwrap().is_fixed());
    }

    #[test]
    fn the_heard_position_follows_rate_and_delay() {
        let c = ClockPoint {
            mono_ns: 1_000_000_000,
            media_us: 5_000_000,
            rate: 1.0,
            delay_ns: 20_000_000,
        };
        // 20 ms of delay: at the reading, the listener hears 20 ms earlier.
        assert_eq!(c.heard_at(1_000_000_000), 4_980_000);
        assert_eq!(c.heard_at(1_020_000_000), 5_000_000);
        let paused = ClockPoint { rate: 0.0, ..c };
        assert_eq!(paused.heard_at(9_000_000_000), 5_000_000);
        let fast = ClockPoint {
            rate: 2.0,
            delay_ns: 0,
            ..c
        };
        assert_eq!(fast.heard_at(1_500_000_000), 6_000_000);
    }
}
