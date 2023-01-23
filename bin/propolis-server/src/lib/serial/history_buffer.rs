// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Maintains a buffer of an instance's serial console data, holding both the
//! first mebibyte and the most recent mebibyte of console output.

use std::collections::VecDeque;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Requested byte offset is no longer cached: {0}")]
    ExpiredRange(usize),
}

const TTY_BUFFER_SIZE: usize = 1024 * 1024;
const DEFAULT_MAX_LENGTH: isize = 16 * 1024;

/// An abstraction for storing the contents of the instance's serial console
/// output, intended for retrieval by the web console or other monitoring or
/// troubleshooting tools.
pub(crate) struct HistoryBuffer {
    beginning: Vec<u8>,
    rolling: VecDeque<u8>,
    total_bytes: usize,
}

#[derive(Copy, Clone)]
pub(crate) enum SerialHistoryOffset {
    /// The byte index since instance start.
    FromStart(usize),
    /// The byte index *backwards* from the most recently buffered data.
    MostRecent(usize),
}

impl Default for HistoryBuffer {
    fn default() -> Self {
        Self::new(TTY_BUFFER_SIZE)
    }
}

impl HistoryBuffer {
    pub fn new(buffer_size: usize) -> Self {
        HistoryBuffer {
            beginning: Vec::with_capacity(buffer_size),
            rolling: VecDeque::with_capacity(buffer_size),
            total_bytes: 0,
        }
    }

    /// Feeds the buffer new bytes from the serial console.
    pub fn consume(&mut self, data: &[u8]) {
        let rolling_len = self.rolling.len();
        let headroom = self.rolling.capacity() - rolling_len;
        let read_size = data.len();
        if read_size > headroom {
            let to_capture = self.beginning.capacity() - self.beginning.len();
            let drain = self
                .rolling
                .drain(0..rolling_len.min(read_size - headroom))
                .take(to_capture);
            self.beginning.extend(drain);
        }
        self.rolling.extend(data);
        self.total_bytes += read_size;
    }

    /// Returns a tuple containing:
    /// - an iterator of serial console bytes from the live buffer.
    /// - the absolute byte index since instance start at which the iterator *begins*.
    pub fn contents_iter(
        &self,
        byte_offset: SerialHistoryOffset,
    ) -> Result<(Box<dyn Iterator<Item = u8> + '_>, usize), Error> {
        let (from_start, from_end) =
            self.offsets_from_start_and_end(byte_offset);

        // determine whether we should pull from beginning or rolling (or if we're straddling both)
        if self.total_bytes == self.rolling.len() + self.beginning.len() {
            // still contiguous
            Ok((
                Box::new(
                    self.beginning
                        .iter()
                        .chain(self.rolling.iter())
                        .skip(from_start)
                        .copied(),
                ),
                from_start,
            ))
        } else if from_start < self.beginning.len() {
            // requesting from beginning buffer
            Ok((
                Box::new(self.beginning.iter().copied().skip(from_start)),
                from_start,
            ))
        } else if from_end < self.rolling.len() {
            // (apologies to Takenobu Mitsuyoshi)
            let rolling_start = self.rolling.len() - from_end;
            Ok((
                Box::new(self.rolling.iter().copied().skip(rolling_start)),
                from_start,
            ))
        } else {
            Err(Error::ExpiredRange(from_start))
        }
    }

    /// Returns a tuple containing:
    /// - a `Vec` of the requested range of serial console bytes from the live buffer.
    /// - the absolute byte index since instance start at which the `Vec<u8>` *ends*.
    /// given a `byte_offset` indicating the index from which the returned `Vec<u8>` should start,
    /// and a `max_bytes` parameter, specifying a maximum length for the returned `Vec<u8>`, which
    /// will be `DEFAULT_MAX_LENGTH` if left unspecified.
    pub fn contents_vec(
        &self,
        byte_offset: SerialHistoryOffset,
        max_bytes: Option<usize>,
    ) -> Result<(Vec<u8>, usize), Error> {
        let (iter, from_start) = self.contents_iter(byte_offset)?;
        let data: Vec<u8> = iter
            .take(max_bytes.unwrap_or(DEFAULT_MAX_LENGTH as usize))
            .collect();
        let end_offset = from_start + data.len();
        Ok((data, end_offset))
    }

    fn offsets_from_start_and_end(
        &self,
        byte_offset: SerialHistoryOffset,
    ) -> (usize, usize) {
        match byte_offset {
            SerialHistoryOffset::FromStart(offset) => {
                if self.total_bytes > offset {
                    (offset, self.total_bytes - offset)
                } else {
                    // if asking for a byte offset we haven't reached yet, just start from the end.
                    (self.total_bytes, 0)
                }
            }
            SerialHistoryOffset::MostRecent(offset) => {
                if self.total_bytes > offset {
                    (self.total_bytes - offset, offset)
                } else {
                    // if asking for the most recent N > total_bytes, just start from the beginning.
                    (0, self.total_bytes)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SerialHistoryOffset::*;

    // for more legible assertions
    fn sugar(
        buf: &HistoryBuffer,
        byte_offset: SerialHistoryOffset,
        max_bytes: usize,
    ) -> (String, usize) {
        buf.contents_vec(byte_offset, Some(max_bytes))
            .map(|x| (String::from_utf8(x.0).expect("invalid utf-8"), x.1))
            .expect("serial range query failed")
    }

    #[test]
    fn test_continuous_buffer_range_abstraction() {
        let mut buf = HistoryBuffer::new(16);

        assert_eq!(buf.contents_vec(FromStart(0), None).unwrap(), (vec![], 0));
        assert_eq!(sugar(&buf, FromStart(0), 0), (String::new(), 0));
        assert_eq!(sugar(&buf, FromStart(0), 11), (String::new(), 0));
        assert_eq!(sugar(&buf, FromStart(11), 0), (String::new(), 0));
        assert_eq!(sugar(&buf, FromStart(11), 11), (String::new(), 0));

        let line = "This is an example of text.";
        let line_bytes = line.as_bytes().to_vec();

        buf.consume(&Vec::from(&line_bytes[..9]));
        assert_eq!(sugar(&buf, FromStart(8), 5), ("a".to_string(), 9));
        buf.consume(&Vec::from(&line_bytes[9..]));

        assert_eq!(
            buf.contents_vec(FromStart(0), None).unwrap(),
            (line_bytes, line.len())
        );
        assert_eq!(
            sugar(&buf, FromStart(0), line.len() + 10),
            (line.to_string(), line.len())
        );
        assert_eq!(sugar(&buf, FromStart(8), 5), ("an ex".to_string(), 13));
        assert_eq!(
            sugar(&buf, FromStart(100), 10),
            (String::new(), line.len())
        );
        assert_eq!(sugar(&buf, MostRecent(10), 4), ("e of".to_string(), 21));
        assert_eq!(
            sugar(&buf, MostRecent(10), 400),
            ("e of text.".to_string(), line.len())
        );
        assert_eq!(sugar(&buf, MostRecent(100), 4), ("This".to_string(), 4));

        buf.consume("\nNo thing beside remains.".as_bytes());
        assert_eq!(sugar(&buf, MostRecent(10), 4), ("e re".to_string(), 46));
        assert_eq!(sugar(&buf, FromStart(8), 8), ("an examp".to_string(), 16));
        assert_eq!(sugar(&buf, FromStart(8), 12), ("an examp".to_string(), 16));

        assert!(buf.contents_vec(FromStart(16), None).is_err());
    }
}
