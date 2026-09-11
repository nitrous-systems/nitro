//! The buffer pair: [`Writer`] (bytes + fds to send) and [`Reader`]
//! (bounds-checked cursor over a received payload).
//!
//! `Writer` owns the outgoing byte buffer and the fds queued with it;
//! `Reader` never allocates except for the `String`/`Vec` it hands back.
//! Neither ever panics on hostile input.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::error::{DecodeError, EncodeError};
use crate::wire::Plain;
use crate::{MAX_FDS, MAX_PAYLOAD, header};

/// A descriptor queued for sending, tagged with the byte offset (from the
/// front of the buffer) one past the end of *its frame's header*.
///
/// The tag is what makes fd placement correct across partial writes: the
/// `sendmsg` carrying these fds must include the frame's header, and no
/// other fd-carrying frame may share that call. [`Writer::consume`] shifts
/// the tags as bytes leave, so the invariant survives any number of short
/// writes.
#[derive(Debug)]
struct QueuedFd {
    /// Bytes that must be written for this frame's header to be complete.
    at: usize,
    fd: OwnedFd,
}

/// A reusable outgoing buffer: framed bytes plus the fds that go with them.
///
/// Frames are written with [`Writer::frame`], which reserves the 8-byte
/// header, runs the body closure and patches the length and fd count in
/// afterwards. A body that overflows [`MAX_PAYLOAD`] truncates the buffer
/// back to the frame boundary, so a failed encode never corrupts the
/// stream.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
    fds: VecDeque<QueuedFd>,
    /// Fd count of the frame currently being written.
    frame_fds: u8,
    /// Byte offset just past the header of the frame being written.
    frame_body: usize,
}

impl Writer {
    /// An empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queued bytes, header included.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Whether there is nothing queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.fds.is_empty()
    }

    /// Number of queued bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Number of queued file descriptors.
    #[must_use]
    pub fn fd_count(&self) -> usize {
        self.fds.len()
    }

    /// Drop every queued byte and fd.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.fds.clear();
        self.frame_fds = 0;
        self.frame_body = 0;
    }

    /// Take the queued bytes and fds, leaving the writer empty and its
    /// allocation intact.
    pub fn take(&mut self) -> (Vec<u8>, Vec<OwnedFd>) {
        let fds = std::mem::take(&mut self.fds)
            .into_iter()
            .map(|q| q.fd)
            .collect();
        self.frame_fds = 0;
        self.frame_body = 0;
        (std::mem::take(&mut self.buf), fds)
    }

    /// Write one frame: reserve the header, run `body`, patch the header.
    ///
    /// # Errors
    /// Whatever `body` returns, [`EncodeError::TooLarge`] if the body
    /// exceeds [`MAX_PAYLOAD`], or [`EncodeError::TooManyFds`] if it
    /// attaches more than [`MAX_FDS`] descriptors. In every case the
    /// buffer is rolled back to where the frame started and any fds the
    /// body pushed are dropped, so a failed encode never desynchronises
    /// the stream.
    pub fn frame<F>(&mut self, op: u16, body: F) -> Result<(), EncodeError>
    where
        F: FnOnce(&mut Self) -> Result<(), EncodeError>,
    {
        let start = self.buf.len();
        let fd_start = self.fds.len();
        let outer_fds = self.frame_fds;
        let outer_body = self.frame_body;
        self.frame_fds = 0;
        self.buf.extend_from_slice(&[0u8; header::SIZE]);
        self.frame_body = self.buf.len();
        let result = body(self);
        let payload = self.buf.len() - start - header::SIZE;
        let fds = self.frame_fds;
        self.frame_fds = outer_fds;
        self.frame_body = outer_body;
        let err = match result {
            Err(e) => Some(e),
            Ok(()) if payload > MAX_PAYLOAD => Some(EncodeError::TooLarge),
            Ok(()) if fds as usize > MAX_FDS => Some(EncodeError::TooManyFds),
            Ok(()) => None,
        };
        if let Some(e) = err {
            self.buf.truncate(start);
            self.fds.truncate(fd_start);
            return Err(e);
        }
        let head = header::encode(payload as u32, op, fds);
        self.buf[start..start + header::SIZE].copy_from_slice(&head);
        Ok(())
    }

    /// Append a fixed-layout body struct.
    pub fn put_struct<T>(&mut self, value: &T)
    where
        T: IntoBytes + Immutable,
    {
        self.buf.extend_from_slice(value.as_bytes());
    }

    /// Append one [`Plain`] value.
    pub fn put<T: Plain>(&mut self, value: T) {
        self.buf.extend_from_slice(value.to_wire().as_bytes());
    }

    /// Append a raw byte.
    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Append a little-endian `u16`.
    pub fn put_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u32`.
    pub fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `i32`.
    pub fn put_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u64`.
    pub fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `f32`.
    pub fn put_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a `str`: `u32` byte length then UTF-8, no NUL terminator.
    ///
    /// A string longer than [`MAX_PAYLOAD`] is truncated at a char
    /// boundary; the frame it belongs to then almost certainly fails with
    /// [`EncodeError::TooLarge`], which is the intended outcome.
    pub fn put_str(&mut self, s: &str) {
        let mut bytes = s.as_bytes();
        if bytes.len() > MAX_PAYLOAD {
            let mut end = MAX_PAYLOAD;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            bytes = &bytes[..end];
        }
        self.put_u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    /// Append a `bytes` field: `u32` length then the bytes.
    pub fn put_bytes(&mut self, b: &[u8]) {
        self.put_u32(b.len().min(MAX_PAYLOAD) as u32);
        self.buf.extend_from_slice(&b[..b.len().min(MAX_PAYLOAD)]);
    }

    /// Append a `vec<T>` of [`Plain`] items: `u32` count then the items.
    pub fn put_vec<T: Plain>(&mut self, items: &[T]) {
        self.put_u32(items.len() as u32);
        for it in items {
            self.put(*it);
        }
    }

    /// Attach a file descriptor to the frame being written.
    ///
    /// The fd is sent with the `sendmsg` that carries this frame's header.
    /// Calling this outside [`Writer::frame`] attaches it to the start of
    /// the buffer, which is only meaningful for tests.
    pub fn put_fd(&mut self, fd: OwnedFd) {
        self.fds.push_back(QueuedFd {
            at: self.frame_body,
            fd,
        });
        self.frame_fds = self.frame_fds.saturating_add(1);
    }

    /// How this buffer must be split into `sendmsg` calls: the largest
    /// chunk that may go out now, and how many leading queued fds ride
    /// with it.
    ///
    /// At most one frame's descriptors per call, and the call always
    /// includes that frame's header — the two rules the receiver's fd
    /// binding relies on.
    pub(crate) fn send_chunk(&self) -> (usize, usize) {
        let Some(first) = self.fds.front() else {
            return (self.buf.len(), 0);
        };
        // Every fd of the same frame shares its tag.
        let count = self.fds.iter().take_while(|q| q.at == first.at).count();
        match self.fds.iter().find(|q| q.at != first.at) {
            // Stop before the next fd-carrying frame's header.
            Some(next) => (next.at.saturating_sub(header::SIZE), count),
            None => (self.buf.len(), count),
        }
    }

    /// Borrow the first `n` queued descriptors, for the ancillary buffer.
    pub(crate) fn borrow_fds(&self, n: usize) -> Vec<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd as _;
        self.fds.iter().take(n).map(|q| q.fd.as_fd()).collect()
    }

    /// Drop `n` bytes from the front (consumed by a partial write) and
    /// shift the fd tags to match.
    pub(crate) fn consume(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        self.buf.drain(..n);
        for q in &mut self.fds {
            q.at = q.at.saturating_sub(n);
        }
        self.frame_body = self.frame_body.saturating_sub(n);
    }

    /// Drop `n` fds from the front (consumed by a write).
    pub(crate) fn consume_fds(&mut self, n: usize) {
        for _ in 0..n.min(self.fds.len()) {
            self.fds.pop_front();
        }
    }
}

/// A bounds-checked cursor over one message payload.
///
/// Every `get_*` either returns a value or a [`DecodeError`]; none of them
/// can panic, whatever the bytes are.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `buf`.
    #[must_use]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the payload is fully consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Require that nothing is left.
    ///
    /// # Errors
    /// [`DecodeError::Trailing`] when bytes remain.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }

    /// Take `n` raw bytes.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when fewer than `n` bytes remain.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        if end > self.buf.len() {
            return Err(DecodeError::Truncated);
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// Decode a fixed-layout struct in place.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when the payload is too short.
    pub fn get_struct<T>(&mut self) -> Result<T, DecodeError>
    where
        T: FromBytes + KnownLayout + Immutable + Unaligned + Copy,
    {
        let bytes = self.take(size_of::<T>())?;
        T::read_from_bytes(bytes).map_err(|_| DecodeError::Truncated)
    }

    /// Decode one [`Plain`] value.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] or [`DecodeError::BadValue`].
    pub fn get<T: Plain>(&mut self) -> Result<T, DecodeError> {
        let w = self.get_struct::<T::Wire>()?;
        T::from_wire(w)
    }

    /// Read a `u8`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`].
    pub fn get_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    /// Read a little-endian `u16`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`].
    pub fn get_u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`].
    pub fn get_u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little-endian `i32`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`].
    pub fn get_i32(&mut self) -> Result<i32, DecodeError> {
        Ok(self.get_u32()?.cast_signed())
    }

    /// Read a little-endian `u64`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`].
    pub fn get_u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a little-endian `f32`.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`].
    pub fn get_f32(&mut self) -> Result<f32, DecodeError> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a `str` field: `u32` length then UTF-8.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] when short, [`DecodeError::TooLarge`]
    /// when the length exceeds [`MAX_PAYLOAD`], [`DecodeError::BadUtf8`]
    /// for invalid UTF-8 or an embedded NUL.
    pub fn get_str(&mut self) -> Result<String, DecodeError> {
        let len = self.get_u32()? as usize;
        if len > MAX_PAYLOAD {
            return Err(DecodeError::TooLarge);
        }
        let bytes = self.take(len)?;
        if bytes.contains(&0) {
            return Err(DecodeError::BadUtf8);
        }
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| DecodeError::BadUtf8)
    }

    /// Read a `bytes` field: `u32` length then the bytes.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`] or [`DecodeError::TooLarge`].
    pub fn get_bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.get_u32()? as usize;
        if len > MAX_PAYLOAD {
            return Err(DecodeError::TooLarge);
        }
        Ok(self.take(len)?.to_vec())
    }

    /// Read a `vec<T>` of [`Plain`] items.
    ///
    /// The count is validated against the bytes actually available before
    /// anything is allocated, so a hostile count cannot make us reserve
    /// gigabytes.
    ///
    /// # Errors
    /// [`DecodeError::Truncated`], [`DecodeError::TooLarge`] or
    /// [`DecodeError::BadValue`].
    pub fn get_vec<T: Plain>(&mut self) -> Result<Vec<T>, DecodeError> {
        let count = self.get_u32()? as usize;
        let item = size_of::<T::Wire>();
        let need = count.checked_mul(item).ok_or(DecodeError::TooLarge)?;
        if need > self.remaining() {
            return Err(DecodeError::Truncated);
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.get::<T>()?);
        }
        Ok(out)
    }
}

/// The fds that arrived with a frame, handed to `decode` in order.
///
/// Messages take the fds they declare; anything left when the message is
/// decoded is [`DecodeError::UnexpectedFd`].
#[derive(Debug, Default)]
pub struct FdQueue {
    fds: VecDeque<OwnedFd>,
}

impl FdQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A queue over `fds`, claimed in order.
    #[must_use]
    pub fn from_vec(fds: Vec<OwnedFd>) -> Self {
        Self { fds: fds.into() }
    }

    /// How many fds are still unclaimed.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.fds.len()
    }

    /// Whether every fd has been claimed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }

    /// Take the next fd.
    ///
    /// # Errors
    /// [`DecodeError::MissingFd`] when the queue is empty.
    pub fn take(&mut self) -> Result<OwnedFd, DecodeError> {
        self.fds.pop_front().ok_or(DecodeError::MissingFd)
    }

    /// Require that no fd is left over.
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedFd`] when fds remain.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::UnexpectedFd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_core::Rect;

    #[test]
    fn writer_frames_and_patches_the_header() {
        let mut w = Writer::new();
        w.frame(0x0123, |w| {
            w.put_u32(7);
            w.put_str("hi");
            Ok(())
        })
        .unwrap();
        let bytes = w.bytes();
        assert_eq!(bytes.len(), header::SIZE + 4 + 4 + 2);
        let h = header::decode(&bytes[..header::SIZE]).unwrap();
        assert_eq!(h.op, 0x0123);
        assert_eq!(h.len, 10);
        assert_eq!(h.fds, 0);
    }

    #[test]
    fn reader_round_trips_primitives() {
        let mut w = Writer::new();
        w.put_u8(1);
        w.put_u16(0x0203);
        w.put_u32(0x0405_0607);
        w.put_i32(-2);
        w.put_u64(0x0809_0a0b_0c0d_0e0f);
        w.put_f32(1.5);
        w.put_str("héllo");
        w.put_bytes(&[1, 2, 3]);
        w.put_vec(&[Rect::new(1.0, 2.0, 3.0, 4.0)]);
        let bytes = w.bytes().to_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.get_u8().unwrap(), 1);
        assert_eq!(r.get_u16().unwrap(), 0x0203);
        assert_eq!(r.get_u32().unwrap(), 0x0405_0607);
        assert_eq!(r.get_i32().unwrap(), -2);
        assert_eq!(r.get_u64().unwrap(), 0x0809_0a0b_0c0d_0e0f);
        assert_eq!(r.get_f32().unwrap().to_bits(), 1.5f32.to_bits());
        assert_eq!(r.get_str().unwrap(), "héllo");
        assert_eq!(r.get_bytes().unwrap(), vec![1, 2, 3]);
        assert_eq!(
            r.get_vec::<Rect>().unwrap(),
            vec![Rect::new(1.0, 2.0, 3.0, 4.0)]
        );
        assert!(r.finish().is_ok());
    }

    #[test]
    fn reader_rejects_hostile_input() {
        // A huge vec count with no bytes behind it must not allocate.
        let mut w = Writer::new();
        w.put_u32(u32::MAX);
        let bytes = w.bytes().to_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.get_vec::<Rect>(), Err(DecodeError::Truncated));

        // Bad UTF-8 and embedded NUL.
        let mut w = Writer::new();
        w.put_u32(2);
        w.buf.extend_from_slice(&[0xff, 0xfe]);
        let bytes = w.bytes().to_vec();
        assert_eq!(Reader::new(&bytes).get_str(), Err(DecodeError::BadUtf8));

        let mut w = Writer::new();
        w.put_u32(2);
        w.buf.extend_from_slice(&[b'a', 0]);
        let bytes = w.bytes().to_vec();
        assert_eq!(Reader::new(&bytes).get_str(), Err(DecodeError::BadUtf8));

        // Oversize declared length.
        let mut w = Writer::new();
        w.put_u32(MAX_PAYLOAD as u32 + 1);
        let bytes = w.bytes().to_vec();
        assert_eq!(Reader::new(&bytes).get_str(), Err(DecodeError::TooLarge));
        assert_eq!(Reader::new(&bytes).get_bytes(), Err(DecodeError::TooLarge));
    }

    #[test]
    fn empty_reader_is_truncated_not_panicking() {
        let mut r = Reader::new(&[]);
        assert_eq!(r.get_u8(), Err(DecodeError::Truncated));
        assert_eq!(r.get_u64(), Err(DecodeError::Truncated));
        assert_eq!(r.take(usize::MAX), Err(DecodeError::Truncated));
    }
}
