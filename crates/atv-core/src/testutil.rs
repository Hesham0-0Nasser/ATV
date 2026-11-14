//! Helpers shared by the unit tests.

/// Deterministic pseudo-random bytes, so tests need no extra crate.
pub(crate) fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect()
}

/// What ffmpeg does when it turns `monob` into `gray`: a 1 bit becomes 255, a 0 bit becomes 0.
pub(crate) fn mono_to_gray(mono: &[u8]) -> Vec<u8> {
    mono.iter()
        .flat_map(|byte| (0..8).map(move |k| if byte & (0x80 >> k) != 0 { 255 } else { 0 }))
        .collect()
}
