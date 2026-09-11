//! Frame header layout and the incremental stream decoder.
//!
//! ```text
//! 0        4      6     7     8
//! +--------+------+-----+-----+---------------- ... ----+
//! | len:u32| op:u16|fds:u8|fl:u8|      payload (len)      |
//! +--------+------+-----+-----+---------------- ... ----+
//! ```
//!
//! `len` counts payload bytes only. `fds` is the number of descriptors
//! attached via `SCM_RIGHTS` to the `sendmsg` call that carried this
//! frame's *header*; the receiver binds them to this frame. `flags` is
//! reserved and must be zero.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use zerocopy::byteorder::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::error::DecodeError;
use crate::{MAX_FDS, MAX_PAYLOAD, MAX_PENDING_FDS};

/// Frame header: the exact 8 bytes on the wire.
pub mod header {
    use super::{DecodeError, FromBytes, Immutable, IntoBytes, KnownLayout, U16, U32, Unaligned};

    /// Header size in bytes.
    pub const SIZE: usize = 8;

    /// The `repr(C)` header. Little-endian, unaligned, 8 bytes.
    #[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
    #[repr(C)]
    pub struct FrameHeader {
        /// Payload length in bytes, header excluded.
        pub len: U32,
        /// Op code.
        pub op: U16,
        /// Number of attached file descriptors.
        pub fds: u8,
        /// Reserved; must be zero.
        pub flags: u8,
    }

    /// A decoded header.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Head {
        /// Payload length in bytes.
        pub len: u32,
        /// Op code.
        pub op: u16,
        /// Number of attached file descriptors.
        pub fds: u8,
    }

    /// Build the 8 header bytes.
    #[must_use]
    pub fn encode(len: u32, op: u16, fds: u8) -> [u8; SIZE] {
        let h = FrameHeader {
            len: U32::new(len),
            op: U16::new(op),
            fds,
            flags: 0,
        };
        let mut out = [0u8; SIZE];
        out.copy_from_slice(h.as_bytes());
        out
    }

    /// Parse 8 header bytes.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] if `bytes` is shorter than [`SIZE`],
    /// [`DecodeError::TooLarge`] if `len` or `fds` exceed the protocol
    /// maxima, [`DecodeError::BadFlags`] if a reserved flag bit is set.
    pub fn decode(bytes: &[u8]) -> Result<Head, DecodeError> {
        let h = FrameHeader::ref_from_bytes(bytes.get(..SIZE).ok_or(DecodeError::Truncated)?)
            .map_err(|_| DecodeError::Truncated)?;
        if h.flags != 0 {
            return Err(DecodeError::BadFlags);
        }
        let len = h.len.get();
        if len as usize > super::MAX_PAYLOAD {
            return Err(DecodeError::TooLarge);
        }
        if h.fds as usize > super::MAX_FDS {
            return Err(DecodeError::TooLarge);
        }
        Ok(Head {
            len,
            op: h.op.get(),
            fds: h.fds,
        })
    }
}

pub use header::{FrameHeader, Head};

/// One complete frame lifted off the stream.
#[derive(Debug)]
pub struct Frame {
    /// Op code.
    pub op: u16,
    /// Payload bytes, header excluded.
    pub payload: Vec<u8>,
    /// The descriptors that arrived with this frame's header.
    pub fds: Vec<OwnedFd>,
}

/// Incremental decoder for a byte stream carrying frames plus out-of-band
/// descriptors.
///
/// Feed it whatever `recvmsg` returned — bytes and the fds of that one
/// call — and pull complete frames out with [`Framer::next_frame`].
///
/// **fd binding.** A stream socket gives no guarantee that a `recvmsg`
/// boundary matches a frame boundary, so fds cannot be tied to a syscall.
/// They are tied to a *byte position*: an fd received with a chunk belongs
/// to the frame whose header byte offset is at or after the start of that
/// chunk. Concretely, the framer records "at stream offset X, N fds
/// arrived"; when it finishes a header that started at offset H, it claims
/// every fd recorded at an offset `< H + header::SIZE`, i.e. every fd that
/// arrived no later than the chunk completing that header. That is exactly
/// the sender's rule — attach the fds to the `sendmsg` carrying the header
/// — read back on the receiving side.
///
/// **Bounded.** A peer that attaches descriptors to every `sendmsg` while
/// declaring `fds: 0` in every header would otherwise park an `OwnedFd`
/// per call here forever and walk the process into `EMFILE`, taking every
/// other client down with it. The pending queue is therefore capped at
/// [`MAX_PENDING_FDS`]; exceeding it is a fatal
/// [`DecodeError::UnexpectedFd`].
#[derive(Debug, Default)]
pub struct Framer {
    /// Unconsumed bytes.
    buf: Vec<u8>,
    /// How much of `buf` has been turned into frames.
    start: usize,
    /// Stream offset of `buf[0]`.
    base: u64,
    /// Fds, each tagged with the stream offset of the chunk it arrived in.
    fds: VecDeque<(u64, OwnedFd)>,
    /// Total bytes fed, for tagging incoming fds.
    fed: u64,
    /// Set when more fds arrived than any frame can claim.
    fd_flood: bool,
    /// The error that poisoned the stream, if any.
    poison: Option<DecodeError>,
}

impl Framer {
    /// A new, empty framer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes and the descriptors that came with them.
    ///
    /// Descriptors beyond [`MAX_PENDING_FDS`] are dropped (closed) and the
    /// framer is marked so the next [`Framer::next_frame`] fails: a peer
    /// cannot make us hold an unbounded number of open descriptors.
    pub fn feed(&mut self, bytes: &[u8], fds: impl IntoIterator<Item = OwnedFd>) {
        // Tag fds with the offset of the *first* byte of this chunk: they
        // belong to the frame whose header completes within it.
        let at = self.fed;
        for fd in fds {
            if self.fds.len() >= MAX_PENDING_FDS {
                // Dropping closes it, which is what we want: the peer is
                // about to be disconnected anyway.
                self.fd_flood = true;
                drop(fd);
                continue;
            }
            self.fds.push_back((at, fd));
        }
        self.buf.extend_from_slice(bytes);
        self.fed += bytes.len() as u64;
        self.compact();
    }

    /// Whether any byte is buffered but not yet a complete frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start >= self.buf.len()
    }

    /// Buffered, not yet framed bytes.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Unclaimed descriptors still waiting for their frame.
    #[must_use]
    pub fn pending_fds(&self) -> usize {
        self.fds.len()
    }

    /// Whether a complete frame is already buffered, or the framer has an
    /// error to report.
    ///
    /// Exact, not a byte-count heuristic: a *partial* trailing frame must
    /// not count, or a peer that hangs up mid-frame would leave the
    /// receiver believing there is always something left to decode.
    #[must_use]
    pub fn has_frame(&self) -> bool {
        if self.poison.is_some() || self.fd_flood {
            return true;
        }
        let avail = &self.buf[self.start..];
        match header::decode(avail) {
            Ok(h) => avail.len() >= header::SIZE + h.len as usize,
            // A malformed header is a complete frame's worth of bad news.
            Err(DecodeError::Truncated) => false,
            Err(_) => true,
        }
    }

    /// Pull the next complete frame.
    ///
    /// Returns `Ok(None)` when more bytes are needed.
    ///
    /// # Errors
    /// [`DecodeError::TooLarge`] for an oversize `len` or fd count,
    /// [`DecodeError::BadFlags`] for a reserved flag,
    /// [`DecodeError::MissingFd`] when a frame declares more descriptors
    /// than arrived with it, and [`DecodeError::UnexpectedFd`] when the
    /// peer sent more unclaimed descriptors than [`MAX_PENDING_FDS`].
    /// Any error poisons the framer: every later call returns that same
    /// error, because a stream that desynchronised cannot be
    /// resynchronised.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        if let Some(e) = self.poison {
            return Err(e);
        }
        match self.try_next() {
            Err(e) => {
                self.poison = Some(e);
                Err(e)
            }
            ok => ok,
        }
    }

    fn try_next(&mut self) -> Result<Option<Frame>, DecodeError> {
        if self.fd_flood {
            return Err(DecodeError::UnexpectedFd);
        }
        let avail = &self.buf[self.start..];
        if avail.len() < header::SIZE {
            return Ok(None);
        }
        let head = header::decode(avail)?;
        let total = header::SIZE + head.len as usize;
        if avail.len() < total {
            return Ok(None);
        }
        let payload = avail[header::SIZE..total].to_vec();
        // Stream offset one past this frame's header.
        let header_end = self.base + self.start as u64 + header::SIZE as u64;
        let mut fds = Vec::with_capacity(head.fds as usize);
        for _ in 0..head.fds {
            match self.fds.front() {
                Some(&(at, _)) if at < header_end => {
                    let (_, fd) = self.fds.pop_front().expect("front peeked above");
                    fds.push(fd);
                }
                _ => return Err(DecodeError::MissingFd),
            }
        }
        self.start += total;
        self.compact();
        Ok(Some(Frame {
            op: head.op,
            payload,
            fds,
        }))
    }

    /// Drop consumed bytes once they are a worthwhile fraction of the
    /// buffer, keeping the allocation for reuse.
    fn compact(&mut self) {
        if self.start == 0 {
            return;
        }
        if self.start == self.buf.len() {
            self.base += self.start as u64;
            self.buf.clear();
            self.start = 0;
        } else if self.start >= 64 * 1024 || self.start * 2 >= self.buf.len() {
            self.buf.drain(..self.start);
            self.base += self.start as u64;
            self.start = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(op: u16, payload: &[u8], fds: u8) -> Vec<u8> {
        let mut v = header::encode(payload.len() as u32, op, fds).to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn header_round_trip() {
        let h = header::encode(16, 0x0201, 2);
        assert_eq!(h, [16, 0, 0, 0, 0x01, 0x02, 2, 0]);
        let d = header::decode(&h).unwrap();
        assert_eq!(
            d,
            Head {
                len: 16,
                op: 0x0201,
                fds: 2
            }
        );
    }

    #[test]
    fn header_rejects_bad_fields() {
        assert_eq!(header::decode(&[0; 7]), Err(DecodeError::Truncated));
        let mut h = header::encode(0, 1, 0);
        h[7] = 1;
        assert_eq!(header::decode(&h), Err(DecodeError::BadFlags));
        let h = header::encode(MAX_PAYLOAD as u32 + 1, 1, 0);
        assert_eq!(header::decode(&h), Err(DecodeError::TooLarge));
        let h = header::encode(0, 1, MAX_FDS as u8 + 1);
        assert_eq!(header::decode(&h), Err(DecodeError::TooLarge));
    }

    #[test]
    fn framer_splits_a_stream() {
        let mut stream = frame_bytes(1, b"abc", 0);
        stream.extend_from_slice(&frame_bytes(2, b"", 0));
        stream.extend_from_slice(&frame_bytes(3, b"0123456789", 0));

        let mut f = Framer::new();
        f.feed(&stream, []);
        let a = f.next_frame().unwrap().unwrap();
        assert_eq!((a.op, a.payload.as_slice()), (1, b"abc".as_slice()));
        let b = f.next_frame().unwrap().unwrap();
        assert_eq!((b.op, b.payload.len()), (2, 0));
        let c = f.next_frame().unwrap().unwrap();
        assert_eq!((c.op, c.payload.as_slice()), (3, b"0123456789".as_slice()));
        assert!(f.next_frame().unwrap().is_none());
        assert!(f.is_empty());
    }

    #[test]
    fn framer_accepts_byte_at_a_time() {
        let mut stream = frame_bytes(7, b"hello world", 0);
        stream.extend_from_slice(&frame_bytes(8, b"!", 0));

        let mut f = Framer::new();
        let mut got = Vec::new();
        for b in &stream {
            f.feed(std::slice::from_ref(b), []);
            while let Some(fr) = f.next_frame().unwrap() {
                got.push((fr.op, fr.payload));
            }
        }
        assert_eq!(got, vec![(7, b"hello world".to_vec()), (8, b"!".to_vec())]);
    }

    #[test]
    fn framer_rejects_oversize_len() {
        let mut f = Framer::new();
        f.feed(&header::encode(MAX_PAYLOAD as u32 + 1, 1, 0), []);
        assert_eq!(f.next_frame().unwrap_err(), DecodeError::TooLarge);
        // Poisoned from here on.
        assert!(f.next_frame().is_err());
    }
}
