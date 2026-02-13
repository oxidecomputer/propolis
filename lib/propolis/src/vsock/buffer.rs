// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::num::NonZeroUsize;
use std::num::Wrapping;

#[derive(Debug, thiserror::Error)]
pub enum VsockBufError {
    #[error(
        "VsockBuf has {remaining} bytes available but tried to push {pushed}"
    )]
    InsufficientSpace { pushed: usize, remaining: usize },
}

/// A ringbuffer used to store guest -> host data
pub struct VsockBuf {
    buf: Box<[u8]>,
    head: Wrapping<usize>,
    tail: Wrapping<usize>,
}

impl std::fmt::Debug for VsockBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VsockBuf")
            .field("capacity", &self.capacity())
            .field("head", &self.head)
            .field("tail", &self.tail)
            .field("in_use", &self.len())
            .field("free", &self.free())
            .finish()
    }
}

impl VsockBuf {
    /// Create a new `VsockBuf`
    pub fn new(capacity: NonZeroUsize) -> Self {
        let capacity = capacity.get();
        Self {
            buf: vec![0; capacity].into_boxed_slice(),
            head: Wrapping(0),
            tail: Wrapping(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn len(&self) -> usize {
        (self.head - self.tail).0
    }

    fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    pub fn push(
        &mut self,
        data: impl AsRef<[u8]>,
    ) -> Result<(), VsockBufError> {
        let data = data.as_ref();

        if data.len() > self.free() {
            return Err(VsockBufError::InsufficientSpace {
                pushed: data.len(),
                remaining: self.free(),
            });
        }

        let head_offset = self.head.0 % self.buf.len();
        let available_len = self.buf.len() - head_offset;

        // If the data can fit in the remaining space of the ring buffer, copy
        // it in one go.
        if data.len() <= available_len {
            self.buf[head_offset..head_offset + data.len()]
                .copy_from_slice(&data);
        // Otherwise, split it and write the remaining data to the front.
        } else {
            let (fits, wrapped) = data.split_at(available_len);
            self.buf[head_offset..].copy_from_slice(fits);
            self.buf[..wrapped.len()].copy_from_slice(wrapped);
        }

        self.head += Wrapping(data.len());
        Ok(())
    }

    pub fn write_to<W: std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<usize> {
        // If we have no data to write, bail early
        if self.is_empty() {
            return Ok(0);
        }

        let tail_offset = self.tail.0 % self.buf.len();
        let head_offset = self.head.0 % self.buf.len();

        // If the data is contiguous, write it in one go
        let nwritten = if tail_offset < head_offset {
            writer.write(&self.buf[tail_offset..head_offset])?
        } else {
            // Data wraps around, so try to write it in batches
            let available_len = self.buf.len() - tail_offset;
            let nwritten = writer.write(&self.buf[tail_offset..])?;

            // If we failed to write the entire first segment, return early
            if nwritten < available_len {
                self.tail += Wrapping(nwritten);
                return Ok(nwritten);
            }

            // If we were successful, attempt to continue writing the wrapped
            // around segment
            let second_nwritten = writer.write(&self.buf[..head_offset])?;
            nwritten + second_nwritten
        };

        self.tail += Wrapping(nwritten);
        Ok(nwritten)
    }
}

#[cfg(test)]
mod test {
    use std::{io::Cursor, num::NonZeroUsize};

    use crate::vsock::buffer::VsockBuf;

    #[test]
    fn test_capacity_and_len() {
        let mut vb = VsockBuf::new(NonZeroUsize::new(10).unwrap());
        assert_eq!(vb.capacity(), 10);
        assert!(vb.is_empty());

        let data = vec![1; 8];
        let data_len = data.len();
        assert!(vb.push(data).is_ok());
        assert!(!vb.is_empty());
        assert_eq!(vb.capacity(), 10);
        assert_eq!(vb.len(), data_len);
    }

    #[test]
    fn test_push_less_than_capacity() {
        let mut vb = VsockBuf::new(NonZeroUsize::new(10).unwrap());
        let data = vec![1; 8];
        assert!(vb.push(data).is_ok());
    }

    #[test]
    fn test_push_more_than_capacity() {
        let mut vb = VsockBuf::new(NonZeroUsize::new(10).unwrap());
        let data = vec![1; 8];
        assert!(vb.push(data).is_ok());

        let data = vec![1; 8];
        assert!(vb.push(data).is_err());
    }

    #[test]
    fn test_write_to() {
        let mut vb = VsockBuf::new(NonZeroUsize::new(10).unwrap());
        let data = vec![1; 10];
        assert!(vb.push(data).is_ok());

        let mut some_socket = [1; 10];
        let mut cursor = Cursor::new(&mut some_socket[..]);
        assert!(vb.write_to(&mut cursor).is_ok_and(|n| n == 10));
    }

    #[test]
    fn test_partial_write_to() {
        let mut vb = VsockBuf::new(NonZeroUsize::new(10).unwrap());
        let data = vec![1; 10];
        assert!(vb.push(data).is_ok());

        let mut some_socket = [1; 5];
        let mut cursor = Cursor::new(&mut some_socket[..]);
        assert!(vb.write_to(&mut cursor).is_ok_and(|n| n == 5));
        assert_eq!(vb.len(), 5, "5 bytes remain");

        // reset the cursor and read another chunk
        cursor.set_position(0);
        assert!(vb.write_to(&mut cursor).is_ok_and(|n| n == 5));
        assert!(vb.is_empty());
    }

    #[test]
    fn test_wrap_around() {
        let mut vb = VsockBuf::new(NonZeroUsize::new(10).unwrap());
        let data = vec![1; 8];
        assert!(vb.push(data).is_ok());

        let mut some_socket = [1; 4];
        let mut cursor = Cursor::new(&mut some_socket[..]);
        assert!(vb.write_to(&mut cursor).is_ok_and(|n| n == 4));
        assert_eq!(some_socket, [1u8; 4]);

        let data = vec![2; 4];
        assert!(vb.push(data).is_ok());

        let mut some_socket = [1; 8];
        let mut cursor = Cursor::new(&mut some_socket[..]);
        assert!(vb.write_to(&mut cursor).is_ok_and(|n| n == 8));
        assert_eq!(some_socket, [1, 1, 1, 1, 2, 2, 2, 2]);
    }
}
