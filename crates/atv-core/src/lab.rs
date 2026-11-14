//! Measuring how much a copy of an ATV video was damaged, for real-site
//! tests. Not used by encode or decode.
//!
//! Both videos are read cell by cell. Each frame of the copy is matched to
//! the frame of the original it looks most like (a site may repeat or drop
//! frames), and the wrong bits are counted. This works even when the copy is
//! too damaged to decode, which is exactly when the numbers matter.
//!
//! The original is held in memory, so this is meant for small test videos.

use std::path::Path;

use crate::error::AtvError;
use crate::ffmpeg::{self, Decoder, FfmpegTools};
use crate::format::header;
use crate::format::{Layout, block_len, grid};
use crate::robust::{Codec, GROUP_FRAMES, GroupRepair};
use crate::types::{Mode, Resolution};

/// What a copy of a video lost.
#[derive(Debug, Clone, PartialEq)]
pub struct DamageReport {
    pub resolution: Resolution,
    pub mode: Mode,
    /// The original's ATV format version (1, 2 or 3).
    pub format_version: u8,
    /// Codec of the copy, as ffprobe names it.
    pub copy_codec: String,
    /// Frame size of the copy as it was stored (it is read scaled back).
    pub copy_size: (u32, u32),
    /// Data frames in the original and in the copy.
    pub original_frames: u64,
    pub copy_frames: u64,
    /// Copy frames that matched an original frame.
    pub compared: u64,
    /// Copy frames too different from every nearby original frame to match
    /// (more than a quarter of their bits wrong).
    pub unmatched: u64,
    /// Original frames that no copy frame matched: lost.
    pub missing: u64,
    /// Share of wrong bits, averaged over matched frames, and the worst frame.
    pub mean_ber: f64,
    pub worst_ber: f64,
    /// Original index of the worst frame.
    pub worst_frame: u64,
    /// Robust mode: matched frames where at least one codeword was past repair.
    pub beyond_repair: u64,
}

/// How far a copy frame may be from where it is expected to be.
const SEARCH: usize = 3;
/// Above this share of wrong bits, a frame is treated as not matching at all.
const NO_MATCH: f64 = 0.25;

/// Compare a copy of an ATV video with the original it was made from.
///
/// A copy that a site resized (same 16:9 shape, another size) is scaled
/// back to the original's size, as decoding does. Any other shape gives
/// `UnsupportedResolution`.
pub fn measure_damage(
    original: &Path,
    copy: &Path,
    tools: &FfmpegTools,
) -> Result<DamageReport, AtvError> {
    let first = ffmpeg::probe_stream(tools, original)?;
    let resolution = Resolution::from_dimensions(first.width, first.height).ok_or(
        AtvError::UnsupportedResolution {
            width: first.width,
            height: first.height,
        },
    )?;
    let second = ffmpeg::probe_stream(tools, copy)?;
    let scale_to = if (second.width, second.height) == (first.width, first.height) {
        None
    } else if crate::decode::is_16_by_9(second.width, second.height) {
        Some(resolution)
    } else {
        return Err(AtvError::UnsupportedResolution {
            width: second.width,
            height: second.height,
        });
    };

    // The original: its header says how the cells are laid out.
    let mut decoder = Decoder::spawn(tools, original, None, None)?;
    let mut gray = vec![0u8; Layout::for_header(resolution).gray_len()];
    let mut frames: Vec<Vec<u8>> = Vec::new();
    while decoder.read_frame(&mut gray)? {
        frames.push(gray.clone());
    }
    drop(decoder);
    let (header, _) = frames
        .first()
        .ok_or(AtvError::NotAtvVideo)
        .and_then(|frame| header::read_frame(frame, resolution, &mut Vec::new()))?;
    let layout = Layout::for_mode(resolution, &header.mode);
    let original_bits: Vec<Vec<u8>> = frames
        .iter()
        .skip(2)
        .map(|frame| cells(frame, layout))
        .collect();
    drop(frames);

    // The copy, frame by frame; for format version 3 also how sure each
    // cell is, which its repair uses.
    let soft = header.version >= crate::format::header::VERSION_3;
    let mut decoder = Decoder::spawn(tools, copy, None, scale_to)?;
    let mut copy_bits = Vec::new();
    let mut copy_soft = Vec::new();
    let mut index = 0usize;
    while decoder.read_frame(&mut gray)? {
        // The first two frames are the header frames.
        if index >= 2 {
            if soft {
                let (bits, values) = soft_cells(&gray, layout);
                copy_bits.push(bits);
                copy_soft.push(values);
            } else {
                copy_bits.push(cells(&gray, layout));
            }
        }
        index += 1;
    }

    let mut report = compare(
        &original_bits,
        &copy_bits,
        &copy_soft,
        &header.mode,
        header.version,
        layout,
    );
    report.resolution = resolution;
    report.copy_codec = second.codec;
    report.copy_size = (second.width, second.height);
    Ok(report)
}

fn cells(gray: &[u8], layout: Layout) -> Vec<u8> {
    let mut bits = vec![0u8; layout.capacity()];
    grid::read(gray, layout, &mut bits);
    bits
}

/// The cells as bytes, and how sure each is (as two's-complement bytes).
fn soft_cells(gray: &[u8], layout: Layout) -> (Vec<u8>, Vec<u8>) {
    let mut bits = vec![0u8; layout.capacity()];
    let mut soft = vec![0i8; layout.capacity() * 8];
    grid::read_soft(gray, layout, &mut bits, &mut soft);
    (bits, soft.into_iter().map(|value| value as u8).collect())
}

fn wrong_bits(a: &[u8], b: &[u8]) -> u64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| u64::from((x ^ y).count_ones()))
        .sum()
}

/// The matching and counting, separate from reading video so it can be
/// tested. `copy_soft` holds the copy's cell confidences for format version
/// 3 (empty otherwise).
fn compare(
    original: &[Vec<u8>],
    copy: &[Vec<u8>],
    copy_soft: &[Vec<u8>],
    mode: &Mode,
    version: u8,
    layout: Layout,
) -> DamageReport {
    // Versions 2 and 3 group frames; a frame alone is not 'past repair', a group is.
    let group_codec = match mode {
        Mode::Robust { parity, .. } if version >= 2 => {
            Some(GroupRepair::new(layout, *parity, version))
        }
        _ => None,
    };
    let codec = match mode {
        Mode::Robust { parity, .. } if group_codec.is_none() => Some(Codec::new(layout, *parity)),
        _ => None,
    };
    let mut copy_at: Vec<Option<usize>> = vec![None; original.len()];
    let mut block = vec![0u8; block_len(layout, mode)];
    let bits_per_frame = (layout.capacity() * 8) as f64;
    let mut matched = vec![false; original.len()];
    let mut report = DamageReport {
        resolution: Resolution::P1080,
        mode: *mode,
        format_version: version,
        copy_codec: String::new(),
        copy_size: (0, 0),
        original_frames: original.len() as u64,
        copy_frames: copy.len() as u64,
        compared: 0,
        unmatched: 0,
        missing: 0,
        mean_ber: 0.0,
        worst_ber: 0.0,
        worst_frame: 0,
        beyond_repair: 0,
    };
    let mut total_ber = 0.0;
    let mut expected = 0usize;
    for (copy_index, frame) in copy.iter().enumerate() {
        if original.is_empty() {
            break;
        }
        let low = expected.saturating_sub(SEARCH);
        let high = (expected + SEARCH + 1).min(original.len());
        let best = (low..high)
            .map(|candidate| (candidate, wrong_bits(&original[candidate], frame)))
            .min_by_key(|(_, wrong)| *wrong);
        let Some((at, wrong)) = best else {
            report.unmatched += 1;
            continue;
        };
        let ber = wrong as f64 / bits_per_frame;
        if ber > NO_MATCH {
            report.unmatched += 1;
            continue;
        }
        report.compared += 1;
        matched[at] = true;
        copy_at[at].get_or_insert(copy_index);
        total_ber += ber;
        if ber > report.worst_ber {
            report.worst_ber = ber;
            report.worst_frame = at as u64;
        }
        if let Some(codec) = &codec
            && codec.recover(frame, &mut block).is_none()
        {
            report.beyond_repair += 1;
        }
        expected = at + 1;
    }
    if let Some(codec) = &group_codec {
        // Frames in groups that cannot be rebuilt, missing frames counting
        // as erasures, exactly as the decoder would.
        let mut block = vec![0u8; codec.block_len()];
        let read = if codec.soft() { copy_soft } else { copy };
        for start in (0..original.len()).step_by(GROUP_FRAMES) {
            let views: Vec<Option<&[u8]>> = (start..start + GROUP_FRAMES)
                .map(|at| {
                    copy_at
                        .get(at)
                        .copied()
                        .flatten()
                        .and_then(|c| read.get(c))
                        .map(Vec::as_slice)
                })
                .collect();
            if codec.recover(&views, &mut block).is_none() {
                report.beyond_repair += GROUP_FRAMES as u64;
            }
        }
    }
    report.missing = matched.iter().filter(|seen| !**seen).count() as u64;
    if report.compared > 0 {
        report.mean_ber = total_ber / report.compared as f64;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::noise;
    use crate::types::Cell;

    fn setup() -> (Mode, Layout, Vec<Vec<u8>>) {
        let mode = Mode::Robust {
            cell: Cell::C8,
            parity: 32,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        let codec = Codec::new(layout, 32);
        let frames = (0..6)
            .map(|seed| {
                let block = noise(block_len(layout, &mode), seed + 1);
                let mut frame = vec![0u8; layout.capacity()];
                codec.protect(&block, &mut frame);
                frame
            })
            .collect();
        (mode, layout, frames)
    }

    #[test]
    fn a_perfect_copy_has_no_errors() {
        let (mode, layout, original) = setup();
        let report = compare(&original, &original, &[], &mode, 1, layout);
        assert_eq!(
            (report.compared, report.missing, report.unmatched),
            (6, 0, 0)
        );
        assert_eq!(report.mean_ber, 0.0);
        assert_eq!(report.beyond_repair, 0);
    }

    #[test]
    fn errors_are_counted_and_the_worst_frame_found() {
        let (mode, layout, original) = setup();
        let mut copy = original.clone();
        // 40 wrong bits in frame 2, 4,000 in frame 4.
        for byte in &mut copy[2][..5] {
            *byte ^= 0xFF;
        }
        for byte in &mut copy[4][..500] {
            *byte ^= 0xFF;
        }
        let report = compare(&original, &copy, &[], &mode, 1, layout);
        let bits = (layout.capacity() * 8) as f64;
        assert_eq!(report.worst_frame, 4);
        assert!((report.worst_ber - 4000.0 / bits).abs() < 1e-12);
        assert!((report.mean_ber - 4040.0 / bits / 6.0).abs() < 1e-12);
        assert_eq!(
            report.beyond_repair, 1,
            "500 bytes in a row are past repair, 5 are not"
        );
    }

    #[test]
    fn repeated_and_dropped_frames_are_matched() {
        let (mode, layout, original) = setup();
        // Frame 1 shown twice, frame 3 dropped.
        let copy = vec![
            original[0].clone(),
            original[1].clone(),
            original[1].clone(),
            original[2].clone(),
            original[4].clone(),
            original[5].clone(),
        ];
        let report = compare(&original, &copy, &[], &mode, 1, layout);
        assert_eq!(report.compared, 6);
        assert_eq!(report.missing, 1, "frame 3 is lost");
        assert_eq!(report.mean_ber, 0.0);
    }

    #[test]
    fn version_3_groups_are_judged_with_soft_values_and_lost_frames() {
        let mode = Mode::Robust {
            cell: Cell::C8,
            parity: 64,
        };
        let layout = Layout::for_mode(Resolution::P1080, &mode);
        let codec = crate::robust::LdpcGroupCodec::new(layout, 64);
        let mut frames = vec![vec![0u8; layout.capacity()]; GROUP_FRAMES];
        codec.protect(&noise(codec.block_len(), 5), 0, true, &mut frames);
        // Clean copies: every cell sure of its bit.
        let soft: Vec<Vec<u8>> = frames
            .iter()
            .map(|frame| {
                (0..layout.capacity() * 8)
                    .map(|cell| {
                        let one = frame[cell / 8] & (0x80 >> (cell % 8)) != 0;
                        (if one { 127i8 } else { -127 }) as u8
                    })
                    .collect()
            })
            .collect();
        let judge = |keep: &[usize]| {
            let copy: Vec<Vec<u8>> = keep.iter().map(|&f| frames[f].clone()).collect();
            let copy_soft: Vec<Vec<u8>> = keep.iter().map(|&f| soft[f].clone()).collect();
            compare(&frames, &copy, &copy_soft, &mode, 3, layout)
        };
        let all: Vec<usize> = (0..GROUP_FRAMES).collect();
        assert_eq!(judge(&all).beyond_repair, 0);
        let one_lost: Vec<usize> = (0..GROUP_FRAMES).filter(|&f| f != 6).collect();
        let report = judge(&one_lost);
        assert_eq!(
            (report.missing, report.beyond_repair),
            (1, 0),
            "strong repair survives a lost frame"
        );
        let three_lost: Vec<usize> = (3..GROUP_FRAMES).collect();
        assert_eq!(judge(&three_lost).beyond_repair, GROUP_FRAMES as u64);
    }

    #[test]
    fn garbage_frames_do_not_match() {
        let (mode, layout, original) = setup();
        let copy = vec![noise(layout.capacity(), 99); 3];
        let report = compare(&original, &copy, &[], &mode, 1, layout);
        assert_eq!(
            (report.compared, report.unmatched, report.missing),
            (0, 3, 6)
        );
    }
}
