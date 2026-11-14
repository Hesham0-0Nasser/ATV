//! The 128-byte header record and the header frame that carries it (spec 5.3).
//!
//! The header is not secret. It says how to read the video, never what is in it.

use super::{Layout, grid};
use crate::error::AtvError;
use crate::types::{Cell, Mode, Resolution};

/// Size of one header record.
pub(crate) const HEADER_LEN: usize = 128;
/// The part of the record covered by its CRC. The same bytes are the
/// associated data that binds the header to encrypted content.
pub(crate) const HEADER_BODY_LEN: usize = 124;
/// Format version 1: Robust codewords woven inside each frame. Still written
/// for Lossless videos (which have no codewords) and always read.
pub(crate) const VERSION_1: u8 = 1;
/// Format version 2 (owner's decision, 13 Nov 2026): Robust codewords placed
/// diagonally and woven across groups of 15 frames, each frame carrying its
/// own protected tag (spec 5.4).
pub(crate) const VERSION_2: u8 = 2;
/// Format version 3 (owner's decision, 14 Nov 2026): Robust repair is an
/// LDPC code that uses how grey each cell came back, woven across groups of
/// 15 frames with version 2's frame tags (spec 5.4b). Written for every
/// Robust video; versions 1 and 2 are still read.
pub(crate) const VERSION_3: u8 = 3;

const MAGIC: [u8; 4] = *b"ATV1";

const FLAG_COMPRESSED: u8 = 1 << 0;
const FLAG_ENCRYPTED: u8 = 1 << 1;
const FLAG_PASSWORD: u8 = 1 << 2;
const FLAG_KEY_FILE: u8 = 1 << 3;

/// Everything the header record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    /// 1, 2 or 3; see [`VERSION_1`], [`VERSION_2`] and [`VERSION_3`].
    pub version: u8,
    pub mode: Mode,
    pub compressed: bool,
    pub encrypted: bool,
    pub password_used: bool,
    pub key_file_used: bool,
    /// Argon2 memory in KiB, iterations and parallelism. All zero without a password.
    pub argon_memory_kib: u32,
    pub argon_iterations: u32,
    pub argon_parallelism: u32,
    /// Random salt. Zero when not encrypted.
    pub salt: [u8; 16],
    /// Random stream nonce prefix. Zero when not encrypted.
    pub nonce_prefix: [u8; 19],
}

impl Header {
    /// A header for a video with no compression and no encryption.
    pub(crate) fn plain(mode: Mode) -> Self {
        Header {
            version: match mode {
                Mode::Lossless => VERSION_1,
                Mode::Robust { .. } => VERSION_3,
            },
            mode,
            compressed: false,
            encrypted: false,
            password_used: false,
            key_file_used: false,
            argon_memory_kib: 0,
            argon_iterations: 0,
            argon_parallelism: 0,
            salt: [0; 16],
            nonce_prefix: [0; 19],
        }
    }

    /// The record as it is stored, CRC included.
    pub(crate) fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut raw = [0u8; HEADER_LEN];
        raw[0..4].copy_from_slice(&MAGIC);
        raw[4] = self.version;
        let (mode, cell, parity) = match self.mode {
            Mode::Lossless => (0, 1, 0),
            Mode::Robust { cell, parity } => (1, cell.pixels() as u8, parity),
        };
        raw[5] = mode;
        raw[6] = cell;
        let mut flags = 0;
        if self.compressed {
            flags |= FLAG_COMPRESSED;
        }
        if self.encrypted {
            flags |= FLAG_ENCRYPTED;
        }
        if self.password_used {
            flags |= FLAG_PASSWORD;
        }
        if self.key_file_used {
            flags |= FLAG_KEY_FILE;
        }
        raw[7] = flags;
        raw[8] = parity;
        // 9..12 reserved
        raw[12..16].copy_from_slice(&self.argon_memory_kib.to_le_bytes());
        raw[16..20].copy_from_slice(&self.argon_iterations.to_le_bytes());
        raw[20..24].copy_from_slice(&self.argon_parallelism.to_le_bytes());
        raw[24..40].copy_from_slice(&self.salt);
        raw[40..59].copy_from_slice(&self.nonce_prefix);
        // 59..124 reserved
        let crc = crc32fast::hash(&raw[..HEADER_BODY_LEN]);
        raw[HEADER_BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
        raw
    }

    /// Read a stored record. Fails with `NotAtvVideo` when the magic, CRC or
    /// field values are wrong, and `UnsupportedVersion` for a newer format.
    pub(crate) fn from_bytes(raw: &[u8; HEADER_LEN]) -> Result<Self, AtvError> {
        if !looks_valid(raw) {
            return Err(AtvError::NotAtvVideo);
        }
        let version = raw[4];
        if !(VERSION_1..=VERSION_3).contains(&version) {
            return Err(AtvError::UnsupportedVersion { version });
        }
        let parity = raw[8];
        let mode = match (raw[5], raw[6]) {
            (0, 1) if parity == 0 => Mode::Lossless,
            (1, 4) if valid_parity(parity) => Mode::Robust {
                cell: Cell::C4,
                parity,
            },
            (1, 8) if valid_parity(parity) => Mode::Robust {
                cell: Cell::C8,
                parity,
            },
            _ => return Err(AtvError::NotAtvVideo),
        };
        // Versions 2 and 3 change only Robust repair.
        if version >= VERSION_2 && mode == Mode::Lossless {
            return Err(AtvError::NotAtvVideo);
        }
        let flags = raw[7];
        let encrypted = flags & FLAG_ENCRYPTED != 0;
        let password_used = flags & FLAG_PASSWORD != 0;
        let key_file_used = flags & FLAG_KEY_FILE != 0;
        // An encrypted video needs at least one secret, a plain one has none.
        if encrypted != (password_used || key_file_used) {
            return Err(AtvError::NotAtvVideo);
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&raw[24..40]);
        let mut nonce_prefix = [0u8; 19];
        nonce_prefix.copy_from_slice(&raw[40..59]);
        Ok(Header {
            version,
            mode,
            compressed: flags & FLAG_COMPRESSED != 0,
            encrypted,
            password_used,
            key_file_used,
            argon_memory_kib: le_u32(&raw[12..16]),
            argon_iterations: le_u32(&raw[16..20]),
            argon_parallelism: le_u32(&raw[20..24]),
            salt,
            nonce_prefix,
        })
    }
}

/// Parity must be even and leave room for data in a 255-byte codeword.
fn valid_parity(parity: u8) -> bool {
    parity >= 2 && parity.is_multiple_of(2) && parity <= 128
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut four = [0u8; 4];
    four.copy_from_slice(bytes);
    u32::from_le_bytes(four)
}

/// True when the record has the magic and its CRC matches.
fn looks_valid(raw: &[u8; HEADER_LEN]) -> bool {
    raw[0..4] == MAGIC
        && crc32fast::hash(&raw[..HEADER_BODY_LEN]) == le_u32(&raw[HEADER_BODY_LEN..])
}

/// Draw the header frame as `monob` pixels: the record repeated back to back
/// in 8 x 8 cells, with the leftover bytes zero.
pub(crate) fn draw_frame(raw: &[u8; HEADER_LEN], resolution: Resolution) -> Vec<u8> {
    let layout = Layout::for_header(resolution);
    let mut bits = vec![0u8; layout.capacity()];
    for copy in bits.chunks_exact_mut(HEADER_LEN) {
        copy.copy_from_slice(raw);
    }
    let mut mono = vec![0u8; layout.mono_len()];
    grid::draw(&bits, layout, &mut mono);
    mono
}

/// Look for a header in a `gray` frame. `scratch` is reused between calls.
///
/// Returns the header and its raw bytes from the first copy with the right
/// magic and a valid CRC. `NotAtvVideo` when no copy qualifies.
pub(crate) fn read_frame(
    gray: &[u8],
    resolution: Resolution,
    scratch: &mut Vec<u8>,
) -> Result<(Header, [u8; HEADER_LEN]), AtvError> {
    let layout = Layout::for_header(resolution);
    scratch.resize(layout.capacity(), 0);
    grid::read(gray, layout, scratch);
    find(scratch)
}

/// Scan thresholded frame bytes for the first good copy of the record.
///
/// If no single copy survived (a heavily re-compressed video can damage a
/// few cells in every one), each bit is decided by a vote across all the
/// copies, and the result is accepted only if its CRC is right.
pub(crate) fn find(bits: &[u8]) -> Result<(Header, [u8; HEADER_LEN]), AtvError> {
    let (copies, _) = bits.as_chunks::<HEADER_LEN>();
    for raw in copies {
        if looks_valid(raw) {
            return Header::from_bytes(raw).map(|header| (header, *raw));
        }
    }
    if copies.len() >= 3 {
        let voted = majority(copies);
        if looks_valid(&voted) {
            return Header::from_bytes(&voted).map(|header| (header, voted));
        }
    }
    Err(AtvError::NotAtvVideo)
}

/// For every bit, the value most of the copies agree on.
fn majority(copies: &[[u8; HEADER_LEN]]) -> [u8; HEADER_LEN] {
    let mut voted = [0u8; HEADER_LEN];
    for (position, out) in voted.iter_mut().enumerate() {
        for bit in 0..8 {
            let mask = 0x80u8 >> bit;
            let ones = copies
                .iter()
                .filter(|copy| copy[position] & mask != 0)
                .count();
            if 2 * ones > copies.len() {
                *out |= mask;
            }
        }
    }
    voted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::mono_to_gray;

    fn sample() -> Header {
        Header {
            version: VERSION_3,
            mode: Mode::Robust {
                cell: Cell::C4,
                parity: 32,
            },
            compressed: true,
            encrypted: true,
            password_used: true,
            key_file_used: false,
            argon_memory_kib: 65_536,
            argon_iterations: 3,
            argon_parallelism: 4,
            salt: [7; 16],
            nonce_prefix: [9; 19],
        }
    }

    /// Recompute the CRC after a test has edited a record on purpose.
    fn reseal(raw: &mut [u8; HEADER_LEN]) {
        let crc = crc32fast::hash(&raw[..HEADER_BODY_LEN]);
        raw[HEADER_BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn record_round_trips() {
        for header in [sample(), Header::plain(Mode::Lossless)] {
            let raw = header.to_bytes();
            assert_eq!(Header::from_bytes(&raw).unwrap(), header);
        }
    }

    #[test]
    fn record_layout_is_the_documented_one() {
        let raw = sample().to_bytes();
        assert_eq!(&raw[0..4], b"ATV1");
        assert_eq!(raw[4], 3, "version");
        assert_eq!(raw[5], 1, "mode: robust");
        assert_eq!(raw[6], 4, "cell size");
        assert_eq!(raw[7], 0b0111, "flags: compressed, encrypted, password");
        assert_eq!(raw[8], 32, "parity");
        assert_eq!(&raw[9..12], &[0; 3]);
        assert_eq!(&raw[12..16], &65_536u32.to_le_bytes());
        assert_eq!(&raw[16..20], &3u32.to_le_bytes());
        assert_eq!(&raw[20..24], &4u32.to_le_bytes());
        assert_eq!(&raw[24..40], &[7; 16]);
        assert_eq!(&raw[40..59], &[9; 19]);
        assert_eq!(&raw[59..124], &[0; 65]);
        assert_eq!(&raw[124..128], &crc32fast::hash(&raw[..124]).to_le_bytes());
    }

    #[test]
    fn a_flipped_bit_fails_the_crc() {
        let mut raw = sample().to_bytes();
        raw[30] ^= 0x01;
        assert!(matches!(
            Header::from_bytes(&raw),
            Err(AtvError::NotAtvVideo)
        ));
    }

    #[test]
    fn wrong_magic_is_rejected() {
        let mut raw = sample().to_bytes();
        raw[0..4].copy_from_slice(b"ATV2");
        reseal(&mut raw);
        assert!(matches!(
            Header::from_bytes(&raw),
            Err(AtvError::NotAtvVideo)
        ));
    }

    #[test]
    fn a_newer_version_is_reported_as_such() {
        let mut raw = sample().to_bytes();
        raw[4] = 4;
        reseal(&mut raw);
        assert!(matches!(
            Header::from_bytes(&raw),
            Err(AtvError::UnsupportedVersion { version: 4 })
        ));
    }

    #[test]
    fn every_robust_version_is_read_and_new_videos_are_version_3() {
        for version in [VERSION_1, VERSION_2, VERSION_3] {
            let mut raw = sample().to_bytes();
            raw[4] = version;
            reseal(&mut raw);
            assert_eq!(
                Header::from_bytes(&raw).map(|h| h.version).ok(),
                Some(version)
            );
        }
        let robust = Mode::Robust {
            cell: Cell::C8,
            parity: 64,
        };
        assert_eq!(Header::plain(robust).version, VERSION_3);
        assert_eq!(Header::plain(Mode::Lossless).version, VERSION_1);
        // Lossless has no repair, so only version 1 makes sense for it.
        let mut raw = Header::plain(Mode::Lossless).to_bytes();
        raw[4] = VERSION_3;
        reseal(&mut raw);
        assert!(matches!(
            Header::from_bytes(&raw),
            Err(AtvError::NotAtvVideo)
        ));
    }

    #[test]
    fn impossible_field_values_are_rejected() {
        // Lossless with a cell size, Robust with odd parity, encrypted with no secret.
        let edits: [fn(&mut [u8; HEADER_LEN]); 3] = [
            |raw| {
                raw[5] = 0;
                raw[6] = 4;
            },
            |raw| raw[8] = 33,
            |raw| raw[7] = FLAG_ENCRYPTED,
        ];
        for edit in edits {
            let mut raw = sample().to_bytes();
            edit(&mut raw);
            reseal(&mut raw);
            assert!(matches!(
                Header::from_bytes(&raw),
                Err(AtvError::NotAtvVideo)
            ));
        }
    }

    #[test]
    fn header_frame_holds_the_documented_number_of_copies() {
        let raw = sample().to_bytes();
        for (resolution, copies) in [(Resolution::P1080, 31), (Resolution::P2160, 126)] {
            let layout = Layout::for_header(resolution);
            let mono = draw_frame(&raw, resolution);
            let mut bits = vec![0u8; layout.capacity()];
            grid::read(&mono_to_gray(&mono), layout, &mut bits);
            let (chunks, rest) = bits.as_chunks::<HEADER_LEN>();
            assert_eq!(chunks.len(), copies);
            assert!(chunks.iter().all(|chunk| chunk == &raw));
            assert!(rest.iter().all(|byte| *byte == 0));
        }
    }

    #[test]
    fn header_frame_round_trips_through_pixels() {
        let header = sample();
        let raw = header.to_bytes();
        let mut scratch = Vec::new();
        for resolution in [Resolution::P1080, Resolution::P2160] {
            let gray = mono_to_gray(&draw_frame(&raw, resolution));
            let (found, found_raw) = read_frame(&gray, resolution, &mut scratch).unwrap();
            assert_eq!(found, header);
            assert_eq!(found_raw, raw);
        }
    }

    #[test]
    fn a_later_copy_is_used_when_the_first_ones_are_damaged() {
        let header = sample();
        let raw = header.to_bytes();
        let mut bits = Vec::new();
        for _ in 0..31 {
            bits.extend_from_slice(&raw);
        }
        // Damage the first twenty copies in different places.
        for copy in 0..20 {
            bits[copy * HEADER_LEN + copy * 5] ^= 0xFF;
        }
        let (found, _) = find(&bits).unwrap();
        assert_eq!(found, header);
    }

    #[test]
    fn a_vote_recovers_the_header_when_every_copy_is_damaged() {
        let header = sample();
        let raw = header.to_bytes();
        let mut bits = Vec::new();
        for _ in 0..31 {
            bits.extend_from_slice(&raw);
        }
        // Every copy is damaged, each in a different place.
        for copy in 0..31 {
            bits[copy * HEADER_LEN + copy * 4] ^= 0x81;
            bits[copy * HEADER_LEN + 127 - copy] ^= 0x18;
        }
        let (found, found_raw) = find(&bits).unwrap();
        assert_eq!(found, header);
        assert_eq!(found_raw, raw);
    }

    #[test]
    fn a_frame_with_no_good_copy_is_not_an_atv_video() {
        let raw = sample().to_bytes();
        let mut bits = Vec::new();
        for _ in 0..31 {
            bits.extend_from_slice(&raw);
        }
        // The same bit wrong in every copy: a vote cannot help, the CRC catches it.
        for copy in 0..31 {
            bits[copy * HEADER_LEN + 60] ^= 0x10;
        }
        assert!(matches!(find(&bits), Err(AtvError::NotAtvVideo)));
        assert!(matches!(find(&[0u8; 4050]), Err(AtvError::NotAtvVideo)));
    }
}
