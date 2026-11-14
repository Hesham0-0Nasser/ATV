//! Plain words for what went wrong, and short labels for modes.

use atv_core::{AtvError, Cell, Mode};
use serde::Serialize;

use crate::text;

/// An error as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorView {
    /// Which screen or message to show: `wrong_key`, `key_required`,
    /// `not_atv`, `damaged`, `disk`, `verify`, `ffmpeg`, `input` or `other`.
    pub code: &'static str,
    pub title: String,
    pub text: String,
    /// Small print: what the engine or ffmpeg said.
    pub detail: String,
    /// For a damaged video: the share that could be read, 0 to 1.
    pub recoverable: Option<f32>,
}

fn view(code: &'static str, title: &str, text: &str, detail: String) -> ErrorView {
    ErrorView {
        code,
        title: title.to_string(),
        text: text.to_string(),
        detail,
        recoverable: None,
    }
}

/// What failed, in plain words, and what to do next.
pub fn explain(error: &AtvError) -> ErrorView {
    match error {
        AtvError::FfmpegNotFound { detail } => view(
            "ffmpeg",
            "ffmpeg is not ready",
            "ATV needs ffmpeg to write and read videos. Check it in Settings.",
            capitalise(detail),
        ),
        AtvError::FfmpegFailed { stderr } if disk_full(stderr) => disk_full_message(),
        AtvError::FfmpegFailed { stderr } => view(
            "ffmpeg",
            "Video writing stopped unexpectedly",
            "ffmpeg, the tool ATV uses for video, quit before finishing. Your files were not changed. Try again; if it keeps happening, check ffmpeg in Settings.",
            text::tail_lines(stderr, 3),
        ),
        AtvError::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => disk_full_message(),
        AtvError::Io(io) => view(
            "other",
            "A file could not be read or written",
            "Check that the files are still there and that the folder can be written to.",
            io.to_string(),
        ),
        AtvError::VerifyFailed => view(
            "verify",
            "The video didn’t pass verification",
            "ATV read the finished video back and it didn’t match your files, so it was deleted. Create it again before deleting the originals; if it fails again, save to another drive.",
            String::new(),
        ),
        AtvError::InvalidInput { message } => view(
            "input",
            "This could not be started",
            &capitalise(message),
            String::new(),
        ),
        AtvError::NotAtvVideo => view(
            "not_atv",
            "This isn’t an ATV video",
            "This file wasn’t made with ATV, so there are no files to restore from it.",
            "No ATV header was found in the first frames".to_string(),
        ),
        AtvError::UnsupportedResolution { .. } => view(
            "not_atv",
            "This isn’t an ATV video",
            "ATV videos are 1920 × 1080 or 3840 × 2160. This one has another size; it may have been resized.",
            capitalise(&error.to_string()),
        ),
        AtvError::UnsupportedVersion { .. } => view(
            "not_atv",
            "Made by a newer ATV",
            "This video uses a newer format. Update ATV to restore it.",
            capitalise(&error.to_string()),
        ),
        AtvError::Damaged { frame } => view(
            "damaged",
            "The video is too damaged to restore",
            "Too much of it was lost to rebuild the files, so nothing was restored. This can happen when a site re-compresses a video very heavily, or when the file was cut short.",
            format!(
                "Data frame {} could not be read",
                text::group_digits(*frame)
            ),
        ),
        AtvError::Corrupt => view(
            "damaged",
            "The video is damaged or was changed",
            "The data inside it cannot be read back as files, so nothing was restored.",
            String::new(),
        ),
        AtvError::Truncated => view(
            "damaged",
            "The end of the video is missing",
            "It was cut short, not fully copied or downloaded, or changed, so nothing was restored. Try the full file again.",
            String::new(),
        ),
        AtvError::IntegrityFailed => view(
            "damaged",
            "The video is too damaged to restore",
            "Every frame was read, but the data does not match its checksum, so nothing was restored.",
            String::new(),
        ),
        AtvError::WrongKey => view(
            "wrong_key",
            "Wrong password or key file",
            "That password or key file doesn’t match this video. Check it and try again.",
            String::new(),
        ),
        AtvError::WeakPassword => view(
            "weak_password",
            "This password is too easy to guess",
            "Use a longer phrase, such as three or four unrelated words, or add a key file.",
            String::new(),
        ),
        AtvError::KeyRequired { .. } => view(
            "key_required",
            "This video is locked",
            &capitalise(&error.to_string()),
            String::new(),
        ),
        AtvError::UnsafePath { path } => view(
            "other",
            "The video holds an unsafe file name",
            "A file in this video tried to leave the output folder, so nothing was restored.",
            path.clone(),
        ),
        AtvError::DownloaderNotFound { detail } => view(
            "downloader",
            "yt-dlp is needed",
            "ATV uses the free yt-dlp program to fetch videos from YouTube. Install it, then try again.",
            capitalise(detail),
        ),
        AtvError::DownloadFailed { detail } => view(
            "download",
            "The video could not be downloaded",
            &download_advice(detail),
            detail.clone(),
        ),
        AtvError::ToolInstallFailed { detail } => view(
            "tool",
            "The tool could not be installed",
            "Check your internet connection and try again. Nothing was changed.",
            capitalise(detail),
        ),
        AtvError::Cancelled => view(
            "other",
            "Cancelled",
            "Nothing was left behind.",
            String::new(),
        ),
    }
}

/// What is wrong with ffmpeg, as one sentence for Settings.
pub fn ffmpeg_problem(error: &AtvError) -> String {
    match error {
        AtvError::FfmpegNotFound { detail } => capitalise(detail) + ".",
        other => capitalise(&other.to_string()) + ".",
    }
}

/// Plain advice for the usual reasons a YouTube download fails.
fn download_advice(detail: &str) -> String {
    let lower = detail.to_lowercase();
    if lower.contains("private video") {
        "This video is private. Make it Unlisted in YouTube Studio, then try again.".to_string()
    } else if lower.contains("unavailable") || lower.contains("removed") {
        "YouTube says this video is not available. Check the link.".to_string()
    } else if lower.contains("sign in") || lower.contains("bot") {
        "YouTube asked to confirm you are not a bot. Wait a while and try again, or update yt-dlp (it changes often to keep up with YouTube).".to_string()
    } else if lower.contains("403") || lower.contains("forbidden") {
        "YouTube refused the download this time. Try again in a moment. If it keeps happening, update yt-dlp and install Deno or Node.js, which yt-dlp uses to unlock YouTube downloads.".to_string()
    } else if lower.contains("unable to download")
        || lower.contains("network")
        || lower.contains("timed out")
        || lower.contains("getaddrinfo")
    {
        "The connection to YouTube failed. Check your internet connection and try again."
            .to_string()
    } else {
        "Check the link and your internet connection, then try again. If it keeps failing, update yt-dlp.".to_string()
    }
}

fn disk_full(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("no space left")
        || lower.contains("not enough space")
        || lower.contains("disk full")
}

fn disk_full_message() -> ErrorView {
    view(
        "disk",
        "Not enough disk space",
        "The drive filled up while the video was being written. Free up space or choose another save location, then try again.",
        String::new(),
    )
}

/// The first letter in capitals, except for names that are always written
/// in small letters, such as ffmpeg.
pub fn capitalise(text: &str) -> String {
    if text.starts_with("ffmpeg") || text.starts_with("ffprobe") {
        return text.to_string();
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `Lossless`, `Robust · 8 × 8 blocks`.
pub fn mode_label(mode: &Mode) -> String {
    match mode {
        Mode::Lossless => "Lossless".to_string(),
        Mode::Robust { cell, parity } => {
            let side = cell.pixels();
            if *parity >= Mode::DEFAULT_PARITY {
                format!("Robust · {side} × {side} blocks, strong repair")
            } else {
                format!("Robust · {side} × {side} blocks, standard repair")
            }
        }
    }
}

/// `Lossless`, `Robust 8 × 8`: the short form used in tags and history.
pub fn mode_short(mode: &Mode) -> String {
    match mode {
        Mode::Lossless => "Lossless".to_string(),
        Mode::Robust { cell, .. } => {
            let side = cell.pixels();
            format!("Robust {side} × {side}")
        }
    }
}

/// The mode from the page's choices: `lossless` or `robust`, 4 or 8, and
/// 32 (standard repair) or 64 (strong repair).
pub fn mode_from(mode: &str, cell: u32, parity: u32) -> Mode {
    if mode == "robust" {
        Mode::Robust {
            cell: if cell == 4 { Cell::C4 } else { Cell::C8 },
            parity: if parity == u32::from(Mode::STANDARD_PARITY) {
                Mode::STANDARD_PARITY
            } else {
                Mode::DEFAULT_PARITY
            },
        }
    } else {
        Mode::Lossless
    }
}

pub fn resolution_from(resolution: &str) -> atv_core::Resolution {
    if resolution == "4k" {
        atv_core::Resolution::P2160
    } else {
        atv_core::Resolution::P1080
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_get_a_code_and_plain_words() {
        let disk = [
            AtvError::FfmpegFailed {
                stderr: "out.mkv: No space left on device".into(),
            },
            AtvError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull)),
        ];
        for error in disk {
            assert_eq!(explain(&error).code, "disk");
        }
        let stopped = explain(&AtvError::FfmpegFailed {
            stderr: "line 1\nline 2\nline 3\nline 4".into(),
        });
        assert_eq!(stopped.code, "ffmpeg");
        assert_eq!(stopped.detail, "line 2\nline 3\nline 4");
        assert_eq!(explain(&AtvError::VerifyFailed).code, "verify");
        assert_eq!(explain(&AtvError::NotAtvVideo).code, "not_atv");
        assert_eq!(
            explain(&AtvError::Damaged { frame: 1234 }).detail,
            "Data frame 1,234 could not be read"
        );
        assert_eq!(explain(&AtvError::WrongKey).code, "wrong_key");
        assert_eq!(
            ffmpeg_problem(&AtvError::FfmpegNotFound {
                detail: "ffmpeg was not found".into()
            }),
            "ffmpeg was not found."
        );
        assert_eq!(
            explain(&AtvError::InvalidInput {
                message: "nothing to encode".into()
            })
            .text,
            "Nothing to encode"
        );
    }

    #[test]
    fn modes_read_naturally() {
        assert_eq!(mode_label(&mode_from("lossless", 8, 64)), "Lossless");
        assert_eq!(
            mode_label(&mode_from("robust", 4, 64)),
            "Robust · 4 × 4 blocks, strong repair"
        );
        assert_eq!(
            mode_label(&mode_from("robust", 8, 32)),
            "Robust · 8 × 8 blocks, standard repair"
        );
        assert_eq!(mode_short(&mode_from("robust", 8, 64)), "Robust 8 × 8");
        // Anything unexpected from the page means the safe default.
        assert_eq!(
            mode_from("robust", 8, 7),
            Mode::Robust {
                cell: Cell::C8,
                parity: 64
            }
        );
        assert_eq!(resolution_from("4k"), atv_core::Resolution::P2160);
        assert_eq!(
            resolution_from("anything else"),
            atv_core::Resolution::P1080
        );
    }
}
