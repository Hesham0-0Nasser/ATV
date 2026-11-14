//! Helpers shared by the end-to-end tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use atv_core::{
    AtvError, CancelToken, DecodeOptions, DecodeReport, EncodeOptions, EncodeReport, FfmpegTools,
    Mode, Protection, Resolution,
};

/// Payload bytes in one 1080p Lossless frame.
pub const FRAME_1080: usize = 259_184;

pub const BOTH: [Resolution; 2] = [Resolution::P1080, Resolution::P2160];

/// ffmpeg and ffprobe, or `None` (with a message) when they are not installed.
pub fn tools() -> Option<FfmpegTools> {
    match atv_core::find_ffmpeg(None) {
        Ok(tools) => Some(tools),
        Err(error) => {
            eprintln!("SKIPPED: this test needs ffmpeg and ffprobe installed ({error})");
            None
        }
    }
}

/// Deterministic bytes that do not compress.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
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

/// Plain Lossless options: no compression, no encryption, no verify.
pub fn encode_options(
    tools: &FfmpegTools,
    inputs: &[PathBuf],
    output: &Path,
    resolution: Resolution,
) -> EncodeOptions {
    EncodeOptions {
        inputs: inputs.to_vec(),
        output: output.to_path_buf(),
        resolution,
        mode: Mode::Lossless,
        compress: false,
        smallest: false,
        protection: None,
        verify: false,
        keep_unverified: false,
        frame_rate: None,
        video_codec: atv_core::VideoCodec::H264,
        tools: tools.clone(),
    }
}

pub fn decode_options(
    tools: &FfmpegTools,
    video: &Path,
    output_dir: &Path,
    protection: Option<&Protection>,
) -> DecodeOptions {
    DecodeOptions {
        video: video.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        protection: protection.cloned(),
        tools: tools.clone(),
    }
}

pub fn run_encode(options: &EncodeOptions) -> Result<EncodeReport, AtvError> {
    atv_core::encode(options, &mut |_| {}, &CancelToken::new())
}

pub fn encode(
    tools: &FfmpegTools,
    inputs: &[PathBuf],
    output: &Path,
    resolution: Resolution,
) -> Result<EncodeReport, AtvError> {
    run_encode(&encode_options(tools, inputs, output, resolution))
}

pub fn decode_with(
    tools: &FfmpegTools,
    video: &Path,
    output_dir: &Path,
    protection: Option<&Protection>,
) -> Result<DecodeReport, AtvError> {
    let options = decode_options(tools, video, output_dir, protection);
    atv_core::decode(&options, &mut |_| {}, &CancelToken::new())
}

pub fn decode(
    tools: &FfmpegTools,
    video: &Path,
    output_dir: &Path,
) -> Result<DecodeReport, AtvError> {
    decode_with(tools, video, output_dir, None)
}

/// Every file under `root` with its contents, and every folder, keyed by relative path.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, into: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for item in fs::read_dir(dir).unwrap() {
            let path = item.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                into.insert(relative, None);
                walk(root, &path, into);
            } else {
                into.insert(relative, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut all = BTreeMap::new();
    walk(root, root, &mut all);
    all
}

/// The top-level items of a folder, sorted, ready to be used as encode inputs.
pub fn items_in(root: &Path) -> Vec<PathBuf> {
    let mut items: Vec<PathBuf> = fs::read_dir(root)
        .unwrap()
        .map(|item| item.unwrap().path())
        .collect();
    items.sort();
    items
}

/// A small mixed set of inputs: a folder with nested files, and a loose file.
pub fn sample_tree(root: &Path) {
    let folder = root.join("album");
    fs::create_dir_all(folder.join("raw")).unwrap();
    fs::write(folder.join("one.bin"), noise(2 * FRAME_1080 + 999, 21)).unwrap();
    fs::write(folder.join("raw/two.txt"), "some text\n".repeat(3000)).unwrap();
    fs::write(root.join("notes.txt"), b"remember the milk").unwrap();
}
