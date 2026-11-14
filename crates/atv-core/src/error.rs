//! The one error type returned by every `atv-core` function.

use std::io;

use thiserror::Error;

/// Everything that can go wrong in the engine.
///
/// Messages are written for the person using the app. They never contain
/// passwords or key material.
#[derive(Debug, Error)]
pub enum AtvError {
    /// ffmpeg or ffprobe is missing, cannot be run, or lacks a needed encoder.
    #[error("ffmpeg is not ready: {detail}")]
    FfmpegNotFound { detail: String },

    /// ffmpeg started but stopped with an error. `stderr` is the end of what it printed.
    #[error("ffmpeg stopped unexpectedly: {stderr}")]
    FfmpegFailed { stderr: String },

    /// The file is not a video made by ATV.
    #[error("this file is not an ATV video")]
    NotAtvVideo,

    /// The video was made by a newer version of the format.
    #[error(
        "this video uses ATV format version {version}, which this version of the app cannot read"
    )]
    UnsupportedVersion { version: u8 },

    /// The video is not 1920 x 1080 or 3840 x 2160.
    #[error(
        "the video is {width} x {height}; ATV videos are 1920 x 1080 or 3840 x 2160 (was it resized?)"
    )]
    UnsupportedResolution { width: u32, height: u32 },

    /// The video is encrypted and the needed secrets were not given.
    #[error("{}", key_required_message(.password, .key_file))]
    KeyRequired { password: bool, key_file: bool },

    /// The secrets given do not open the video.
    #[error("wrong password or key file, or the video is damaged")]
    WrongKey,

    /// Encode only: the password is the video's only secret and is too easy
    /// to guess (spec 6.4). A longer password or a key file fixes it.
    #[error("this password is too easy to guess: use a longer phrase, or add a key file")]
    WeakPassword,

    /// A data frame could not be read back. `frame` counts data frames from 0.
    #[error("the video is too damaged to restore (data frame {frame} could not be recovered)")]
    Damaged { frame: u64 },

    /// The data inside the video cannot be read back as files: it is damaged,
    /// or the video was changed after it was made (for example its header).
    #[error("the data inside the video cannot be read: the video is damaged or was changed")]
    Corrupt,

    /// The video stops before its end: it was cut short or not fully copied
    /// (or its end was changed).
    #[error("the end of the video is missing: it was cut short, not fully copied, or changed")]
    Truncated,

    /// Every frame was read but the restored data does not match its SHA-256.
    #[error("the restored data does not match its checksum, so the video is damaged")]
    IntegrityFailed,

    /// Verify-after-encode found that the finished video does not decode to the same data.
    #[error("verification failed: the finished video does not decode back to the same data")]
    VerifyFailed,

    /// An archive entry tried to leave the output folder.
    #[error("the video contains an unsafe file path and was not restored: {path}")]
    UnsafePath { path: String },

    /// The request itself cannot be carried out (no inputs, unreadable input, bad output name, ...).
    #[error("{message}")]
    InvalidInput { message: String },

    /// yt-dlp, which fetches videos from YouTube, is missing or cannot be run.
    #[error("yt-dlp is not ready: {detail}")]
    DownloaderNotFound { detail: String },

    /// yt-dlp could not get the video. `detail` is what it said.
    #[error("the video could not be downloaded: {detail}")]
    DownloadFailed { detail: String },

    /// A helper program (ffmpeg, yt-dlp, Deno) could not be checked or installed.
    #[error("the tool could not be installed: {detail}")]
    ToolInstallFailed { detail: String },

    /// The job was cancelled through its `CancelToken`.
    #[error("cancelled")]
    Cancelled,

    /// A file or pipe operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl AtvError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        AtvError::InvalidInput {
            message: message.into(),
        }
    }
}

fn key_required_message(password: &bool, key_file: &bool) -> &'static str {
    match (*password, *key_file) {
        (true, true) => "this video is encrypted: it needs its password and its key file",
        (true, false) => "this video is encrypted: it needs its password",
        (false, true) => "this video is encrypted: it needs its key file",
        (false, false) => "this video is encrypted",
    }
}
