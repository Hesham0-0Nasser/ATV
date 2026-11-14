//! The frame block carried by each data frame (spec 5.4): a 16-byte block
//! header, the payload, then zero padding.

/// Size of the block header in front of the payload.
pub(crate) const BLOCK_HEADER_LEN: usize = 16;

const FLAG_LAST: u8 = 1 << 0;

/// What a block header says, once its CRC has been checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockInfo {
    /// Position of this data frame in the video, counting from 0.
    pub index: u32,
    /// Bytes of payload that follow the header.
    pub payload_len: usize,
    /// This is the final data frame.
    pub last: bool,
}

/// Finish a block in place. The payload must already be in
/// `block[16..16 + payload_len]`; this writes the header and zeroes the padding.
pub(crate) fn seal(block: &mut [u8], index: u32, payload_len: usize, last: bool) {
    debug_assert!(BLOCK_HEADER_LEN + payload_len <= block.len());
    let end = BLOCK_HEADER_LEN + payload_len;
    block[end..].fill(0);
    block[0..4].copy_from_slice(&index.to_le_bytes());
    block[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
    block[8] = if last { FLAG_LAST } else { 0 };
    block[9..12].fill(0);
    let crc = checksum(&block[0..12], &block[BLOCK_HEADER_LEN..end]);
    block[12..16].copy_from_slice(&crc.to_le_bytes());
}

/// Check a block read back from a frame. `None` when the length is impossible
/// or the CRC does not match.
pub(crate) fn open(block: &[u8]) -> Option<BlockInfo> {
    if block.len() < BLOCK_HEADER_LEN {
        return None;
    }
    let index = le_u32(&block[0..4]);
    let payload_len = le_u32(&block[4..8]) as usize;
    let end = BLOCK_HEADER_LEN.checked_add(payload_len)?;
    if end > block.len() {
        return None;
    }
    let stored = le_u32(&block[12..16]);
    if checksum(&block[0..12], &block[BLOCK_HEADER_LEN..end]) != stored {
        return None;
    }
    Some(BlockInfo {
        index,
        payload_len,
        last: block[8] & FLAG_LAST != 0,
    })
}

/// CRC-32 of the first 12 header bytes followed by the payload.
fn checksum(head: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(head);
    hasher.update(payload);
    hasher.finalize()
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut four = [0u8; 4];
    four.copy_from_slice(bytes);
    u32::from_le_bytes(four)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Layout, block_len};
    use crate::testutil::noise;
    use crate::types::{Cell, Mode, Resolution};

    fn sealed(len: usize, index: u32, payload: &[u8], last: bool) -> Vec<u8> {
        // Start from junk to prove the padding really gets cleared.
        let mut block = vec![0xAA; len];
        block[BLOCK_HEADER_LEN..BLOCK_HEADER_LEN + payload.len()].copy_from_slice(payload);
        seal(&mut block, index, payload.len(), last);
        block
    }

    #[test]
    fn round_trips_for_every_block_size() {
        let modes = [
            Mode::Lossless,
            Mode::Robust {
                cell: Cell::C4,
                parity: 32,
            },
            Mode::Robust {
                cell: Cell::C8,
                parity: 64,
            },
        ];
        for resolution in [Resolution::P1080, Resolution::P2160] {
            for mode in modes {
                let len = block_len(Layout::for_mode(resolution, &mode), &mode);
                let capacity = len - BLOCK_HEADER_LEN;
                for payload_len in [0, 1, capacity / 2, capacity] {
                    let payload = noise(payload_len, payload_len as u64 + 1);
                    let block = sealed(len, 42, &payload, payload_len == 0);
                    let info = open(&block).unwrap();
                    assert_eq!(info.index, 42);
                    assert_eq!(info.payload_len, payload_len);
                    assert_eq!(info.last, payload_len == 0);
                    let end = BLOCK_HEADER_LEN + payload_len;
                    assert_eq!(&block[BLOCK_HEADER_LEN..end], &payload[..]);
                    assert!(
                        block[end..].iter().all(|byte| *byte == 0),
                        "padding is zero"
                    );
                }
            }
        }
    }

    #[test]
    fn header_fields_are_where_the_spec_puts_them() {
        let block = sealed(64, 0x0102_0304, b"hello", true);
        assert_eq!(&block[0..4], &[4, 3, 2, 1], "index, little-endian");
        assert_eq!(&block[4..8], &[5, 0, 0, 0], "payload length");
        assert_eq!(block[8], 1, "last flag");
        assert_eq!(&block[9..12], &[0; 3], "reserved");
        let mut covered = block[0..12].to_vec();
        covered.extend_from_slice(b"hello");
        assert_eq!(&block[12..16], &crc32fast::hash(&covered).to_le_bytes());
    }

    #[test]
    fn any_flipped_bit_in_header_or_payload_is_caught() {
        let payload = noise(100, 9);
        let block = sealed(256, 7, &payload, false);
        for position in 0..BLOCK_HEADER_LEN + payload.len() {
            let mut damaged = block.clone();
            damaged[position] ^= 0x04;
            assert!(open(&damaged).is_none(), "flip at byte {position}");
        }
    }

    #[test]
    fn an_impossible_length_is_rejected_without_reading_past_the_end() {
        let mut block = sealed(64, 0, b"abc", false);
        block[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(open(&block).is_none());
        assert!(open(&[0u8; 8]).is_none());
    }
}
