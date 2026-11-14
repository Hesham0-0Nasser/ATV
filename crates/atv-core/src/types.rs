//! Public option, report and event types.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::crypto::Protection;
use crate::ffmpeg::FfmpegTools;

/// Frames per second of every ATV video in format version 1.
pub const FPS: u32 = 30;

/// The two frame sizes ATV writes and reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// 1920 x 1080
    P1080,
    /// 3840 x 2160 (4K)
    P2160,
}

impl Resolution {
    pub fn width(self) -> u32 {
        match self {
            Resolution::P1080 => 1920,
            Resolution::P2160 => 3840,
        }
    }

    pub fn height(self) -> u32 {
        match self {
            Resolution::P1080 => 1080,
            Resolution::P2160 => 2160,
        }
    }

    /// The resolution with exactly this frame size, if it is one ATV supports.
    pub fn from_dimensions(width: u32, height: u32) -> Option<Self> {
        match (width, height) {
            (1920, 1080) => Some(Resolution::P1080),
            (3840, 2160) => Some(Resolution::P2160),
            _ => None,
        }
    }

    /// Short name for display: `1080p` or `4K`.
    pub fn label(self) -> &'static str {
        match self {
            Resolution::P1080 => "1080p",
            Resolution::P2160 => "4K",
        }
    }
}

/// Block size used by Robust mode: each bit is drawn as a square of this many pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cell {
    /// 4 x 4 pixels per bit.
    C4,
    /// 8 x 8 pixels per bit.
    C8,
}

impl Cell {
    /// Side of the square in pixels.
    pub fn pixels(self) -> u32 {
        match self {
            Cell::C4 => 4,
            Cell::C8 => 8,
        }
    }
}

/// How bits are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// One pixel per bit, stored with a lossless codec.
    Lossless,
    /// Blocks of pixels plus Reed-Solomon error correction, made to survive re-compression.
    /// `parity` is the number of parity bytes in each 255-byte codeword.
    Robust { cell: Cell, parity: u8 },
}

impl Mode {
    /// Default Reed-Solomon parity bytes per codeword: "strong repair".
    /// The owner chose 64 after WhatsApp's 480p copy of a 32-parity video
    /// failed by 2 bytes in one codeword while a 64-parity copy restored
    /// (spec 8.5). 32 is "standard repair": 14% more data per frame.
    pub const DEFAULT_PARITY: u8 = 64;
    /// Standard repair: more data per frame, less margin.
    pub const STANDARD_PARITY: u8 = 32;

    /// Side in pixels of the square that carries one bit.
    pub fn cell_pixels(&self) -> u32 {
        match self {
            Mode::Lossless => 1,
            Mode::Robust { cell, .. } => cell.pixels(),
        }
    }

    /// The file extension the output video must have for this mode.
    pub fn extension(&self) -> &'static str {
        match self {
            Mode::Lossless => "mkv",
            Mode::Robust { .. } => "mp4",
        }
    }
}

/// The video codec a Robust video is written with. Lossless videos are always FFV1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    /// H.264: fast, and every phone and site plays it.
    #[default]
    H264,
    /// AV1 with its screen-content tools (libaom, palette mode): about half
    /// the size of H.264 for ATV's two-colour blocks, but 10-20x slower to
    /// create (spec 7.1). Meant for YouTube uploads.
    Av1,
}

/// What to encode and how.
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    /// Files and folders to put in the video. Folders are added with everything inside them.
    pub inputs: Vec<PathBuf>,
    /// The video file to write. Its extension must match the mode (`.mkv` for Lossless).
    pub output: PathBuf,
    pub resolution: Resolution,
    pub mode: Mode,
    /// Compress the data with zstd before it is drawn.
    pub compress: bool,
    /// Compress harder (zstd level 19 instead of 3) when `compress` is on:
    /// documents measured 17% smaller, but creating the video is slower.
    /// Restoring is just as fast, and the video format is the same.
    pub smallest: bool,
    /// Encrypt the data with these secrets. `None` leaves the video readable by anyone.
    pub protection: Option<Protection>,
    /// Decode the finished video and check it matches before reporting success.
    pub verify: bool,
    /// Keep the video file even if verification fails. Normally it is deleted.
    pub keep_unverified: bool,
    /// Frames per second, for Robust mode only: fewer frames per second means
    /// less data per second, which survives harsher re-compression. `None`
    /// means 30. Lossless videos are always 30.
    pub frame_rate: Option<u32>,
    /// Codec for Robust videos; ignored for Lossless.
    pub video_codec: VideoCodec,
    /// Resolved ffmpeg and ffprobe, from [`crate::find_ffmpeg`].
    pub tools: FfmpegTools,
}

/// What to decode and where to put the result.
#[derive(Debug, Clone)]
pub struct DecodeOptions {
    /// The ATV video to read.
    pub video: PathBuf,
    /// Folder to restore into. It is created if missing. Existing files are never overwritten.
    pub output_dir: PathBuf,
    /// The password and/or key file, for an encrypted video.
    pub protection: Option<Protection>,
    /// Resolved ffmpeg and ffprobe, from [`crate::find_ffmpeg`].
    pub tools: FfmpegTools,
}

/// The step a job is on.
///
/// The pipeline streams, so compression, encryption and frame writing happen
/// together; the stage names the step that sets the pace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Encode: listing the inputs and adding up their sizes.
    Preparing,
    /// Encode: reading inputs and writing video frames.
    WritingFrames,
    /// Encode: decoding the finished video to check it.
    Verifying,
    /// Decode: reading video frames and restoring files.
    ReadingFrames,
    /// Decode: comparing the SHA-256 of the restored data.
    CheckingIntegrity,
}

/// A small grey picture of the frame being written or read, for a live preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes, one per pixel, row by row. 0 is black, 255 is white.
    pub pixels: Vec<u8>,
}

/// A progress report. Sent at most about 30 times per second.
#[derive(Debug, Clone)]
pub struct Event {
    pub stage: Stage,
    /// Encode: input bytes read so far. Decode: bytes written to restored files so far.
    pub bytes_done: u64,
    /// Encode: total input bytes. Decode: 0, because it is not known until the end.
    pub bytes_total: u64,
    /// Data frames written or read so far.
    pub frames_done: u64,
    /// Expected number of data frames. An estimate; may be missing. When
    /// compression is on it is an upper limit, since the saving is not known ahead.
    pub frames_total: Option<u64>,
    /// Bytes repaired by error correction so far (Robust mode only).
    pub repaired_bytes: u64,
    /// Present at most about 10 times per second.
    pub preview: Option<Preview>,
}

/// Result of a successful encode.
#[derive(Debug, Clone)]
pub struct EncodeReport {
    pub output: PathBuf,
    pub resolution: Resolution,
    pub mode: Mode,
    /// The ATV format version written: 3 for Robust, 1 for Lossless.
    pub format_version: u8,
    pub compressed: bool,
    pub encrypted: bool,
    /// The finished video was decoded again and matched.
    pub verified: bool,
    /// Frames per second of the video.
    pub frame_rate: u32,
    /// Robust mode: bytes that error correction had to repair while verifying
    /// the fresh video. A sign of how much margin is left for re-compression.
    pub verify_repaired_bytes: u64,
    /// Total size of the input files.
    pub input_bytes: u64,
    /// Number of files stored.
    pub file_count: u64,
    /// Total video frames, including the two header frames.
    pub frames: u64,
    /// Length of the video in seconds.
    pub seconds: f64,
    /// Size of the video file.
    pub video_bytes: u64,
    /// SHA-256 of the archived data inside the video.
    pub sha256: [u8; 32],
    /// Symbolic links found inside input folders. They are not stored.
    pub skipped_links: u64,
}

/// One restored file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredFile {
    /// Path relative to the output folder.
    pub path: PathBuf,
    pub size: u64,
}

/// Result of a successful decode.
#[derive(Debug, Clone)]
pub struct DecodeReport {
    pub output_dir: PathBuf,
    /// The top-level files and folders created in `output_dir`, as full paths.
    pub items: Vec<PathBuf>,
    /// Every restored file.
    pub files: Vec<RestoredFile>,
    /// Total size of the restored files.
    pub total_bytes: u64,
    /// SHA-256 of the archived data; it matched the one stored in the video.
    pub sha256: [u8; 32],
    /// Bytes repaired by error correction (Robust mode only).
    pub repaired_bytes: u64,
    /// Archive entries that were not restored (links and special files).
    pub skipped_entries: u64,
}

/// What a video says about itself in its header frame. Nothing here is secret.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub resolution: Resolution,
    pub mode: Mode,
    /// The ATV format version the video was made with: 1, 2 or 3 (spec 5.4,
    /// 5.4b). Robust videos are 3 since 14 Nov 2026.
    pub format_version: u8,
    pub compressed: bool,
    pub encrypted: bool,
    /// The video needs a password to decode.
    pub needs_password: bool,
    /// The video needs a key file to decode.
    pub needs_key_file: bool,
    /// Name of the video codec, as ffprobe reports it.
    pub codec: String,
    /// Length in seconds, if ffprobe could tell.
    pub seconds: Option<f64>,
    /// Number of video frames, header frames included. An estimate for most MKV files.
    pub frames: Option<u64>,
    /// Size of the video file.
    pub file_bytes: u64,
    /// A small picture of the first data frame, for a thumbnail.
    pub preview: Option<Preview>,
    /// The file's own frame size when a site resized the video; it is read
    /// scaled back to `resolution`. `None` for a video at its ATV size.
    pub resized_from: Option<(u32, u32)>,
}

/// A rough prediction for the summary strip, made before encoding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Total video frames, header frames included.
    pub frames: u64,
    pub seconds: f64,
    pub video_bytes: u64,
}

/// Lets another thread stop a running job.
///
/// Cloning gives a handle to the same flag. A cancelled encode kills ffmpeg and
/// deletes the partial video; a cancelled decode removes the partly restored files.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the job to stop. Safe to call from any thread, any number of times.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
