//! Format version 3's repair code (spec 5.4b): LDPC.
//!
//! Reed-Solomon (versions 1 and 2) sees each block as 0 or 1 and repairs
//! whole bytes. An LDPC code repairs single bits and is told how sure each
//! block is: a block that came back mid-grey counts as unsure. On the real
//! WhatsApp and YouTube copies it needs about half the extra data for the
//! same safety (spec 8.8).
//!
//! Each code is quasi-cyclic and regular: a 3 x L table of shifts, each
//! standing for a Z x Z identity matrix rotated by that many places, so every
//! bit is in exactly 3 checks. Codewords are n = L * Z = 8,100 bits. The
//! tables were found by a search for codes with no 4-cycles and few
//! 6-cycles, and are part of the format: they must never change. (A first
//! design used Wi-Fi's staircase shape, which encodes in one pass, but its
//! bits in only 2 checks made it fail 6 codewords in 5 million with a lost
//! frame on harsh WhatsApp damage, against none for this one; spec 8.8.)
//!
//! Encoding is systematic: the check matrix is brought to reduced row
//! echelon form once (choosing pivot columns from the last column down);
//! pivot columns hold parity bits, every other column holds a data bit, in
//! order. Each parity bit is then the parity of its row's data bits.
//!
//! Decoding is layered normalised min-sum. Its input is one confidence value
//! per bit, positive for 1 and negative for 0, 0 for "no idea" (a lost
//! frame); its output is the data, or nothing when the codeword cannot be
//! repaired.

use std::sync::OnceLock;

/// Codeword length in bits, for both codes. Divisible by 15, so each frame of
/// a group carries the same share of every codeword.
pub(crate) const CODE_BITS: usize = 8100;

/// The strong code (17.6% extra; WhatsApp): L = 20, Z = 405.
const STRONG_Z: usize = 405;
const STRONG: [[u16; 20]; 3] = [
    [
        126, 195, 159, 41, 29, 200, 358, 387, 352, 324, 386, 270, 370, 346, 95, 325, 182, 101, 330,
        209,
    ],
    [
        197, 327, 259, 138, 196, 263, 160, 378, 188, 91, 12, 322, 145, 331, 134, 318, 232, 219,
        181, 388,
    ],
    [
        77, 55, 309, 182, 382, 268, 242, 284, 354, 58, 200, 303, 212, 104, 169, 109, 283, 291, 240,
        174,
    ],
];

/// The standard code (11.1% extra; YouTube): L = 30, Z = 270.
const STANDARD_Z: usize = 270;
const STANDARD: [[u16; 30]; 3] = [
    [
        102, 221, 168, 8, 66, 247, 269, 186, 5, 218, 20, 68, 21, 196, 212, 160, 72, 173, 242, 11,
        25, 226, 6, 164, 73, 99, 188, 233, 27, 216,
    ],
    [
        145, 108, 250, 35, 29, 38, 37, 62, 199, 86, 202, 259, 63, 157, 265, 71, 197, 266, 196, 119,
        132, 75, 31, 4, 234, 243, 209, 22, 206, 264,
    ],
    [
        214, 64, 25, 180, 27, 95, 133, 47, 215, 262, 93, 129, 140, 258, 100, 213, 96, 78, 86, 197,
        154, 197, 167, 50, 103, 169, 267, 168, 66, 211,
    ],
];

/// Decoder settings (spec 8.8): scale on check messages, and the most passes.
const ALPHA: f32 = 0.8;
const MAX_ITERATIONS: usize = 50;
/// Confidence given to data bits known to be 0 (the unused end of the data).
const KNOWN: f32 = 1.0e4;

/// One of the two LDPC codes.
pub(crate) struct Ldpc {
    /// For each check, the bits it covers (`check_bits[check_start[c]..]`).
    check_start: Vec<u32>,
    check_bits: Vec<u32>,
    /// Codeword positions of the data bits, in order. The first
    /// `8 * data_bytes` carry data; the rest are always 0.
    data_positions: Vec<u32>,
    /// Codeword position of each parity bit, and its row of the reduced
    /// matrix over the data bits (`words` 64-bit words per row).
    parity_positions: Vec<u32>,
    parity_rows: Vec<u64>,
    words: usize,
}

impl Ldpc {
    /// The code for a Robust repair strength: 64 = strong, 32 = standard.
    /// Built once per run of the program.
    pub(crate) fn for_parity(parity: u8) -> &'static Ldpc {
        static STRONG_CODE: OnceLock<Ldpc> = OnceLock::new();
        static STANDARD_CODE: OnceLock<Ldpc> = OnceLock::new();
        if parity >= 64 {
            STRONG_CODE.get_or_init(|| Ldpc::new(STRONG_Z, &STRONG))
        } else {
            STANDARD_CODE.get_or_init(|| Ldpc::new(STANDARD_Z, &STANDARD))
        }
    }

    fn new<const L: usize>(z: usize, table: &[[u16; L]; 3]) -> Self {
        let checks = 3 * z;
        let shifts: Vec<[u16; 3]> = (0..L)
            .map(|column| [table[0][column], table[1][column], table[2][column]])
            .collect();
        let mut rows: Vec<Vec<u32>> = vec![Vec::new(); checks];
        for (column, column_shifts) in shifts.iter().enumerate() {
            for (j, &shift) in column_shifts.iter().enumerate() {
                for t in 0..z {
                    rows[j * z + (t + usize::from(shift)) % z].push((column * z + t) as u32);
                }
            }
        }

        // Reduced row echelon form over GF(2), pivots from the last column down.
        let words_n = CODE_BITS.div_ceil(64);
        let mut dense: Vec<Vec<u64>> = rows
            .iter()
            .map(|bits| {
                let mut row = vec![0u64; words_n];
                for &bit in bits {
                    row[bit as usize / 64] ^= 1 << (bit % 64);
                }
                row
            })
            .collect();
        let mut pivots: Vec<usize> = Vec::new();
        let mut rank = 0;
        for column in (0..CODE_BITS).rev() {
            if rank == checks {
                break;
            }
            let (word, mask) = (column / 64, 1u64 << (column % 64));
            let Some(found) = (rank..checks).find(|&r| dense[r][word] & mask != 0) else {
                continue;
            };
            dense.swap(rank, found);
            let pivot_row = dense[rank].clone();
            for (r, row) in dense.iter_mut().enumerate() {
                if r != rank && row[word] & mask != 0 {
                    for (a, b) in row.iter_mut().zip(&pivot_row) {
                        *a ^= b;
                    }
                }
            }
            pivots.push(column);
            rank += 1;
        }
        let mut is_pivot = vec![false; CODE_BITS];
        for &column in &pivots {
            is_pivot[column] = true;
        }
        let data_positions: Vec<u32> = (0..CODE_BITS as u32)
            .filter(|&column| !is_pivot[column as usize])
            .collect();
        let k = data_positions.len();
        let words = k.div_ceil(64);
        let mut parity_rows = vec![0u64; rank * words];
        for (r, row) in dense.iter().take(rank).enumerate() {
            for (i, &column) in data_positions.iter().enumerate() {
                let column = column as usize;
                if row[column / 64] & (1 << (column % 64)) != 0 {
                    parity_rows[r * words + i / 64] |= 1 << (i % 64);
                }
            }
        }

        let mut check_start = Vec::with_capacity(checks + 1);
        let mut check_bits = Vec::new();
        check_start.push(0);
        for row in rows {
            check_bits.extend(row);
            check_start.push(check_bits.len() as u32);
        }
        Ldpc {
            check_start,
            check_bits,
            data_positions,
            parity_positions: pivots.iter().map(|&column| column as u32).collect(),
            parity_rows,
            words,
        }
    }

    /// Data bytes one codeword carries: the data bits rounded down to whole bytes.
    pub(crate) fn data_bytes(&self) -> usize {
        self.data_positions.len() / 8
    }

    /// Encode `data` (exactly [`Ldpc::data_bytes`]) into `bits`, one 0 or 1
    /// per entry, [`CODE_BITS`] entries. Data bit `i` (first byte's top bit
    /// first) goes to the `i`-th data position.
    pub(crate) fn encode(&self, data: &[u8], bits: &mut [u8]) {
        debug_assert_eq!(data.len(), self.data_bytes());
        debug_assert_eq!(bits.len(), CODE_BITS);
        bits.fill(0);
        let mut packed = vec![0u64; self.words];
        for i in 0..8 * data.len() {
            if (data[i / 8] >> (7 - i % 8)) & 1 == 1 {
                packed[i / 64] |= 1 << (i % 64);
                bits[self.data_positions[i] as usize] = 1;
            }
        }
        for (row, &position) in self
            .parity_rows
            .chunks_exact(self.words)
            .zip(&self.parity_positions)
        {
            let ones: u32 = row
                .iter()
                .zip(&packed)
                .map(|(a, b)| (a & b).count_ones())
                .sum();
            bits[position as usize] = (ones & 1) as u8;
        }
    }

    /// Decode one codeword. `confidence` has [`CODE_BITS`] entries: above 0
    /// leans to 1, below 0 to 0, 0 knows nothing. Writes the data to `data`
    /// and returns how many of its bytes differed from what was read, or
    /// `None` when the codeword cannot be repaired.
    pub(crate) fn decode(
        &self,
        confidence: &[f32],
        data: &mut [u8],
        work: &mut LdpcWork,
    ) -> Option<u64> {
        debug_assert_eq!(confidence.len(), CODE_BITS);
        debug_assert_eq!(data.len(), self.data_bytes());
        let data_end = 8 * self.data_bytes();
        // The decoder's convention is the usual one: positive means 0.
        let total = &mut work.total;
        total.clear();
        total.extend(confidence.iter().map(|&c| -c));
        for &position in &self.data_positions[data_end..] {
            total[position as usize] = KNOWN;
        }
        let read = |total: &[f32]| -> Vec<u8> {
            self.data_positions[..data_end]
                .chunks_exact(8)
                .map(|eight| {
                    eight.iter().fold(0u8, |byte, &position| {
                        (byte << 1) | u8::from(total[position as usize] <= 0.0)
                    })
                })
                .collect()
        };
        let before = read(total);
        if !self.satisfied(total) {
            let edges = self.check_bits.len();
            work.messages.clear();
            work.messages.resize(edges, 0.0);
            let mut solved = false;
            for _ in 0..MAX_ITERATIONS {
                self.pass(total, &mut work.messages, &mut work.inputs);
                if self.satisfied(total) {
                    solved = true;
                    break;
                }
            }
            if !solved {
                return None;
            }
        }
        let after = read(total);
        data.copy_from_slice(&after);
        Some(before.iter().zip(&after).filter(|(a, b)| a != b).count() as u64)
    }

    /// One layered min-sum pass over every check.
    fn pass(&self, total: &mut [f32], messages: &mut [f32], inputs: &mut Vec<f32>) {
        for check in 0..self.check_start.len() - 1 {
            let (a, b) = (
                self.check_start[check] as usize,
                self.check_start[check + 1] as usize,
            );
            let bits = &self.check_bits[a..b];
            let messages = &mut messages[a..b];
            inputs.clear();
            let (mut min1, mut min2, mut at, mut negative) =
                (f32::INFINITY, f32::INFINITY, 0, false);
            for (i, (&bit, &message)) in bits.iter().zip(messages.iter()).enumerate() {
                let x = total[bit as usize] - message;
                inputs.push(x);
                negative ^= x < 0.0;
                let size = x.abs();
                if size < min1 {
                    min2 = min1;
                    min1 = size;
                    at = i;
                } else if size < min2 {
                    min2 = size;
                }
            }
            for (i, (&bit, message)) in bits.iter().zip(messages.iter_mut()).enumerate() {
                let size = ALPHA * if i == at { min2 } else { min1 };
                let out = if negative ^ (inputs[i] < 0.0) {
                    -size
                } else {
                    size
                };
                *message = out;
                total[bit as usize] = inputs[i] + out;
            }
        }
    }

    /// Whether the hard decisions (bit 1 when the value is 0 or below)
    /// satisfy every check.
    fn satisfied(&self, total: &[f32]) -> bool {
        self.check_start.windows(2).all(|range| {
            let bits = &self.check_bits[range[0] as usize..range[1] as usize];
            !bits
                .iter()
                .fold(false, |odd, &bit| odd ^ (total[bit as usize] <= 0.0))
        })
    }
}

/// Scratch space for [`Ldpc::decode`], reused between codewords.
#[derive(Default)]
pub(crate) struct LdpcWork {
    total: Vec<f32>,
    messages: Vec<f32>,
    inputs: Vec<f32>,
}

impl LdpcWork {
    /// After a decode, whether the decoder ended up taking bit `b` as 1.
    pub(crate) fn leans_to_one(&self, b: usize) -> bool {
        self.total.get(b).is_some_and(|&value| value <= 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_data(code: &Ldpc, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..code.data_bytes())
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    fn codes() -> [(&'static Ldpc, &'static str); 2] {
        [
            (Ldpc::for_parity(64), "strong"),
            (Ldpc::for_parity(32), "standard"),
        ]
    }

    #[test]
    fn sizes_match_the_spec() {
        let strong = Ldpc::for_parity(64);
        let standard = Ldpc::for_parity(32);
        assert_eq!(STRONG[0].len() * STRONG_Z, CODE_BITS);
        assert_eq!(STANDARD[0].len() * STANDARD_Z, CODE_BITS);
        // Each block row of a regular code adds to the all-ones word, so
        // two of the 3Z checks repeat the others.
        assert_eq!(strong.parity_positions.len(), 3 * STRONG_Z - 2);
        assert_eq!(standard.parity_positions.len(), 3 * STANDARD_Z - 2);
        assert_eq!(strong.data_positions.len(), 6887);
        assert_eq!(standard.data_positions.len(), 7292);
        assert_eq!(strong.data_bytes(), 860);
        assert_eq!(standard.data_bytes(), 911);
    }

    #[test]
    fn the_tables_have_no_4_cycles() {
        // Two columns sharing two checks make a 4-cycle, the worst short
        // loop for the decoder.
        let columns = |table: &[&[u16]], z: usize| -> (Vec<[u16; 3]>, i64) {
            let shifts = (0..table[0].len())
                .map(|c| [table[0][c], table[1][c], table[2][c]])
                .collect();
            (shifts, z as i64)
        };
        for ((shifts, z), name) in [
            (
                columns(&[&STRONG[0], &STRONG[1], &STRONG[2]], STRONG_Z),
                "strong",
            ),
            (
                columns(&[&STANDARD[0], &STANDARD[1], &STANDARD[2]], STANDARD_Z),
                "standard",
            ),
        ] {
            for (a, sa) in shifts.iter().enumerate() {
                for (b, sb) in shifts.iter().enumerate().skip(a + 1) {
                    for r1 in 0..3 {
                        for r2 in r1 + 1..3 {
                            let loop_sum =
                                i64::from(sa[r1]) - i64::from(sa[r2]) - i64::from(sb[r1])
                                    + i64::from(sb[r2]);
                            assert_ne!(loop_sum.rem_euclid(z), 0, "{name}: columns {a}, {b}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn encoded_codewords_satisfy_every_check() {
        for (code, name) in codes() {
            let mut bits = vec![0u8; CODE_BITS];
            for seed in 0..20 {
                code.encode(&random_data(code, seed), &mut bits);
                let values: Vec<f32> = bits
                    .iter()
                    .map(|&b| if b == 1 { -1.0 } else { 1.0 })
                    .collect();
                assert!(code.satisfied(&values), "{name}, seed {seed}");
            }
        }
    }

    #[test]
    fn a_clean_codeword_decodes_with_nothing_repaired() {
        for (code, name) in codes() {
            let data = random_data(code, 1);
            let mut bits = vec![0u8; CODE_BITS];
            code.encode(&data, &mut bits);
            let confidence: Vec<f32> = bits
                .iter()
                .map(|&b| if b == 1 { 100.0 } else { -100.0 })
                .collect();
            let mut out = vec![0u8; code.data_bytes()];
            let repaired = code.decode(&confidence, &mut out, &mut LdpcWork::default());
            assert_eq!(repaired, Some(0), "{name}");
            assert_eq!(out, data, "{name}");
        }
    }

    /// Encode random data, damage it, decode it.
    fn survives(code: &Ldpc, lost_frame: bool, wrong: usize, unsure: usize) -> Option<u64> {
        let data = random_data(code, 2);
        let mut bits = vec![0u8; CODE_BITS];
        code.encode(&data, &mut bits);
        let mut confidence: Vec<f32> = bits
            .iter()
            .map(|&b| if b == 1 { 100.0 } else { -100.0 })
            .collect();
        // Confidently wrong bits, then unsure ones (right, but nearly grey).
        for i in 0..wrong {
            confidence[7 + i * 263 % (CODE_BITS - 7)] *= -1.0;
        }
        for i in 0..unsure {
            confidence[3 + i * 131 % (CODE_BITS - 3)] *= 0.05;
        }
        // A lost frame: every 15th bit knows nothing.
        if lost_frame {
            for value in confidence.iter_mut().step_by(15) {
                *value = 0.0;
            }
        }
        let mut out = vec![0u8; code.data_bytes()];
        let repaired = code.decode(&confidence, &mut out, &mut LdpcWork::default());
        if repaired.is_some() {
            assert_eq!(out, data);
        }
        repaired
    }

    #[test]
    fn wrong_unsure_and_lost_bits_are_repaired() {
        let strong = Ldpc::for_parity(64);
        let standard = Ldpc::for_parity(32);
        // What each strength is for (spec 8.8): strong takes a lost frame on
        // top of damage; standard takes damage, or a lost frame with little.
        assert!(survives(strong, true, 30, 0).is_some_and(|n| n > 0));
        assert!(survives(strong, false, 40, 200).is_some_and(|n| n > 0));
        assert!(survives(standard, false, 30, 200).is_some_and(|n| n > 0));
        assert!(survives(standard, true, 5, 0).is_some());
    }

    #[test]
    fn hopeless_damage_is_refused_not_guessed() {
        for (code, name) in codes() {
            let data = random_data(code, 3);
            let mut bits = vec![0u8; CODE_BITS];
            code.encode(&data, &mut bits);
            // A fifth of all bits flipped: far past any repair.
            let confidence: Vec<f32> = bits
                .iter()
                .enumerate()
                .map(|(i, &b)| {
                    let sign = if b == 1 { 1.0 } else { -1.0 };
                    if i % 5 == 0 { -sign } else { sign }
                })
                .collect();
            let mut out = vec![0u8; code.data_bytes()];
            assert_eq!(
                code.decode(&confidence, &mut out, &mut LdpcWork::default()),
                None,
                "{name}"
            );
        }
    }
}
