//! Small stream adapters for the body layer (spec 4.1): the body is the packed
//! data followed by the 32-byte SHA-256 of the tar stream.

use std::io::{self, Read, Write};

use sha2::{Digest, Sha256};

/// Length of the SHA-256 that ends the body.
pub(crate) const HASH_LEN: usize = 32;

/// Passes bytes through to `inner` and hashes them on the way.
pub(crate) struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// The hash of everything written, and the inner writer.
    pub(crate) fn finish(self) -> ([u8; HASH_LEN], W) {
        (self.hasher.finalize().into(), self.inner)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let count = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Passes bytes through from `inner` and hashes them on the way.
pub(crate) struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        HashingReader {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// The hash of everything read so far.
    pub(crate) fn finish(self) -> [u8; HASH_LEN] {
        self.hasher.finalize().into()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..count]);
        Ok(count)
    }
}

/// Reads a body but holds back its last 32 bytes, which are the stored hash
/// and not part of the packed data.
///
/// The end of the stream is only known when it arrives, so this always keeps
/// the newest 32 bytes in hand and releases the ones before them.
pub(crate) struct TrailerReader<R> {
    inner: R,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
    ended: bool,
}

impl<R: Read> TrailerReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        TrailerReader {
            inner,
            buffer: vec![0u8; 64 * 1024 + HASH_LEN],
            start: 0,
            end: 0,
            ended: false,
        }
    }

    /// The held-back hash. `None` until the stream has been read to its end,
    /// and `None` if the whole stream was shorter than a hash.
    pub(crate) fn trailer(&self) -> Option<[u8; HASH_LEN]> {
        if !self.ended {
            return None;
        }
        self.buffer[self.start..self.end].try_into().ok()
    }
}

impl<R: Read> Read for TrailerReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let ready = (self.end - self.start).saturating_sub(HASH_LEN);
            if ready > 0 {
                let count = ready.min(out.len());
                out[..count].copy_from_slice(&self.buffer[self.start..self.start + count]);
                self.start += count;
                return Ok(count);
            }
            if self.ended {
                return Ok(0);
            }
            // Slide what is held to the front and read more behind it.
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
            match self.inner.read(&mut self.buffer[self.end..]) {
                Ok(0) => self.ended = true,
                Ok(count) => self.end += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::noise;

    /// A reader that hands out its data in awkward little pieces.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let count = self.step.min(out.len()).min(self.data.len());
            out[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            Ok(count)
        }
    }

    #[test]
    fn hashing_matches_sha256_of_the_bytes() {
        let data = noise(100_000, 1);
        let expected: [u8; 32] = Sha256::digest(&data).into();

        let mut writer = HashingWriter::new(Vec::new());
        writer.write_all(&data).unwrap();
        let (hash, written) = writer.finish();
        assert_eq!(hash, expected);
        assert_eq!(written, data);

        let mut reader = HashingReader::new(&data[..]);
        let mut read_back = Vec::new();
        reader.read_to_end(&mut read_back).unwrap();
        assert_eq!(reader.finish(), expected);
        assert_eq!(read_back, data);
    }

    #[test]
    fn known_sha256_value() {
        let (hash, _) = HashingWriter::new(io::sink()).finish();
        // SHA-256 of nothing.
        assert_eq!(hash[..4], [0xe3, 0xb0, 0xc4, 0x42]);
    }

    #[test]
    fn the_last_32_bytes_are_held_back() {
        for total in [32usize, 33, 64, 1000, 64 * 1024 + 31, 200_000] {
            let data = noise(total, total as u64);
            for step in [1usize, 7, 32, 33, 5000, usize::MAX] {
                if total > 5000 && step < 32 {
                    continue; // same code path as the small totals, just slow
                }
                let mut reader = TrailerReader::new(Trickle { data: &data, step });
                assert!(reader.trailer().is_none(), "not known before the end");
                let mut front = Vec::new();
                reader.read_to_end(&mut front).unwrap();
                assert_eq!(front, &data[..total - 32], "total {total} step {step}");
                assert_eq!(reader.trailer().unwrap(), &data[total - 32..]);
            }
        }
    }

    #[test]
    fn a_stream_shorter_than_a_hash_has_no_trailer() {
        for total in [0usize, 1, 31] {
            let data = noise(total, 3);
            let mut reader = TrailerReader::new(&data[..]);
            let mut front = Vec::new();
            reader.read_to_end(&mut front).unwrap();
            assert!(front.is_empty());
            assert!(reader.trailer().is_none());
        }
    }
}
