//! The ATV video format, version 1 (spec section 5): how bytes become pixels.

pub(crate) mod block;
pub(crate) mod grid;
pub(crate) mod header;

use crate::types::{Mode, Resolution};

/// Reed-Solomon codeword length in bytes (Robust mode).
pub(crate) const CODEWORD: usize = 255;

/// The pixel grid of one frame: its size and how many pixels carry one bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub width: usize,
    pub height: usize,
    /// Side of the square of pixels that carries one bit: 1, 4 or 8.
    pub cell: usize,
}

impl Layout {
    /// The grid used by data frames in this mode.
    pub(crate) fn for_mode(resolution: Resolution, mode: &Mode) -> Self {
        Self::with_cell(resolution, mode.cell_pixels() as usize)
    }

    /// The grid used by header frames: always 8 x 8 cells, whatever the mode.
    pub(crate) fn for_header(resolution: Resolution) -> Self {
        Self::with_cell(resolution, 8)
    }

    fn with_cell(resolution: Resolution, cell: usize) -> Self {
        Layout {
            width: resolution.width() as usize,
            height: resolution.height() as usize,
            cell,
        }
    }

    /// Bytes one frame can carry: one bit per cell.
    pub(crate) fn capacity(&self) -> usize {
        (self.width / self.cell) * (self.height / self.cell) / 8
    }

    /// Size of a frame in ffmpeg's `monob` format (one bit per pixel).
    pub(crate) fn mono_len(&self) -> usize {
        self.width * self.height / 8
    }

    /// Size of a frame in ffmpeg's `gray` format (one byte per pixel).
    pub(crate) fn gray_len(&self) -> usize {
        self.width * self.height
    }
}

/// Size in bytes of the frame block (block header plus payload plus padding)
/// that one data frame carries.
pub(crate) fn block_len(layout: Layout, mode: &Mode) -> usize {
    match mode {
        Mode::Lossless => layout.capacity(),
        Mode::Robust { parity, .. } => {
            let codewords = layout.capacity() / CODEWORD;
            codewords * CODEWORD.saturating_sub(*parity as usize)
        }
    }
}

/// Bytes of the sealed stream that one data frame carries.
pub(crate) fn payload_len(layout: Layout, mode: &Mode) -> usize {
    block_len(layout, mode).saturating_sub(block::BLOCK_HEADER_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Cell;

    fn robust(cell: Cell) -> Mode {
        Mode::Robust { cell, parity: 32 }
    }

    #[test]
    fn capacities_match_the_spec_table() {
        let cases = [
            (Resolution::P1080, 1, 259_200),
            (Resolution::P1080, 4, 16_200),
            (Resolution::P1080, 8, 4_050),
            (Resolution::P2160, 1, 1_036_800),
            (Resolution::P2160, 4, 64_800),
            (Resolution::P2160, 8, 16_200),
        ];
        for (resolution, cell, expected) in cases {
            assert_eq!(Layout::with_cell(resolution, cell).capacity(), expected);
        }
    }

    #[test]
    fn payloads_match_the_spec_table() {
        let cases = [
            (Resolution::P1080, Mode::Lossless, 259_184),
            (Resolution::P2160, Mode::Lossless, 1_036_784),
            (Resolution::P1080, robust(Cell::C4), 14_033),
            (Resolution::P1080, robust(Cell::C8), 3_329),
            (Resolution::P2160, robust(Cell::C4), 56_626),
            (Resolution::P2160, robust(Cell::C8), 14_033),
        ];
        for (resolution, mode, expected) in cases {
            let layout = Layout::for_mode(resolution, &mode);
            assert_eq!(
                payload_len(layout, &mode),
                expected,
                "{resolution:?} {mode:?}"
            );
        }
    }
}
