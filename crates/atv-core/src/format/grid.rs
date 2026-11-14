//! Turning frame bytes into pixels and pixels back into bytes (spec 5.1, 5.5).
//!
//! Bits run row by row from the top-left, most significant bit of each byte
//! first. A 1 bit is white, a 0 bit is black. With one pixel per bit this is
//! exactly ffmpeg's `monob` pixel format.

use super::Layout;
use crate::types::Preview;

/// Width of the preview image. Both frame widths divide by it evenly.
const PREVIEW_WIDTH: usize = 480;

/// Draw `bits` (one frame's worth, `layout.capacity()` bytes) as a `monob`
/// frame of `layout.mono_len()` bytes.
pub(crate) fn draw(bits: &[u8], layout: Layout, mono: &mut [u8]) {
    debug_assert_eq!(bits.len(), layout.capacity());
    debug_assert_eq!(mono.len(), layout.mono_len());
    let cell = layout.cell;
    if cell == 1 {
        mono.copy_from_slice(bits);
        return;
    }
    // Bytes in one row of pixels, and data bytes in one row of cells.
    let stride = layout.width / 8;
    let data_per_row = layout.width / cell / 8;
    for (row, data) in bits.chunks_exact(data_per_row).enumerate() {
        let band = &mut mono[row * cell * stride..(row + 1) * cell * stride];
        let (first, rest) = band.split_at_mut(stride);
        expand_row(data, cell, first);
        for copy in rest.chunks_exact_mut(stride) {
            copy.copy_from_slice(first);
        }
    }
}

/// Stretch one row of cells to one row of pixels.
fn expand_row(data: &[u8], cell: usize, pixels: &mut [u8]) {
    match cell {
        4 => {
            // Two data bits fill one byte of pixels.
            const PAIR: [u8; 4] = [0x00, 0x0F, 0xF0, 0xFF];
            for (byte, out) in data.iter().zip(pixels.chunks_exact_mut(4)) {
                for (k, px) in out.iter_mut().enumerate() {
                    *px = PAIR[usize::from((byte >> (6 - 2 * k)) & 0b11)];
                }
            }
        }
        8 => {
            // One data bit fills one byte of pixels.
            for (byte, out) in data.iter().zip(pixels.chunks_exact_mut(8)) {
                for (k, px) in out.iter_mut().enumerate() {
                    *px = if byte & (0x80 >> k) != 0 { 0xFF } else { 0x00 };
                }
            }
        }
        _ => {
            // Any other size, one pixel at a time. Not used by format version 1.
            pixels.fill(0);
            for (i, byte) in data.iter().enumerate() {
                for k in 0..8 {
                    if byte & (0x80 >> k) == 0 {
                        continue;
                    }
                    let start = (i * 8 + k) * cell;
                    for x in start..start + cell {
                        pixels[x / 8] |= 0x80 >> (x % 8);
                    }
                }
            }
        }
    }
}

/// Read a `gray` frame (`layout.gray_len()` bytes) back into
/// `layout.capacity()` bytes.
///
/// With one pixel per bit, a pixel is 1 when its value is at least 128. With
/// larger cells only the centre quarter of each cell is averaged, because the
/// edges are the part most damaged by re-compression.
pub(crate) fn read(gray: &[u8], layout: Layout, bits: &mut [u8]) {
    debug_assert_eq!(gray.len(), layout.gray_len());
    debug_assert_eq!(bits.len(), layout.capacity());
    let cell = layout.cell;
    if cell == 1 {
        let (groups, _) = gray.as_chunks::<8>();
        for (out, group) in bits.iter_mut().zip(groups) {
            *out = pack_top_bits(*group);
        }
        return;
    }
    let half = cell / 2;
    let inset = cell / 4;
    let data_per_row = layout.width / cell / 8;
    // The bit is 1 when the average is above 127.5, i.e. 2 * sum > 255 * count.
    let threshold = 255 * (half * half) as u32;
    for (row, out_row) in bits.chunks_exact_mut(data_per_row).enumerate() {
        let top = row * cell + inset;
        for (i, out) in out_row.iter_mut().enumerate() {
            let mut byte = 0u8;
            for k in 0..8 {
                let left = (i * 8 + k) * cell + inset;
                let mut sum = 0u32;
                for dy in 0..half {
                    let start = (top + dy) * layout.width + left;
                    for &value in &gray[start..start + half] {
                        sum += u32::from(value);
                    }
                }
                if 2 * sum > threshold {
                    byte |= 0x80 >> k;
                }
            }
            *out = byte;
        }
    }
}

/// Like [`read`], and also how sure each cell is (format version 3, spec
/// 5.4b): `soft` gets one value per cell, in the same order as the bits, from
/// -127 (surely black, 0) to 127 (surely white, 1); 0 is mid-grey. The
/// centre quarter of each cell is averaged, as in [`read`].
pub(crate) fn read_soft(gray: &[u8], layout: Layout, bits: &mut [u8], soft: &mut [i8]) {
    debug_assert_eq!(gray.len(), layout.gray_len());
    debug_assert_eq!(bits.len(), layout.capacity());
    debug_assert_eq!(soft.len(), layout.capacity() * 8);
    let cell = layout.cell;
    let half = cell / 2;
    let inset = cell / 4;
    let per_row = layout.width / cell;
    let count = (half * half) as i32;
    let threshold = 255 * count;
    for (row, (out_row, soft_row)) in bits
        .chunks_exact_mut(per_row / 8)
        .zip(soft.chunks_exact_mut(per_row))
        .enumerate()
    {
        let top = row * cell + inset;
        for (column, value) in soft_row.iter_mut().enumerate() {
            let left = column * cell + inset;
            let mut sum = 0i32;
            for dy in 0..half {
                let start = (top + dy) * layout.width + left;
                for &pixel in &gray[start..start + half] {
                    sum += i32::from(pixel);
                }
            }
            // 2 * sum - 255 * count runs from -255 * count to 255 * count.
            let lean = 2 * sum - threshold;
            *value = (lean * 127 / threshold) as i8;
            let byte = &mut out_row[column / 8];
            if column % 8 == 0 {
                *byte = 0;
            }
            if lean > 0 {
                *byte |= 0x80 >> (column % 8);
            }
        }
    }
}

/// Collect the top bit of eight pixels into one byte, first pixel in the top bit.
#[inline]
fn pack_top_bits(pixels: [u8; 8]) -> u8 {
    // Reading big-endian puts pixel 0 in the highest byte. The multiplication
    // moves the top bit of every byte into the highest byte of the product;
    // the eight partial products land on different bits, so nothing carries.
    let tops = u64::from_be_bytes(pixels) & 0x8080_8080_8080_8080;
    (tops.wrapping_mul(0x0002_0408_1020_4081) >> 56) as u8
}

/// A small grey picture of a `monob` frame, taking every n-th pixel.
pub(crate) fn preview_from_mono(mono: &[u8], layout: Layout) -> Preview {
    let stride = layout.width / 8;
    make_preview(layout, |x, y| {
        if mono[y * stride + x / 8] & (0x80 >> (x % 8)) != 0 {
            255
        } else {
            0
        }
    })
}

/// A small grey picture of a `gray` frame, taking every n-th pixel.
pub(crate) fn preview_from_gray(gray: &[u8], layout: Layout) -> Preview {
    make_preview(layout, |x, y| gray[y * layout.width + x])
}

fn make_preview(layout: Layout, pixel: impl Fn(usize, usize) -> u8) -> Preview {
    let step = (layout.width / PREVIEW_WIDTH).max(1);
    let width = layout.width / step;
    let height = layout.height / step;
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        let source_y = y * step + step / 2;
        for x in 0..width {
            pixels.push(pixel(x * step + step / 2, source_y));
        }
    }
    Preview {
        width: width as u32,
        height: height as u32,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{mono_to_gray, noise};
    use crate::types::Resolution;

    fn layouts() -> Vec<Layout> {
        let mut all = Vec::new();
        for resolution in [Resolution::P1080, Resolution::P2160] {
            for cell in [1, 4, 8] {
                all.push(Layout::with_cell(resolution, cell));
            }
        }
        all
    }

    #[test]
    fn pack_top_bits_matches_the_slow_way() {
        let data = noise(8 * 4096, 7);
        for group in data.as_chunks::<8>().0 {
            let mut slow = 0u8;
            for (k, value) in group.iter().enumerate() {
                if *value >= 128 {
                    slow |= 0x80 >> k;
                }
            }
            assert_eq!(pack_top_bits(*group), slow);
        }
        assert_eq!(pack_top_bits([128, 127, 255, 0, 0, 0, 0, 200]), 0b1010_0001);
    }

    #[test]
    fn draw_then_read_round_trips_for_every_layout() {
        for layout in layouts() {
            let bits = noise(layout.capacity(), layout.width as u64 + layout.cell as u64);
            let mut mono = vec![0u8; layout.mono_len()];
            draw(&bits, layout, &mut mono);
            let gray = mono_to_gray(&mono);
            let mut back = vec![0u8; layout.capacity()];
            read(&gray, layout, &mut back);
            assert_eq!(back, bits, "{layout:?}");
        }
    }

    #[test]
    fn soft_reading_agrees_with_hard_reading_and_measures_grey() {
        for layout in layouts().into_iter().filter(|layout| layout.cell > 1) {
            let bits = noise(layout.capacity(), 40 + layout.cell as u64);
            let mut mono = vec![0u8; layout.mono_len()];
            draw(&bits, layout, &mut mono);
            let mut gray = mono_to_gray(&mono);
            // Turn the first cell mid-grey, the second a light grey.
            for (cell, value) in [(0usize, 128u8), (1, 191)] {
                for y in 0..layout.cell {
                    for x in 0..layout.cell {
                        gray[y * layout.width + cell * layout.cell + x] = value;
                    }
                }
            }
            let (mut hard, mut both) = (vec![0u8; layout.capacity()], vec![0u8; layout.capacity()]);
            let mut soft = vec![0i8; layout.capacity() * 8];
            read(&gray, layout, &mut hard);
            read_soft(&gray, layout, &mut both, &mut soft);
            assert_eq!(both, hard, "{layout:?}");
            assert_eq!(soft[0], 0, "mid-grey knows nothing");
            assert_eq!(soft[1], 63, "light grey leans to white");
            for (i, &value) in soft.iter().enumerate().skip(2).take(5000) {
                let bit = bits[i / 8] & (0x80 >> (i % 8)) != 0;
                assert_eq!(value, if bit { 127 } else { -127 }, "{layout:?} cell {i}");
            }
        }
    }

    #[test]
    fn one_pixel_per_bit_is_monob_unchanged() {
        let layout = Layout::with_cell(Resolution::P1080, 1);
        let bits = noise(layout.capacity(), 3);
        let mut mono = vec![0u8; layout.mono_len()];
        draw(&bits, layout, &mut mono);
        assert_eq!(mono, bits);
    }

    #[test]
    fn cells_are_solid_squares_in_the_right_place() {
        for cell in [4usize, 8] {
            let layout = Layout::with_cell(Resolution::P1080, cell);
            let mut bits = vec![0u8; layout.capacity()];
            // Second cell of the first row, and first cell of the second row.
            bits[0] = 0b0100_0000;
            let data_per_row = layout.width / cell / 8;
            bits[data_per_row] = 0b1000_0000;
            let mut mono = vec![0u8; layout.mono_len()];
            draw(&bits, layout, &mut mono);
            let gray = mono_to_gray(&mono);
            for y in 0..3 * cell {
                for x in 0..3 * cell {
                    let white = (y < cell && (cell..2 * cell).contains(&x))
                        || ((cell..2 * cell).contains(&y) && x < cell);
                    let expected = if white { 255 } else { 0 };
                    assert_eq!(
                        gray[y * layout.width + x],
                        expected,
                        "cell {cell} at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_generic_cell_path_agrees_with_the_fast_ones() {
        for cell in [4usize, 8] {
            let data = noise(30, cell as u64);
            let mut fast = vec![0u8; data.len() * cell];
            expand_row(&data, cell, &mut fast);
            // Drive the generic branch by hand for the same cell size.
            let mut slow = vec![0u8; data.len() * cell];
            for (i, byte) in data.iter().enumerate() {
                for k in 0..8 {
                    if byte & (0x80 >> k) != 0 {
                        let start = (i * 8 + k) * cell;
                        for x in start..start + cell {
                            slow[x / 8] |= 0x80 >> (x % 8);
                        }
                    }
                }
            }
            assert_eq!(fast, slow);
        }
        // And the generic branch itself, with a size the format does not use.
        let mut two = vec![0u8; 2];
        expand_row(&[0b1010_0000], 2, &mut two);
        assert_eq!(two, [0b1100_1100, 0]);
    }

    #[test]
    fn reading_ignores_damaged_cell_edges() {
        for cell in [4usize, 8] {
            let layout = Layout::with_cell(Resolution::P1080, cell);
            let bits = noise(layout.capacity(), 11);
            let mut mono = vec![0u8; layout.mono_len()];
            draw(&bits, layout, &mut mono);
            let mut gray = mono_to_gray(&mono);
            // Wreck the outer ring of every cell and dim the centres a little.
            let inset = cell / 4;
            for y in 0..layout.height {
                for x in 0..layout.width {
                    let (cx, cy) = (x % cell, y % cell);
                    let edge = cx < inset || cx >= cell - inset || cy < inset || cy >= cell - inset;
                    let value = &mut gray[y * layout.width + x];
                    *value = if edge {
                        128
                    } else if *value == 255 {
                        180
                    } else {
                        70
                    };
                }
            }
            let mut back = vec![0u8; layout.capacity()];
            read(&gray, layout, &mut back);
            assert_eq!(back, bits, "cell {cell}");
        }
    }

    #[test]
    fn previews_are_small_and_agree_between_formats() {
        for layout in layouts() {
            let bits = noise(layout.capacity(), 5);
            let mut mono = vec![0u8; layout.mono_len()];
            draw(&bits, layout, &mut mono);
            let from_mono = preview_from_mono(&mono, layout);
            let from_gray = preview_from_gray(&mono_to_gray(&mono), layout);
            assert_eq!(from_mono, from_gray);
            assert_eq!((from_mono.width, from_mono.height), (480, 270));
            assert_eq!(from_mono.pixels.len(), 480 * 270);
        }
    }
}
