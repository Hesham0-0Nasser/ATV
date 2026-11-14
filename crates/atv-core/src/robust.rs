//! Robust mode's error correction (spec 5.4, 5.4b): Reed-Solomon codewords
//! interleaved across the frame (version 1) or a group of frames (version 2),
//! and LDPC codewords woven across a group (version 3).
//!
//! A frame block is cut into `n` pieces of `k` bytes. Each piece gets `p`
//! parity bytes, making a 255-byte codeword that can lose up to `p / 2` bytes
//! anywhere and still be rebuilt. The codewords are then woven together, byte
//! `i` of codeword `j` at frame position `i * n + j`, so that a damaged patch
//! of the picture costs every codeword a few bytes instead of costing one
//! codeword everything.
//!
//! The code is the common one over GF(2^8): field polynomial 0x11D, generator
//! 2, roots 2^0 to 2^(p-1), parity appended after the data.

use reed_solomon::{Decoder, Encoder};

use crate::format::{CODEWORD, Layout};
use crate::ldpc::{CODE_BITS, Ldpc, LdpcWork};

pub(crate) struct Codec {
    /// Codewords per frame (`n`).
    codewords: usize,
    /// Data bytes per codeword (`k`).
    data_len: usize,
    encoder: Encoder,
    decoder: Decoder,
}

impl Codec {
    pub(crate) fn new(layout: Layout, parity: u8) -> Self {
        let parity = usize::from(parity);
        Codec {
            codewords: layout.capacity() / CODEWORD,
            data_len: CODEWORD.saturating_sub(parity),
            encoder: Encoder::new(parity),
            decoder: Decoder::new(parity),
        }
    }

    /// Add parity to `block` (`n * k` bytes) and weave the codewords into
    /// `frame` (one frame's worth of bytes). Bytes past the last codeword are zero.
    pub(crate) fn protect(&self, block: &[u8], frame: &mut [u8]) {
        debug_assert_eq!(block.len(), self.codewords * self.data_len);
        let n = self.codewords;
        frame[n * CODEWORD..].fill(0);
        for (j, data) in block.chunks_exact(self.data_len).enumerate() {
            let codeword = self.encoder.encode(data);
            for (i, byte) in codeword.iter().enumerate() {
                frame[i * n + j] = *byte;
            }
        }
    }

    /// Undo [`Codec::protect`], repairing what can be repaired. Returns the
    /// number of bytes that had to be corrected, or `None` when any codeword
    /// is damaged beyond repair (the frame is then unusable).
    pub(crate) fn recover(&self, frame: &[u8], block: &mut [u8]) -> Option<u64> {
        debug_assert_eq!(block.len(), self.codewords * self.data_len);
        let n = self.codewords;
        let mut repaired = 0u64;
        let mut codeword = [0u8; CODEWORD];
        for (j, data) in block.chunks_exact_mut(self.data_len).enumerate() {
            for (i, byte) in codeword.iter_mut().enumerate() {
                *byte = frame[i * n + j];
            }
            // Fast path. A word is a valid codeword exactly when its parity is
            // what the encoder would produce for its data, and encoding is
            // several times cheaper than running the decoder. Most codewords
            // of most videos are undamaged, so this is the common case.
            let (received_data, received_parity) = codeword.split_at(self.data_len);
            if self.encoder.encode(received_data).ecc() == received_parity {
                data.copy_from_slice(received_data);
                continue;
            }
            let (fixed, count) = self.decoder.correct_err_count(&codeword, None).ok()?;
            data.copy_from_slice(fixed.data());
            repaired += count as u64;
        }
        Some(repaired)
    }
}

/// Frames whose codewords are woven together in format version 2.
pub(crate) const GROUP_FRAMES: usize = 15;
/// The frame tag: index (4 bytes), flags (1), reserved (7), CRC-32 (4).
pub(crate) const TAG_LEN: usize = 16;
/// The most parity a tag gets; the spare bytes of a layout may allow less.
const MAX_TAG_PARITY: usize = 128;
const TAG_LAST_GROUP: u8 = 1 << 0;

/// What a version-2 frame says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameTag {
    /// Position of the frame among the data frames, counting from 0.
    pub index: u32,
    /// The frame belongs to the final group.
    pub last_group: bool,
}

/// Format version 2's error correction (spec 5.4).
///
/// The `n` codewords of each frame are placed diagonally: byte `i` of the
/// frame's codeword slot `s` goes to data position `i * n + (s + i) mod n`, so
/// a column of the picture no longer feeds the same codeword in every row.
/// The codewords of a group of [`GROUP_FRAMES`] frames are then woven across
/// the group: byte `i` of group codeword `g` (slot `g mod n`, set `g / n`)
/// lives in frame `(g / n - i) mod 15`. A frame that is lost or unreadable
/// costs each codeword of its group at most 17 bytes, and since its frame
/// number says which bytes are gone, those count as erasures, which cost
/// half as much repair as errors.
///
/// Each frame also carries a tag (its index and a last-group flag) in the
/// spare bytes every layout has after its last codeword, protected by its own
/// shortened Reed-Solomon code and spread evenly over the frame. So frames
/// can be recognised, ordered and checked one by one, before their group is
/// repaired. The tag uses no data capacity.
pub(crate) struct GroupCodec {
    /// Codewords per frame (`n`).
    slots: usize,
    data_len: usize,
    parity: usize,
    /// Frame position of data byte `i` of slot `s`, at `i * n + s`.
    data_positions: Vec<u32>,
    /// Frame positions of the tag codeword's bytes.
    tag_positions: Vec<u32>,
    encoder: Encoder,
    decoder: Decoder,
    tag_encoder: Encoder,
    tag_decoder: Decoder,
}

impl GroupCodec {
    pub(crate) fn new(layout: Layout, parity: u8) -> Self {
        let capacity = layout.capacity();
        let slots = capacity / CODEWORD;
        let spare = capacity - slots * CODEWORD;
        // Every Robust layout has at least 30 spare bytes (spec 5.4).
        let mut tag_len = spare.min(TAG_LEN + MAX_TAG_PARITY);
        if (tag_len - TAG_LEN.min(tag_len)) % 2 == 1 {
            tag_len -= 1;
        }
        let tag_parity = tag_len.saturating_sub(TAG_LEN);
        // Tag bytes spread evenly over the frame; data bytes take the rest.
        let tag_positions: Vec<u32> = (0..tag_len)
            .map(|j| (j * capacity / tag_len.max(1)) as u32)
            .collect();
        let mut is_tag = vec![false; capacity];
        for &position in &tag_positions {
            is_tag[position as usize] = true;
        }
        let free: Vec<u32> = (0..capacity)
            .filter(|&position| !is_tag[position])
            .map(|position| position as u32)
            .collect();
        let mut data_positions = vec![0u32; slots * CODEWORD];
        for i in 0..CODEWORD {
            for s in 0..slots {
                data_positions[i * slots + s] = free[i * slots + (s + i) % slots];
            }
        }
        let parity = usize::from(parity);
        GroupCodec {
            slots,
            data_len: CODEWORD.saturating_sub(parity),
            parity,
            data_positions,
            tag_positions,
            encoder: Encoder::new(parity),
            decoder: Decoder::new(parity),
            tag_encoder: Encoder::new(tag_parity),
            tag_decoder: Decoder::new(tag_parity),
        }
    }

    /// Where the tag's bytes sit in a frame (for tests that damage tags).
    #[cfg(test)]
    pub(crate) fn tag_positions(&self) -> &[u32] {
        &self.tag_positions
    }

    /// Bytes of one group block: `15 * n * k`.
    pub(crate) fn block_len(&self) -> usize {
        GROUP_FRAMES * self.slots * self.data_len
    }

    /// The frame that holds byte `i` of group codeword `g`, and where.
    fn locate(&self, g: usize, i: usize) -> (usize, usize) {
        let set = g / self.slots;
        let slot = g % self.slots;
        let frame = (set + GROUP_FRAMES - i % GROUP_FRAMES) % GROUP_FRAMES;
        (frame, self.data_positions[i * self.slots + slot] as usize)
    }

    /// Add parity to `block` and weave it into the group's frames (each one
    /// frame's worth of bytes), with tags numbered from `first_index`.
    pub(crate) fn protect(
        &self,
        block: &[u8],
        first_index: u32,
        last_group: bool,
        frames: &mut [Vec<u8>],
    ) {
        debug_assert_eq!(block.len(), self.block_len());
        debug_assert_eq!(frames.len(), GROUP_FRAMES);
        for frame in frames.iter_mut() {
            frame.fill(0);
        }
        for (g, data) in block.chunks_exact(self.data_len).enumerate() {
            let codeword = self.encoder.encode(data);
            for (i, byte) in codeword.iter().enumerate() {
                let (frame, position) = self.locate(g, i);
                frames[frame][position] = *byte;
            }
        }
        for (f, frame) in frames.iter_mut().enumerate() {
            let tag = tag_bytes(FrameTag {
                index: first_index.wrapping_add(f as u32),
                last_group,
            });
            let codeword = self.tag_encoder.encode(&tag);
            for (byte, &position) in codeword.iter().zip(&self.tag_positions) {
                frame[position as usize] = *byte;
            }
        }
    }

    /// Read a frame's tag, repairing it if needed. `None` when it cannot be
    /// read: a header frame, or a frame too damaged to place.
    pub(crate) fn read_tag(&self, frame: &[u8]) -> Option<FrameTag> {
        let mut word: Vec<u8> = self
            .tag_positions
            .iter()
            .map(|&position| frame.get(position as usize).copied().unwrap_or(0))
            .collect();
        let (data, parity) = word.split_at(TAG_LEN);
        if self.tag_encoder.encode(data).ecc() != parity {
            let (fixed, _) = self.tag_decoder.correct_err_count(&word, None).ok()?;
            word.truncate(0);
            word.extend_from_slice(fixed.data());
        }
        read_tag_bytes(word.get(..TAG_LEN)?)
    }

    /// Undo [`GroupCodec::protect`]. `frames` has one entry per frame of the
    /// group, `None` for a frame that is missing (its bytes become
    /// erasures). Returns the bytes repaired, or `None` when any codeword is
    /// past repair.
    pub(crate) fn recover(&self, frames: &[Option<&[u8]>], block: &mut [u8]) -> Option<u64> {
        debug_assert_eq!(block.len(), self.block_len());
        let mut repaired = 0u64;
        let mut codeword = [0u8; CODEWORD];
        let mut erasures: Vec<u8> = Vec::with_capacity(CODEWORD);
        for (g, data) in block.chunks_exact_mut(self.data_len).enumerate() {
            erasures.clear();
            for (i, byte) in codeword.iter_mut().enumerate() {
                let (frame, position) = self.locate(g, i);
                match frames.get(frame).copied().flatten() {
                    Some(bytes) => *byte = bytes.get(position).copied().unwrap_or(0),
                    None => {
                        *byte = 0;
                        erasures.push(i as u8);
                    }
                }
            }
            if erasures.len() > self.parity {
                return None;
            }
            let (received_data, received_parity) = codeword.split_at(self.data_len);
            if erasures.is_empty() && self.encoder.encode(received_data).ecc() == received_parity {
                data.copy_from_slice(received_data);
                continue;
            }
            let known = (!erasures.is_empty()).then_some(erasures.as_slice());
            let (fixed, count) = self.decoder.correct_err_count(&codeword, known).ok()?;
            data.copy_from_slice(fixed.data());
            repaired += count as u64;
        }
        Some(repaired)
    }
}

/// Format version 3's tag codeword: the 16 tag bytes and 32 parity bytes.
const V3_TAG_CODEWORD: usize = 48;
/// Bits of each codeword that travel in each frame of a group.
const V3_BITS_PER_FRAME: usize = CODE_BITS / GROUP_FRAMES;

/// Format version 3's error correction (spec 5.4b): LDPC codewords of 8,100
/// bits woven across groups of [`GROUP_FRAMES`] frames.
///
/// Bit `b` of a group codeword travels in frame `b mod 15`, so a lost frame
/// takes the same 540 bits from every codeword of its group, spread over the
/// whole code. Within frame `f`, bit `i = b / 15` of codeword `c` goes to
/// slot `((c + f) mod C) * 540 + i` (C codewords per group), and slots are
/// mapped to data cells by a fixed shuffle (see [`shuffled`]), so any patch
/// of damage, of any shape, is shared out over all codewords, and a spot
/// that is bad in every frame hits a different codeword in each. The tag
/// works as in version 2 but has a fixed 48-byte codeword (32 parity bytes),
/// spread evenly over the frame's bytes; data cells are all the cells of the
/// other bytes.
///
/// Every codeword is whitened (XORed with a fixed pseudo-random pattern,
/// see [`whitening`]) before it is drawn, so frames are an even mix of black
/// and white whatever the data: padding and plain unencrypted files would
/// otherwise make mostly-black frames whose few white cells compression
/// smears away (found with the YouTube 720p imitation, spec 5.4b).
///
/// The decoder is given how sure each cell is (its grey level), and a lost
/// frame's cells count as "no idea".
pub(crate) struct LdpcGroupCodec {
    code: &'static Ldpc,
    /// 1 where a codeword bit is flipped before drawing.
    whitening: Vec<u8>,
    /// Codewords per group (`C`).
    codewords: usize,
    /// Cell number of slot `s` (cells counted row by row).
    data_cells: Vec<u32>,
    tag_positions: Vec<u32>,
    tag_encoder: Encoder,
    tag_decoder: Decoder,
}

impl LdpcGroupCodec {
    pub(crate) fn new(layout: Layout, parity: u8) -> Self {
        let capacity = layout.capacity();
        let tag_positions: Vec<u32> = (0..V3_TAG_CODEWORD)
            .map(|j| (j * capacity / V3_TAG_CODEWORD) as u32)
            .collect();
        let mut is_tag = vec![false; capacity];
        for &position in &tag_positions {
            is_tag[position as usize] = true;
        }
        let free: Vec<u32> = (0..capacity * 8)
            .filter(|&cell| !is_tag[cell / 8])
            .map(|cell| cell as u32)
            .collect();
        let codewords = free.len() / V3_BITS_PER_FRAME;
        let tag_parity = V3_TAG_CODEWORD - TAG_LEN;
        LdpcGroupCodec {
            code: Ldpc::for_parity(parity),
            whitening: whitening(),
            codewords,
            data_cells: shuffled(free),
            tag_positions,
            tag_encoder: Encoder::new(tag_parity),
            tag_decoder: Decoder::new(tag_parity),
        }
    }

    /// Bytes of one group block: `C` codewords' data.
    pub(crate) fn block_len(&self) -> usize {
        self.codewords * self.code.data_bytes()
    }

    /// Where the tag's bytes sit in a frame (for tests that damage tags).
    #[cfg(test)]
    pub(crate) fn tag_positions(&self) -> &[u32] {
        &self.tag_positions
    }

    /// The cell that holds bit `b` of codeword `c`, and its frame.
    fn locate(&self, c: usize, b: usize) -> (usize, usize) {
        let frame = b % GROUP_FRAMES;
        let i = b / GROUP_FRAMES;
        let slot = (c + frame) % self.codewords * V3_BITS_PER_FRAME + i;
        (frame, self.data_cells[slot] as usize)
    }

    /// Encode `block` into the group's frames (each one frame's worth of
    /// bytes), with tags numbered from `first_index`.
    pub(crate) fn protect(
        &self,
        block: &[u8],
        first_index: u32,
        last_group: bool,
        frames: &mut [Vec<u8>],
    ) {
        debug_assert_eq!(block.len(), self.block_len());
        debug_assert_eq!(frames.len(), GROUP_FRAMES);
        for frame in frames.iter_mut() {
            frame.fill(0);
        }
        let mut bits = vec![0u8; CODE_BITS];
        for (c, data) in block.chunks_exact(self.code.data_bytes()).enumerate() {
            self.code.encode(data, &mut bits);
            for (b, (&bit, &flip)) in bits.iter().zip(&self.whitening).enumerate() {
                if bit ^ flip == 1 {
                    let (frame, cell) = self.locate(c, b);
                    frames[frame][cell / 8] |= 0x80 >> (cell % 8);
                }
            }
        }
        for (f, frame) in frames.iter_mut().enumerate() {
            let tag = tag_bytes(FrameTag {
                index: first_index.wrapping_add(f as u32),
                last_group,
            });
            let codeword = self.tag_encoder.encode(&tag);
            for (byte, &position) in codeword.iter().zip(&self.tag_positions) {
                frame[position as usize] = *byte;
            }
        }
    }

    /// Read a frame's tag from its bytes, repairing it if needed.
    pub(crate) fn read_tag(&self, frame: &[u8]) -> Option<FrameTag> {
        let mut word: Vec<u8> = self
            .tag_positions
            .iter()
            .map(|&position| frame.get(position as usize).copied().unwrap_or(0))
            .collect();
        let (data, parity) = word.split_at(TAG_LEN);
        if self.tag_encoder.encode(data).ecc() != parity {
            let (fixed, _) = self.tag_decoder.correct_err_count(&word, None).ok()?;
            word.truncate(0);
            word.extend_from_slice(fixed.data());
        }
        read_tag_bytes(word.get(..TAG_LEN)?)
    }

    /// Undo [`LdpcGroupCodec::protect`]. `frames` has, for each frame of the
    /// group, how sure each of its cells is ([`crate::format::grid::read_soft`],
    /// each value stored as its two's-complement byte), or `None` for a
    /// missing frame. Returns the data bytes repaired, or `None` when any
    /// codeword is past repair.
    pub(crate) fn recover(&self, frames: &[Option<&[u8]>], block: &mut [u8]) -> Option<u64> {
        debug_assert_eq!(block.len(), self.block_len());
        let mut confidence = vec![0f32; CODE_BITS];
        let mut work = LdpcWork::default();
        let mut repaired = 0u64;
        for (c, data) in block.chunks_exact_mut(self.code.data_bytes()).enumerate() {
            for (b, (value, &flip)) in confidence.iter_mut().zip(&self.whitening).enumerate() {
                let (frame, cell) = self.locate(c, b);
                let read = match frames.get(frame).copied().flatten() {
                    Some(soft) => f32::from(soft.get(cell).copied().unwrap_or(0) as i8),
                    None => 0.0,
                };
                *value = if flip == 1 { -read } else { read };
            }
            repaired += match self.code.decode(&confidence, data, &mut work) {
                Some(count) => count,
                None => self.retry_without_a_frame(&mut confidence, data, &mut work)?,
            };
        }
        Some(repaired)
    }

    /// A codeword that failed may have one frame ruined rather than lost (a
    /// garbled frame whose damage looks sure of itself). Treat the frames as
    /// lost one at a time, the one that most disagrees with the failed
    /// attempt first, and keep the first repair that works. The block's
    /// checksum still checks the result.
    fn retry_without_a_frame(
        &self,
        confidence: &mut [f32],
        data: &mut [u8],
        work: &mut LdpcWork,
    ) -> Option<u64> {
        let mut disagree = [0usize; GROUP_FRAMES];
        for (b, &value) in confidence.iter().enumerate() {
            if value != 0.0 && (value > 0.0) != work.leans_to_one(b) {
                disagree[b % GROUP_FRAMES] += 1;
            }
        }
        let mut order: Vec<usize> = (0..GROUP_FRAMES).collect();
        order.sort_by_key(|&frame| std::cmp::Reverse(disagree[frame]));
        let original = confidence.to_vec();
        for frame in order {
            if disagree[frame] == 0 {
                break;
            }
            confidence.copy_from_slice(&original);
            for value in confidence.iter_mut().skip(frame).step_by(GROUP_FRAMES) {
                *value = 0.0;
            }
            if let Some(count) = self.code.decode(confidence, data, work) {
                return Some(count);
            }
        }
        None
    }
}

/// Format version 3's fixed shuffle of a frame's data cells (spec 5.4b):
/// Fisher-Yates from the last entry down, entry `k` swapped with entry
/// `next() mod (k + 1)`, where `next()` is xorshift64* seeded with the bytes
/// "ATV3". Part of the format: it must never change.
fn shuffled(mut cells: Vec<u32>) -> Vec<u32> {
    // "ATV3" in ASCII.
    let mut state: u64 = 0x4154_5633;
    for k in (1..cells.len()).rev() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let next = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        cells.swap(k, (next % (k as u64 + 1)) as usize);
    }
    cells
}

/// Format version 3's whitening pattern (spec 5.4b): for codeword bit `b`,
/// the top bit of the `b`-th output of xorshift64* seeded with the bytes
/// "ATVW". The same for every codeword. Part of the format.
fn whitening() -> Vec<u8> {
    // "ATVW" in ASCII.
    let mut state: u64 = 0x4154_5657;
    (0..CODE_BITS)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 63) as u8
        })
        .collect()
}

/// The group repair of format versions 2 and 3, behind one face for the
/// frame writer and reader.
pub(crate) enum GroupRepair {
    ReedSolomon(Box<GroupCodec>),
    Ldpc(Box<LdpcGroupCodec>),
}

impl GroupRepair {
    /// The repair for a Robust video of this format version (2 or 3).
    pub(crate) fn new(layout: Layout, parity: u8, version: u8) -> Self {
        if version >= crate::format::header::VERSION_3 {
            GroupRepair::Ldpc(Box::new(LdpcGroupCodec::new(layout, parity)))
        } else {
            GroupRepair::ReedSolomon(Box::new(GroupCodec::new(layout, parity)))
        }
    }

    /// Whether frames must be read with how sure each cell is, rather than
    /// as plain bytes.
    pub(crate) fn soft(&self) -> bool {
        matches!(self, GroupRepair::Ldpc(_))
    }

    pub(crate) fn block_len(&self) -> usize {
        match self {
            GroupRepair::ReedSolomon(codec) => codec.block_len(),
            GroupRepair::Ldpc(codec) => codec.block_len(),
        }
    }

    pub(crate) fn protect(
        &self,
        block: &[u8],
        first_index: u32,
        last_group: bool,
        frames: &mut [Vec<u8>],
    ) {
        match self {
            GroupRepair::ReedSolomon(codec) => {
                codec.protect(block, first_index, last_group, frames)
            }
            GroupRepair::Ldpc(codec) => codec.protect(block, first_index, last_group, frames),
        }
    }

    /// Read a frame's tag from its bytes.
    pub(crate) fn read_tag(&self, frame: &[u8]) -> Option<FrameTag> {
        match self {
            GroupRepair::ReedSolomon(codec) => codec.read_tag(frame),
            GroupRepair::Ldpc(codec) => codec.read_tag(frame),
        }
    }

    /// Repair a group. Each frame is its bytes (version 2) or its cells'
    /// confidence values (version 3, see [`GroupRepair::soft`]).
    pub(crate) fn recover(&self, frames: &[Option<&[u8]>], block: &mut [u8]) -> Option<u64> {
        match self {
            GroupRepair::ReedSolomon(codec) => codec.recover(frames, block),
            GroupRepair::Ldpc(codec) => codec.recover(frames, block),
        }
    }
}

fn tag_bytes(tag: FrameTag) -> [u8; TAG_LEN] {
    let mut bytes = [0u8; TAG_LEN];
    bytes[0..4].copy_from_slice(&tag.index.to_le_bytes());
    bytes[4] = if tag.last_group { TAG_LAST_GROUP } else { 0 };
    let crc = crc32fast::hash(&bytes[0..12]);
    bytes[12..16].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn read_tag_bytes(bytes: &[u8]) -> Option<FrameTag> {
    let four = |range: std::ops::Range<usize>| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(range)?.try_into().ok()?))
    };
    let head = bytes.get(0..12)?;
    if crc32fast::hash(head) != four(12..16)? || head.get(5..12)?.iter().any(|b| *b != 0) {
        return None;
    }
    Some(FrameTag {
        index: four(0..4)?,
        last_group: bytes.get(4)? & TAG_LAST_GROUP != 0,
    })
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use crate::testutil::noise;
    use crate::types::{Cell, Mode, Resolution};

    fn all_layouts() -> Vec<(Layout, u8)> {
        let mut all = Vec::new();
        for resolution in [Resolution::P1080, Resolution::P2160] {
            for cell in [Cell::C4, Cell::C8] {
                for parity in [32u8, 64] {
                    let mode = Mode::Robust { cell, parity };
                    all.push((Layout::for_mode(resolution, &mode), parity));
                }
            }
        }
        all
    }

    fn encode(codec: &GroupCodec, layout: Layout, seed: u64) -> (Vec<u8>, Vec<Vec<u8>>) {
        let block = noise(codec.block_len(), seed);
        let mut frames = vec![vec![0u8; layout.capacity()]; GROUP_FRAMES];
        codec.protect(&block, 30, true, &mut frames);
        (block, frames)
    }

    #[test]
    fn every_frame_position_is_used_once() {
        for (layout, parity) in all_layouts() {
            let codec = GroupCodec::new(layout, parity);
            let mut seen = vec![0u8; layout.capacity()];
            for &p in codec.data_positions.iter().chain(&codec.tag_positions) {
                seen[p as usize] += 1;
            }
            assert!(seen.iter().all(|count| *count <= 1), "{layout:?}");
            assert!(codec.tag_positions.len() >= 30, "{layout:?}");
        }
    }

    #[test]
    fn clean_groups_round_trip_and_tags_read_back() {
        for (layout, parity) in all_layouts() {
            let codec = GroupCodec::new(layout, parity);
            let (block, frames) = encode(&codec, layout, u64::from(parity));
            for (f, frame) in frames.iter().enumerate() {
                assert_eq!(
                    codec.read_tag(frame),
                    Some(FrameTag {
                        index: 30 + f as u32,
                        last_group: true
                    })
                );
            }
            let views: Vec<Option<&[u8]>> = frames.iter().map(|f| Some(f.as_slice())).collect();
            let mut back = vec![0u8; codec.block_len()];
            assert_eq!(codec.recover(&views, &mut back), Some(0));
            assert_eq!(back, block, "{layout:?}");
        }
    }

    #[test]
    fn a_lost_frame_is_rebuilt_with_strong_repair() {
        for (layout, parity) in all_layouts().into_iter().filter(|(_, p)| *p == 64) {
            let codec = GroupCodec::new(layout, parity);
            let (block, frames) = encode(&codec, layout, 7);
            for lost in [0, 7, 14] {
                let views: Vec<Option<&[u8]>> = frames
                    .iter()
                    .enumerate()
                    .map(|(f, frame)| (f != lost).then_some(frame.as_slice()))
                    .collect();
                let mut back = vec![0u8; codec.block_len()];
                assert!(
                    codec.recover(&views, &mut back).is_some(),
                    "{layout:?} lost {lost}"
                );
                assert_eq!(back, block);
            }
        }
    }

    #[test]
    fn a_ruined_quarter_of_one_frame_is_repaired() {
        for (layout, parity) in all_layouts() {
            let codec = GroupCodec::new(layout, parity);
            let (block, mut frames) = encode(&codec, layout, 9);
            // The top-left quarter of frame 5 painted black: every byte of
            // the top half of its rows, left half of each row, set to zero.
            let row_bytes = layout.width / layout.cell / 8;
            let rows = layout.height / layout.cell;
            for row in 0..rows / 2 {
                for column in 0..row_bytes / 2 {
                    frames[5][row * row_bytes + column] = 0;
                }
            }
            let views: Vec<Option<&[u8]>> = frames.iter().map(|f| Some(f.as_slice())).collect();
            let mut back = vec![0u8; codec.block_len()];
            assert!(codec.recover(&views, &mut back).is_some(), "{layout:?}");
            assert_eq!(back, block);
        }
    }

    #[test]
    fn a_damaged_tag_is_repaired_and_a_wrong_one_refused() {
        for (layout, parity) in all_layouts() {
            let codec = GroupCodec::new(layout, parity);
            let (_, mut frames) = encode(&codec, layout, 11);
            let tag_len = codec.tag_positions.len();
            let fixable = (tag_len - TAG_LEN) / 2;
            for &p in codec.tag_positions.iter().take(fixable) {
                frames[3][p as usize] ^= 0xA5;
            }
            assert_eq!(
                codec.read_tag(&frames[3]).map(|t| t.index),
                Some(33),
                "{layout:?}"
            );
            // A header frame or noise is not a data frame.
            assert_eq!(codec.read_tag(&noise(layout.capacity(), 5)), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::block_len;
    use crate::testutil::noise;
    use crate::types::{Cell, Mode, Resolution};

    fn layouts() -> Vec<(Layout, Mode)> {
        let mut all = Vec::new();
        for resolution in [Resolution::P1080, Resolution::P2160] {
            for cell in [Cell::C4, Cell::C8] {
                for parity in [32u8, 64] {
                    let mode = Mode::Robust { cell, parity };
                    all.push((Layout::for_mode(resolution, &mode), mode));
                }
            }
        }
        all
    }

    fn parity_of(mode: &Mode) -> u8 {
        match mode {
            Mode::Robust { parity, .. } => *parity,
            Mode::Lossless => 0,
        }
    }

    /// A tiny random number generator for picking damage positions.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 33) as usize % sides
        }
    }

    /// Change `count` different bytes of codeword `j` inside a woven frame.
    fn damage_codeword(frame: &mut [u8], n: usize, j: usize, count: usize, dice: &mut Dice) {
        let mut hit = [false; CODEWORD];
        let mut done = 0;
        while done < count {
            let i = dice.roll(CODEWORD);
            if !hit[i] {
                hit[i] = true;
                frame[i * n + j] ^= 1 + dice.roll(255) as u8;
                done += 1;
            }
        }
    }

    #[test]
    fn clean_frames_round_trip_for_every_layout() {
        for (layout, mode) in layouts() {
            let codec = Codec::new(layout, parity_of(&mode));
            let block = noise(block_len(layout, &mode), layout.width as u64);
            let mut frame = vec![0xAAu8; layout.capacity()];
            codec.protect(&block, &mut frame);
            let mut back = vec![0u8; block.len()];
            assert_eq!(codec.recover(&frame, &mut back), Some(0), "{mode:?}");
            assert_eq!(back, block);
            let used = codec.codewords * CODEWORD;
            assert!(
                frame[used..].iter().all(|byte| *byte == 0),
                "the tail is zero"
            );
        }
    }

    #[test]
    fn bytes_are_woven_as_the_spec_says() {
        let mode = Mode::Robust {
            cell: Cell::C8,
            parity: 32,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        let codec = Codec::new(layout, 32);
        assert_eq!((codec.codewords, codec.data_len), (15, 223));
        let block = noise(block_len(layout, &mode), 9);
        let mut frame = vec![0u8; layout.capacity()];
        codec.protect(&block, &mut frame);
        // Byte i of codeword j is at position i * n + j, and the data comes first.
        for j in 0..15 {
            for i in 0..223 {
                assert_eq!(frame[i * 15 + j], block[j * 223 + i]);
            }
        }
    }

    #[test]
    fn known_parity_values() {
        // The reference vector from the reed-solomon crate's own tests: this
        // pins the field, the generator and the byte order of the format.
        let data: Vec<u8> = (0..30).collect();
        let codeword = Encoder::new(8).encode(&data);
        assert_eq!(codeword.ecc(), [99, 26, 219, 193, 9, 94, 186, 143]);
    }

    #[test]
    fn up_to_half_the_parity_in_errors_is_repaired_and_counted() {
        let mut dice = Dice(0x1234_5678_9ABC_DEF1);
        for (layout, mode) in layouts() {
            let parity = parity_of(&mode);
            let limit = usize::from(parity) / 2;
            let codec = Codec::new(layout, parity);
            let block = noise(block_len(layout, &mode), 3);
            let mut frame = vec![0u8; layout.capacity()];
            codec.protect(&block, &mut frame);
            // Every codeword gets a different number of errors, up to the limit.
            let mut expected = 0u64;
            for j in 0..codec.codewords {
                let errors = if j == 0 { limit } else { dice.roll(limit + 1) };
                damage_codeword(&mut frame, codec.codewords, j, errors, &mut dice);
                expected += errors as u64;
            }
            let mut back = vec![0u8; block.len()];
            assert_eq!(codec.recover(&frame, &mut back), Some(expected), "{mode:?}");
            assert_eq!(back, block, "{mode:?}");
        }
    }

    #[test]
    fn one_error_too_many_is_a_clean_failure() {
        let mut dice = Dice(0x0BAD_C0DE_1234_5678);
        for (layout, mode) in layouts() {
            let parity = parity_of(&mode);
            let codec = Codec::new(layout, parity);
            let block = noise(block_len(layout, &mode), 4);
            let mut clean = vec![0u8; layout.capacity()];
            codec.protect(&block, &mut clean);
            for round in 0..20 {
                let mut frame = clean.clone();
                let j = dice.roll(codec.codewords);
                let errors = usize::from(parity) / 2 + 1 + round % 5;
                damage_codeword(&mut frame, codec.codewords, j, errors, &mut dice);
                let mut back = vec![0u8; block.len()];
                // Beyond the limit a decoder can, very rarely, land on a
                // different valid codeword. It must never hand back the
                // original as if nothing happened, and it must not crash.
                match codec.recover(&frame, &mut back) {
                    None => {}
                    Some(_) => assert_ne!(back, block, "{mode:?}: silently wrong"),
                }
            }
        }
    }

    #[test]
    fn garbage_never_crashes_the_decoder() {
        let mode = Mode::Robust {
            cell: Cell::C8,
            parity: 32,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        for parity in [32u8, 64] {
            let codec = Codec::new(layout, parity);
            let mut back = vec![0u8; codec.codewords * codec.data_len];
            for seed in 0..300u64 {
                let frame = noise(layout.capacity(), seed + 1);
                // Random bytes are essentially never within reach of a codeword.
                let _ = codec.recover(&frame, &mut back);
            }
            for fill in [0x00u8, 0xFF, 0x55] {
                let frame = vec![fill; layout.capacity()];
                let _ = codec.recover(&frame, &mut back);
            }
        }
    }

    #[test]
    fn a_damaged_patch_is_spread_across_all_codewords() {
        // Wipe a run of consecutive frame bytes (a patch of the picture).
        // Without weaving it would destroy whole codewords; with it, every
        // codeword loses only a few bytes.
        let mode = Mode::Robust {
            cell: Cell::C4,
            parity: 32,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        let codec = Codec::new(layout, 32);
        assert_eq!(codec.codewords, 63);
        let block = noise(block_len(layout, &mode), 6);
        let mut frame = vec![0u8; layout.capacity()];
        codec.protect(&block, &mut frame);
        // 16 bytes from each of 63 codewords: 1,008 bytes in a row, about 6% of the frame.
        let patch = 16 * 63;
        for byte in &mut frame[5000..5000 + patch] {
            *byte = !*byte;
        }
        let mut back = vec![0u8; block.len()];
        assert_eq!(codec.recover(&frame, &mut back), Some(patch as u64));
        assert_eq!(back, block);

        // One byte more per codeword is past the limit.
        for byte in &mut frame[5000 + patch..5000 + patch + 63] {
            *byte = !*byte;
        }
        assert_eq!(codec.recover(&frame, &mut back), None);
    }

    #[test]
    fn known_weakness_tall_narrow_damage_at_1080p_with_8x8_cells() {
        // At 1080p with 8 x 8 cells a row of cells is 30 bytes and a frame
        // has 15 codewords, so every row starts on the same codeword. Damage
        // that is narrow but tall (a stripe, the edge of a logo) therefore
        // keeps hitting the same codewords instead of being spread out.
        // This pins the behaviour of format version 1 as specified; the other
        // three layouts do not line up this way.
        let mode = Mode::Robust {
            cell: Cell::C8,
            parity: 32,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        let codec = Codec::new(layout, 32);
        let row_bytes = layout.width / layout.cell / 8;
        assert_eq!(
            row_bytes % codec.codewords,
            0,
            "rows line up with codewords"
        );

        let block = noise(block_len(layout, &mode), 12);
        let mut frame = vec![0u8; layout.capacity()];
        codec.protect(&block, &mut frame);
        let mut back = vec![0u8; block.len()];
        // One byte (8 cells, 64 pixels) wide, 16 rows (128 pixels) tall: repaired.
        for row in 20..36 {
            frame[row * row_bytes + 7] ^= 0xFF;
        }
        assert_eq!(codec.recover(&frame, &mut back), Some(16));
        // One row taller: 17 bytes of one codeword are gone, and so is the frame,
        // although under 0.5% of the picture was touched.
        frame[36 * row_bytes + 7] ^= 0xFF;
        assert_eq!(codec.recover(&frame, &mut back), None);

        // The same stripe at 1080p with 4 x 4 cells is spread over many codewords.
        let mode = Mode::Robust {
            cell: Cell::C4,
            parity: 32,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        let codec = Codec::new(layout, 32);
        let row_bytes = layout.width / layout.cell / 8;
        assert_ne!(row_bytes % codec.codewords, 0);
        let block = noise(block_len(layout, &mode), 13);
        let mut frame = vec![0u8; layout.capacity()];
        codec.protect(&block, &mut frame);
        let mut back = vec![0u8; block.len()];
        for row in 20..120 {
            frame[row * row_bytes + 7] ^= 0xFF;
        }
        assert_eq!(codec.recover(&frame, &mut back), Some(100));
        assert_eq!(back, block);
    }

    /// Not a pass/fail test: prints how fast the error correction runs.
    /// `cargo test --release -p atv-core --lib rs_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a speed measurement, run by hand"]
    fn rs_speed() {
        use std::time::Instant;
        let mut dice = Dice(0x5EED_5EED_5EED_5EED);
        for (layout, mode) in layouts() {
            let parity = parity_of(&mode);
            let codec = Codec::new(layout, parity);
            let block = noise(block_len(layout, &mode), 8);
            let mut clean = vec![0u8; layout.capacity()];
            let mut back = vec![0u8; block.len()];
            let rounds = (20_000 / codec.codewords).max(8);

            let start = Instant::now();
            for _ in 0..rounds {
                codec.protect(&block, &mut clean);
            }
            let protect = rounds as f64 / start.elapsed().as_secs_f64();

            let start = Instant::now();
            for _ in 0..rounds {
                codec.recover(&clean, &mut back).unwrap();
            }
            let recover_clean = rounds as f64 / start.elapsed().as_secs_f64();

            // Worst case that still decodes: every codeword at its limit.
            let mut worst = clean.clone();
            for j in 0..codec.codewords {
                damage_codeword(
                    &mut worst,
                    codec.codewords,
                    j,
                    usize::from(parity) / 2,
                    &mut dice,
                );
            }
            let start = Instant::now();
            for _ in 0..rounds {
                codec.recover(&worst, &mut back).unwrap();
            }
            let recover_worst = rounds as f64 / start.elapsed().as_secs_f64();

            println!(
                "{}x{} cell {} parity {parity}: {} codewords/frame | protect {protect:.0} frames/s | recover clean {recover_clean:.0} frames/s | recover worst-case {recover_worst:.0} frames/s",
                layout.width, layout.height, layout.cell, codec.codewords
            );
        }
    }
}
