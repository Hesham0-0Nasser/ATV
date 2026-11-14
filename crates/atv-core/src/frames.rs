//! Cutting the sealed stream into video frames and putting it back together
//! (spec 5.2, 5.4, 5.6).
//!
//! [`FrameWriter`] is a `Write`: bytes go in, frames come out to a sink.
//! [`FrameReader`] is a `Read`: frames come in from a source, bytes go out.
//! The sink and source are traits so the unit tests can run without ffmpeg.

use std::io::{self, Read, Write};

use crate::error::AtvError;
use crate::ffmpeg::{Decoder, Encoder};
use crate::format::block::{self, BLOCK_HEADER_LEN};
use crate::format::header::VERSION_2;
use crate::format::header::{self, HEADER_LEN, Header};
use crate::format::{Layout, block_len, grid};
use crate::progress::Ctx;
use crate::robust::{Codec, GROUP_FRAMES, GroupRepair};
use crate::types::{Mode, Resolution};

/// Where finished `monob` frames go.
pub(crate) trait FrameSink {
    fn write_frame(&mut self, mono: &[u8]) -> io::Result<()>;
}

/// Where `gray` frames come from. `Ok(false)` means there are no more.
pub(crate) trait FrameSource {
    fn read_frame(&mut self, gray: &mut [u8]) -> io::Result<bool>;
}

impl FrameSink for Encoder {
    fn write_frame(&mut self, mono: &[u8]) -> io::Result<()> {
        Encoder::write_frame(self, mono)
    }
}

impl FrameSource for Decoder {
    fn read_frame(&mut self, gray: &mut [u8]) -> io::Result<bool> {
        Decoder::read_frame(self, gray)
    }
}

/// Number of identical header frames at the start of every video.
pub(crate) const HEADER_FRAMES: u64 = 2;

/// Turns the sealed stream into data frames.
///
/// A frame is only sent once it is known whether more data follows, because
/// the final frame must carry the "last" flag. [`FrameWriter::finish`] sends
/// that final frame, so there is always at least one data frame.
pub(crate) struct FrameWriter<'a, 'c, S: FrameSink> {
    sink: &'a mut S,
    ctx: &'a Ctx<'c>,
    layout: Layout,
    /// The frame block being filled: 16 header bytes, then the payload.
    block: Vec<u8>,
    /// Payload bytes in `block` so far.
    filled: usize,
    index: u32,
    /// Robust mode only: the error correction, the frame's bytes once parity
    /// has been added, and those bytes drawn as pixels.
    codec: Option<Codec>,
    coded: Vec<u8>,
    mono: Vec<u8>,
    /// Format versions 2 and 3: groups of frames woven together; `index`
    /// then counts groups.
    group: Option<GroupWriter>,
}

struct GroupWriter {
    codec: GroupRepair,
    frames: Vec<Vec<u8>>,
}

impl<'a, 'c, S: FrameSink> FrameWriter<'a, 'c, S> {
    pub(crate) fn new(
        sink: &'a mut S,
        resolution: Resolution,
        mode: &Mode,
        version: u8,
        ctx: &'a Ctx<'c>,
    ) -> Self {
        let layout = Layout::for_mode(resolution, mode);
        let group = match mode {
            Mode::Robust { parity, .. } if version >= VERSION_2 => Some(GroupWriter {
                codec: GroupRepair::new(layout, *parity, version),
                frames: vec![vec![0u8; layout.capacity()]; GROUP_FRAMES],
            }),
            _ => None,
        };
        let codec = if group.is_some() {
            None
        } else {
            robust_codec(layout, mode)
        };
        let robust = codec.is_some() || group.is_some();
        let (coded, mono) = if robust {
            (vec![0u8; layout.capacity()], vec![0u8; layout.mono_len()])
        } else {
            (Vec::new(), Vec::new())
        };
        let block_len = match &group {
            Some(group) => group.codec.block_len(),
            None => block_len(layout, mode),
        };
        FrameWriter {
            sink,
            ctx,
            layout,
            block: vec![0u8; block_len],
            filled: 0,
            index: 0,
            codec,
            coded,
            mono,
            group,
        }
    }

    /// Send the two header frames. Call once, before any data.
    pub(crate) fn write_header(
        &mut self,
        raw: &[u8; HEADER_LEN],
        resolution: Resolution,
    ) -> io::Result<()> {
        let frame = header::draw_frame(raw, resolution);
        for _ in 0..HEADER_FRAMES {
            self.sink.write_frame(&frame)?;
        }
        Ok(())
    }

    fn payload_capacity(&self) -> usize {
        self.block.len() - BLOCK_HEADER_LEN
    }

    fn send(&mut self, last: bool) -> io::Result<()> {
        self.ctx.check_cancel()?;
        block::seal(&mut self.block, self.index, self.filled, last);
        if let Some(group) = &mut self.group {
            // Versions 2 and 3: one block fills a whole group of frames.
            let Some(first) = self.index.checked_mul(GROUP_FRAMES as u32) else {
                return Err(self
                    .ctx
                    .fail(AtvError::invalid("the input is too large for one video")));
            };
            group
                .codec
                .protect(&self.block, first, last, &mut group.frames);
            for frame in &group.frames {
                self.ctx.check_cancel()?;
                grid::draw(frame, self.layout, &mut self.mono);
                self.sink.write_frame(&self.mono)?;
                let (mono, layout) = (&self.mono, self.layout);
                self.ctx
                    .frame_done(|| grid::preview_from_mono(mono, layout));
            }
            self.filled = 0;
            if !last {
                self.index = self.index.checked_add(1).ok_or_else(|| {
                    self.ctx
                        .fail(AtvError::invalid("the input is too large for one video"))
                })?;
            }
            return Ok(());
        }
        let layout = self.layout;
        let frame: &[u8] = match &self.codec {
            // One pixel per bit: the block is the frame, byte for byte.
            None => &self.block,
            // Robust: add parity, weave the codewords, draw the cells.
            Some(codec) => {
                codec.protect(&self.block, &mut self.coded);
                grid::draw(&self.coded, layout, &mut self.mono);
                &self.mono
            }
        };
        self.sink.write_frame(frame)?;
        self.ctx
            .frame_done(|| grid::preview_from_mono(frame, layout));
        self.filled = 0;
        if !last {
            self.index = self.index.checked_add(1).ok_or_else(|| {
                self.ctx
                    .fail(AtvError::invalid("the input is too large for one video"))
            })?;
        }
        Ok(())
    }

    /// Send the final frame. Returns the number of data frames written.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        self.send(true)?;
        let units = u64::from(self.index) + 1;
        Ok(if self.group.is_some() {
            units * GROUP_FRAMES as u64
        } else {
            units
        })
    }
}

impl<S: FrameSink> Write for FrameWriter<'_, '_, S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.filled == self.payload_capacity() {
            // More data has arrived, so the full frame in hand is not the last.
            self.send(false)?;
        }
        let count = buffer.len().min(self.payload_capacity() - self.filled);
        let at = BLOCK_HEADER_LEN + self.filled;
        self.block[at..at + count].copy_from_slice(&buffer[..count]);
        self.filled += count;
        Ok(count)
    }

    /// Frames are sent whole, so there is nothing to push out early.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The error correction for a mode, if it has any.
fn robust_codec(layout: Layout, mode: &Mode) -> Option<Codec> {
    match mode {
        Mode::Lossless => None,
        Mode::Robust { parity, .. } => Some(Codec::new(layout, *parity)),
    }
}

/// Read frames from the start of a video until one holds a valid header.
/// Only the first two frames are tried, since those are the header frames.
pub(crate) fn read_header<S: FrameSource>(
    source: &mut S,
    resolution: Resolution,
    gray: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<(Header, [u8; HEADER_LEN]), AtvError> {
    gray.resize(Layout::for_header(resolution).gray_len(), 0);
    for _ in 0..HEADER_FRAMES {
        if !source.read_frame(gray)? {
            break;
        }
        match header::read_frame(gray, resolution, scratch) {
            Ok(found) => return Ok(found),
            // A readable header from a newer format: say so instead of "not ATV".
            Err(error @ AtvError::UnsupportedVersion { .. }) => return Err(error),
            Err(_) => continue,
        }
    }
    Err(AtvError::NotAtvVideo)
}

/// Turns data frames back into the sealed stream.
///
/// Frame order is checked with the index in each block: a repeated frame is
/// skipped, an unreadable frame is forgiven if a good copy follows, and a
/// missing frame ends the read with `Damaged`.
///
/// In Robust mode each frame goes through error correction first. The bytes
/// it repairs are counted, but only for frames that end up being used.
pub(crate) struct FrameReader<'a, 'c, S: FrameSource> {
    source: &'a mut S,
    ctx: &'a Ctx<'c>,
    layout: Layout,
    gray: Vec<u8>,
    block: Vec<u8>,
    /// Robust mode only: the error correction, and the frame's bytes as read
    /// from the pixels, before repair.
    codec: Option<Codec>,
    coded: Vec<u8>,
    /// Unread payload is `block[position..end]`.
    position: usize,
    end: usize,
    next_index: u32,
    finished: bool,
    /// Format versions 2 and 3; `next_index` then counts groups.
    group: Option<GroupReader>,
}

/// The frames of the group being collected (format versions 2 and 3).
struct GroupReader {
    codec: GroupRepair,
    /// Version 3: how sure each cell of the frame just read is.
    soft: Vec<i8>,
    /// What was read of each position in the group: the frame's bytes
    /// (version 2) or its cells' confidence values (version 3).
    slots: Vec<Option<Vec<u8>>>,
    filled: usize,
    /// A frame of the next group, read while finishing this one.
    pending: Option<(usize, Vec<u8>)>,
    /// Frames read since the last readable tag whose own tag could not be
    /// read (or header frames, at the start).
    unknown: Vec<Vec<u8>>,
    /// The highest data-frame index seen so far.
    last_index: Option<u32>,
}

/// At most this many unplaceable frames are kept while waiting for the next
/// readable tag.
const MAX_UNKNOWN: usize = 2 * GROUP_FRAMES;

impl GroupReader {
    /// Put data frame `index` into the group being collected (`expected`).
    /// Returns whether it was new there.
    fn place(&mut self, index: u32, expected: u32, bytes: Vec<u8>) -> bool {
        let set = index / GROUP_FRAMES as u32;
        let position = (index % GROUP_FRAMES as u32) as usize;
        if set != expected || self.slots[position].is_some() {
            return false;
        }
        self.slots[position] = Some(bytes);
        self.filled += 1;
        true
    }

    /// Frames whose tags were unreadable, between the last readable tag and
    /// data frame `next` (or the end of the video when `None`), are those
    /// frames in order when their count fits the gap exactly. Frames arrive
    /// in order, so a tag broken by compression need not lose the frame's
    /// data. When the count does not fit (a repeated or lost frame nearby),
    /// nothing is guessed. Returns how many were placed.
    fn infer(&mut self, next: Option<u32>, expected: u32) -> usize {
        let unknown = std::mem::take(&mut self.unknown);
        let count = unknown.len() as u32;
        let (first, skip) = match (self.last_index, next) {
            (Some(last), Some(next)) if next > last && next - last - 1 == count => (last + 1, 0),
            // At the end of the video: the frames after the last tag.
            (Some(last), None) => (last + 1, 0),
            // Before the first readable tag, the header frames come first.
            (None, Some(next)) if count >= next => (0, (count - next) as usize),
            _ => return 0,
        };
        let mut placed = 0;
        for (offset, bytes) in unknown.into_iter().skip(skip).enumerate() {
            if self.place(first.wrapping_add(offset as u32), expected, bytes) {
                placed += 1;
            }
        }
        placed
    }
}

impl<'a, 'c, S: FrameSource> FrameReader<'a, 'c, S> {
    /// Read the header frames and get ready to read data frames.
    pub(crate) fn open(
        source: &'a mut S,
        resolution: Resolution,
        ctx: &'a Ctx<'c>,
    ) -> Result<(Self, Header, [u8; HEADER_LEN]), AtvError> {
        let mut gray = Vec::new();
        let mut scratch = Vec::new();
        let (header, raw) = read_header(source, resolution, &mut gray, &mut scratch)?;
        let layout = Layout::for_mode(resolution, &header.mode);
        let group = match header.mode {
            Mode::Robust { parity, .. } if header.version >= VERSION_2 => Some(GroupReader {
                codec: GroupRepair::new(layout, parity, header.version),
                soft: Vec::new(),
                slots: vec![None; GROUP_FRAMES],
                filled: 0,
                pending: None,
                unknown: Vec::new(),
                last_index: None,
            }),
            _ => None,
        };
        let codec = if group.is_some() {
            None
        } else {
            robust_codec(layout, &header.mode)
        };
        let coded = if codec.is_some() || group.is_some() {
            vec![0u8; layout.capacity()]
        } else {
            Vec::new()
        };
        let block_len = match &group {
            Some(group) => group.codec.block_len(),
            None => block_len(layout, &header.mode),
        };
        let reader = FrameReader {
            source,
            ctx,
            layout,
            gray,
            block: vec![0u8; block_len],
            codec,
            coded,
            position: 0,
            end: 0,
            next_index: 0,
            finished: false,
            group,
        };
        Ok((reader, header, raw))
    }

    fn damaged(&self) -> io::Error {
        let frame = if self.group.is_some() {
            u64::from(self.next_index) * GROUP_FRAMES as u64
        } else {
            u64::from(self.next_index)
        };
        self.ctx.fail(AtvError::Damaged { frame })
    }

    /// The video ended before the frame with the "last" flag, with nothing
    /// damaged on the way: it was cut short or not fully copied.
    fn truncated(&self) -> io::Error {
        self.ctx.fail(AtvError::Truncated)
    }

    /// Format versions 2 and 3: collect the frames of the next group (in any order,
    /// ignoring repeats and frames that cannot be placed), then repair the
    /// group, treating its missing frames as erasures.
    fn load_next_group(&mut self) -> io::Result<()> {
        let expected = self.next_index;
        loop {
            let Some(group) = self.group.as_mut() else {
                return Err(self.damaged());
            };
            if group.filled == GROUP_FRAMES {
                break;
            }
            self.ctx.check_cancel()?;
            if !self.source.read_frame(&mut self.gray)? {
                // The video ended: the unplaced frames after the last tag are
                // the next ones in order; then repair with what there is.
                group.infer(None, expected);
                if group.filled == 0 {
                    return Err(self.truncated());
                }
                break;
            }
            // What is kept of the frame: its bytes, or for version 3 how
            // sure each cell is (each value as its two's-complement byte).
            let kept = if group.codec.soft() {
                group.soft.resize(self.layout.capacity() * 8, 0);
                grid::read_soft(&self.gray, self.layout, &mut self.coded, &mut group.soft);
                group.soft.iter().map(|&value| value as u8).collect()
            } else {
                grid::read(&self.gray, self.layout, &mut self.coded);
                self.coded.clone()
            };
            let Some(tag) = group.codec.read_tag(&self.coded) else {
                // A header frame, or a frame whose tag was too damaged to read.
                if group.unknown.len() == MAX_UNKNOWN {
                    group.unknown.remove(0);
                }
                group.unknown.push(kept);
                continue;
            };
            let inferred = group.infer(Some(tag.index), expected);
            if group.last_index.is_none_or(|last| tag.index > last) {
                group.last_index = Some(tag.index);
            }
            let set = tag.index / GROUP_FRAMES as u32;
            let position = (tag.index % GROUP_FRAMES as u32) as usize;
            let (gray, layout) = (&self.gray, self.layout);
            for _ in 0..inferred {
                self.ctx
                    .frame_done(|| grid::preview_from_gray(gray, layout));
            }
            if set < expected {
                continue; // a repeat of a frame already used
            }
            if set > expected {
                if set == expected.wrapping_add(1) && group.filled > 0 {
                    group.pending = Some((position, kept));
                    break;
                }
                return Err(self.damaged()); // a whole group is missing
            }
            if group.place(tag.index, expected, kept) {
                self.ctx
                    .frame_done(|| grid::preview_from_gray(gray, layout));
            }
        }
        let outcome = match self.group.as_mut() {
            Some(group) => {
                let views: Vec<Option<&[u8]>> =
                    group.slots.iter().map(|slot| slot.as_deref()).collect();
                group.codec.recover(&views, &mut self.block)
            }
            None => None,
        };
        let info = outcome.and_then(|_| block::open(&self.block));
        let (Some(repaired), Some(info)) = (outcome, info) else {
            return Err(self.damaged());
        };
        if info.index != expected {
            return Err(self.damaged());
        }
        self.ctx.add_repaired(repaired);
        self.position = BLOCK_HEADER_LEN;
        self.end = BLOCK_HEADER_LEN + info.payload_len;
        self.finished = info.last;
        self.next_index = expected.wrapping_add(1);
        if let Some(group) = self.group.as_mut() {
            group.slots.iter_mut().for_each(|slot| *slot = None);
            group.filled = 0;
            if let Some((position, bytes)) = group.pending.take() {
                group.slots[position] = Some(bytes);
                group.filled = 1;
            }
        }
        Ok(())
    }

    /// Read frames until the next expected block is in `self.block`.
    fn load_next(&mut self) -> io::Result<()> {
        if self.group.is_some() {
            return self.load_next_group();
        }
        // An unreadable frame skipped on the way to the end means damage; none
        // means the end itself is missing. Before the first data frame, the
        // second header frame is skipped as unreadable too, so it is allowed.
        let mut skipped = 0u32;
        loop {
            self.ctx.check_cancel()?;
            if !self.source.read_frame(&mut self.gray)? {
                // The video ended before the frame with the "last" flag.
                let header_copy = u32::from(self.next_index == 0);
                return Err(if skipped > header_copy {
                    self.damaged()
                } else {
                    self.truncated()
                });
            }
            let (info, repaired) = match &self.codec {
                // One pixel per bit: thresholding the frame gives the block.
                None => {
                    grid::read(&self.gray, self.layout, &mut self.block);
                    (block::open(&self.block), 0)
                }
                // Robust: read the cells, repair the codewords, then check
                // the block. A frame past repair counts as unreadable.
                Some(codec) => {
                    grid::read(&self.gray, self.layout, &mut self.coded);
                    match codec.recover(&self.coded, &mut self.block) {
                        Some(repaired) => (block::open(&self.block), repaired),
                        None => (None, 0),
                    }
                }
            };
            match info {
                Some(info) if info.index == self.next_index => {
                    self.ctx.add_repaired(repaired);
                    self.position = BLOCK_HEADER_LEN;
                    self.end = BLOCK_HEADER_LEN + info.payload_len;
                    self.finished = info.last;
                    self.next_index = self.next_index.wrapping_add(1);
                    let (gray, layout) = (&self.gray, self.layout);
                    self.ctx
                        .frame_done(|| grid::preview_from_gray(gray, layout));
                    return Ok(());
                }
                // The frame we already have, shown again: skip it.
                Some(info) if info.index.wrapping_add(1) == self.next_index => continue,
                // A good frame from further on: the one we need is gone.
                Some(_) => return Err(self.damaged()),
                // Unreadable: the second header frame, an extra copy of it, or
                // a damaged data frame. Carry on. If a good copy of the frame
                // we need follows, it is used; if not, the gap is caught above
                // or at the end of the video.
                None => {
                    skipped = skipped.saturating_add(1);
                    continue;
                }
            }
        }
    }
}

impl<S: FrameSource> Read for FrameReader<'_, '_, S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.position == self.end {
            if self.finished {
                return Ok(0);
            }
            self.load_next()?;
        }
        let count = out.len().min(self.end - self.position);
        out[..count].copy_from_slice(&self.block[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::{mono_to_gray, noise};
    use crate::types::{CancelToken, Event, Stage};

    /// Keeps every frame in memory instead of sending it to ffmpeg.
    #[derive(Default)]
    pub(crate) struct MemorySink {
        pub frames: Vec<Vec<u8>>,
    }

    impl FrameSink for MemorySink {
        fn write_frame(&mut self, mono: &[u8]) -> io::Result<()> {
            self.frames.push(mono.to_vec());
            Ok(())
        }
    }

    /// Plays frames back as ffmpeg would deliver them: one byte per pixel.
    pub(crate) struct MemorySource {
        frames: std::vec::IntoIter<Vec<u8>>,
    }

    impl MemorySource {
        pub(crate) fn new(mono_frames: Vec<Vec<u8>>) -> Self {
            MemorySource {
                frames: mono_frames.into_iter(),
            }
        }
    }

    impl FrameSource for MemorySource {
        fn read_frame(&mut self, gray: &mut [u8]) -> io::Result<bool> {
            match self.frames.next() {
                Some(mono) => {
                    gray.copy_from_slice(&mono_to_gray(&mono));
                    Ok(true)
                }
                None => Ok(false),
            }
        }
    }

    const RES: Resolution = Resolution::P1080;
    const PAYLOAD: usize = 259_184;

    /// Write `data` as frames in memory.
    fn to_frames(data: &[u8]) -> Vec<Vec<u8>> {
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::WritingFrames);
        let mut sink = MemorySink::default();
        let mut writer = FrameWriter::new(&mut sink, RES, &Mode::Lossless, 1, &ctx);
        let raw = Header::plain(Mode::Lossless).to_bytes();
        writer.write_header(&raw, RES).unwrap();
        // Uneven pieces, so frame boundaries fall in the middle of writes.
        for piece in data.chunks(100_003) {
            writer.write_all(piece).unwrap();
        }
        let count = writer.finish().unwrap();
        assert_eq!(count as usize + 2, sink.frames.len());
        sink.frames
    }

    /// Read frames back into bytes, resolving any parked engine error.
    fn from_frames(frames: Vec<Vec<u8>>) -> Result<Vec<u8>, AtvError> {
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::ReadingFrames);
        let mut source = MemorySource::new(frames);
        let (mut reader, header, _) = FrameReader::open(&mut source, RES, &ctx)?;
        assert_eq!(header, Header::plain(Mode::Lossless));
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(|error| ctx.resolve(error.into()))?;
        Ok(data)
    }

    #[test]
    fn data_of_every_awkward_size_round_trips() {
        let sizes = [
            0,
            1,
            PAYLOAD - 1,
            PAYLOAD,
            PAYLOAD + 1,
            2 * PAYLOAD,
            3 * PAYLOAD + 17,
        ];
        for size in sizes {
            let data = noise(size, size as u64 + 1);
            let frames = to_frames(&data);
            let expected_data_frames = size.div_ceil(PAYLOAD).max(1);
            assert_eq!(frames.len(), 2 + expected_data_frames, "size {size}");
            assert_eq!(from_frames(frames).unwrap(), data, "size {size}");
        }
    }

    #[test]
    fn only_the_final_frame_has_the_last_flag() {
        let frames = to_frames(&noise(2 * PAYLOAD, 4));
        assert_eq!(frames.len(), 4);
        let first = block::open(&frames[2]).unwrap();
        let second = block::open(&frames[3]).unwrap();
        assert_eq!(
            (first.index, first.last, first.payload_len),
            (0, false, PAYLOAD)
        );
        assert_eq!(
            (second.index, second.last, second.payload_len),
            (1, true, PAYLOAD)
        );
    }

    #[test]
    fn the_two_header_frames_are_identical() {
        let frames = to_frames(b"x");
        assert_eq!(frames[0], frames[1]);
    }

    #[test]
    fn a_damaged_first_header_frame_falls_back_to_the_second() {
        let data = noise(PAYLOAD + 5, 6);
        let mut frames = to_frames(&data);
        frames[0].fill(0);
        assert_eq!(from_frames(frames).unwrap(), data);
    }

    #[test]
    fn no_header_in_either_frame_is_not_an_atv_video() {
        let mut frames = to_frames(b"data");
        frames[0].fill(0);
        frames[1].fill(0xFF);
        assert!(matches!(from_frames(frames), Err(AtvError::NotAtvVideo)));
        assert!(matches!(
            from_frames(Vec::new()),
            Err(AtvError::NotAtvVideo)
        ));
    }

    #[test]
    fn repeated_frames_are_skipped() {
        let data = noise(3 * PAYLOAD, 8);
        let frames = to_frames(&data);
        // Every frame twice, headers included, as a frame-doubling re-encode would do.
        let doubled: Vec<_> = frames.iter().flat_map(|f| [f.clone(), f.clone()]).collect();
        assert_eq!(from_frames(doubled).unwrap(), data);
    }

    #[test]
    fn a_bad_frame_followed_by_a_good_copy_is_forgiven() {
        let data = noise(3 * PAYLOAD, 9);
        let frames = to_frames(&data);
        let mut bad = frames[3].clone();
        bad[5000] ^= 0xFF;
        let mut patched = frames.clone();
        patched.insert(3, bad);
        assert_eq!(from_frames(patched).unwrap(), data);
    }

    #[test]
    fn a_missing_frame_is_reported_with_its_index() {
        let frames = to_frames(&noise(3 * PAYLOAD, 10));
        let mut missing = frames.clone();
        missing.remove(3); // data frame 1
        assert!(matches!(
            from_frames(missing),
            Err(AtvError::Damaged { frame: 1 })
        ));
    }

    #[test]
    fn a_bad_frame_with_no_good_copy_is_reported() {
        let mut frames = to_frames(&noise(3 * PAYLOAD, 11));
        frames[3][5000] ^= 0xFF; // data frame 1
        assert!(matches!(
            from_frames(frames),
            Err(AtvError::Damaged { frame: 1 })
        ));
    }

    #[test]
    fn a_video_cut_short_is_reported() {
        let mut frames = to_frames(&noise(3 * PAYLOAD, 12));
        frames.pop();
        assert!(matches!(from_frames(frames), Err(AtvError::Truncated)));
    }

    #[test]
    fn a_damaged_last_frame_is_damage_not_a_cut() {
        let mut frames = to_frames(&noise(3 * PAYLOAD, 13));
        let last = frames.len() - 1;
        frames[last][5000] ^= 0xFF;
        assert!(matches!(
            from_frames(frames),
            Err(AtvError::Damaged { frame: 2 })
        ));
    }

    #[test]
    fn frames_after_the_last_one_are_ignored() {
        let data = noise(PAYLOAD / 2, 13);
        let mut frames = to_frames(&data);
        let junk = frames[2].clone();
        frames.push(junk);
        frames.push(vec![0xFF; frames[0].len()]);
        assert_eq!(from_frames(frames).unwrap(), data);
    }

    #[test]
    fn cancelling_stops_writing_and_reading() {
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::WritingFrames);
        let mut sink = MemorySink::default();
        let mut writer = FrameWriter::new(&mut sink, RES, &Mode::Lossless, 1, &ctx);
        writer.write_all(&noise(PAYLOAD, 14)).unwrap();
        cancel.cancel();
        let error = writer.write_all(b"more").unwrap_err();
        assert!(matches!(ctx.resolve(error.into()), AtvError::Cancelled));
        assert!(sink.frames.is_empty(), "no frame is sent after cancelling");

        let frames = to_frames(&noise(PAYLOAD, 15));
        let cancel = CancelToken::new();
        let mut ignore = |_event: Event| {};
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::ReadingFrames);
        let mut source = MemorySource::new(frames);
        let (mut reader, _, _) = FrameReader::open(&mut source, RES, &ctx).unwrap();
        cancel.cancel();
        let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(matches!(ctx.resolve(error.into()), AtvError::Cancelled));
    }

    #[test]
    fn progress_counts_frames_and_carries_previews() {
        let mut events: Vec<Event> = Vec::new();
        let mut record = |event: Event| events.push(event);
        let cancel = CancelToken::new();
        {
            let ctx = Ctx::new(&mut record, &cancel, Stage::WritingFrames);
            let mut sink = MemorySink::default();
            let mut writer = FrameWriter::new(&mut sink, RES, &Mode::Lossless, 1, &ctx);
            writer.write_all(&noise(2 * PAYLOAD + 1, 16)).unwrap();
            writer.finish().unwrap();
            ctx.emit_now();
        }
        let last = events.last().unwrap();
        assert_eq!(last.frames_done, 3);
        let preview = events
            .iter()
            .find_map(|event| event.preview.as_ref())
            .unwrap();
        assert_eq!((preview.width, preview.height), (480, 270));
        assert!(preview.pixels.iter().all(|p| *p == 0 || *p == 255));
    }
}
