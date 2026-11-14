//! Finding ffmpeg and ffprobe, and running them as child processes (spec section 7).
//!
//! ffmpeg is never bundled. Frames go to it and come from it through pipes;
//! its error output is read on a separate thread so neither side can block on
//! a full pipe.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::AtvError;
use crate::format::Layout;
use crate::types::{Mode, Resolution, VideoCodec};

/// How much of ffmpeg's error output is kept for error messages.
const STDERR_TAIL: usize = 8 * 1024;
/// How long to wait for ffmpeg to exit by itself after its input pipe broke.
const EXIT_GRACE: Duration = Duration::from_secs(5);

#[cfg(windows)]
const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const EXE_SUFFIX: &str = "";

/// Resolved, checked locations of the two programs ATV needs.
/// Get one from [`find_ffmpeg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegTools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// The version text ffmpeg reports, for example `8.1-full_build-www.gyan.dev`.
    pub version: String,
    /// Whether this ffmpeg knows `-fps_mode` (5.1 and later). Older ones use `-vsync`.
    fps_mode: bool,
    /// Whether this ffmpeg has the libaom AV1 encoder, for [`crate::VideoCodec::Av1`].
    pub av1: bool,
}

/// Look for a working ffmpeg and ffprobe.
///
/// Order: `override_path` (the path saved in Settings; a file or a folder),
/// then `PATH`, then the usual install folders. A candidate is accepted when
/// it runs, has ffprobe beside it or findable, and has the `ffv1` and
/// `libx264` encoders.
pub fn find_ffmpeg(override_path: Option<&Path>) -> Result<FfmpegTools, AtvError> {
    let folders = search_folders();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = override_path {
        candidates.push(if path.is_dir() {
            path.join(exe_name("ffmpeg"))
        } else {
            path.to_path_buf()
        });
    }
    candidates.extend(folders.iter().map(|folder| folder.join(exe_name("ffmpeg"))));

    let mut first_problem: Option<String> = None;
    let mut seen: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if !candidate.is_file() || seen.contains(&candidate) {
            continue;
        }
        seen.push(candidate.clone());
        match check_candidate(&candidate, &folders) {
            Ok(tools) => return Ok(tools),
            Err(problem) => {
                first_problem.get_or_insert(problem);
            }
        }
    }
    Err(AtvError::FfmpegNotFound {
        detail: first_problem.unwrap_or_else(|| {
            "ffmpeg was not found in Settings, on the PATH or in the usual install folders"
                .to_string()
        }),
    })
}

fn exe_name(base: &str) -> String {
    format!("{base}{EXE_SUFFIX}")
}

/// Folders on `PATH`, then the common install locations for this system.
fn search_folders() -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    folders.retain(|folder| !folder.as_os_str().is_empty());
    folders.extend(common_folders());
    folders
}

#[cfg(windows)]
fn common_folders() -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let mut add = |variable: &str, rest: &str| {
        if let Some(base) = env::var_os(variable) {
            folders.push(PathBuf::from(base).join(rest));
        }
    };
    add("LOCALAPPDATA", r"Microsoft\WinGet\Links");
    add("ProgramData", r"chocolatey\bin");
    add("USERPROFILE", r"scoop\shims");
    add("ProgramFiles", r"ffmpeg\bin");
    add("SystemDrive", r"\ffmpeg\bin");
    folders
}

#[cfg(target_os = "macos")]
fn common_folders() -> Vec<PathBuf> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn common_folders() -> Vec<PathBuf> {
    ["/usr/local/bin", "/usr/bin", "/snap/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Run one ffmpeg candidate through all the checks. The error is a sentence
/// for the person, saying what is wrong with this candidate.
fn check_candidate(ffmpeg: &Path, folders: &[PathBuf]) -> Result<FfmpegTools, String> {
    let shown = ffmpeg.display();
    let version_text =
        run_for_stdout(ffmpeg, &["-version"]).ok_or(format!("{shown} could not be run"))?;
    let (version, number) = parse_version(&version_text);

    let beside = ffmpeg.with_file_name(exe_name("ffprobe"));
    let ffprobe = if beside.is_file() {
        beside
    } else {
        folders
            .iter()
            .map(|folder| folder.join(exe_name("ffprobe")))
            .find(|path| path.is_file())
            .ok_or(format!("ffprobe was not found (ffmpeg is at {shown})"))?
    };
    run_for_stdout(&ffprobe, &["-version"])
        .ok_or(format!("{} could not be run", ffprobe.display()))?;

    let encoders = run_for_stdout(ffmpeg, &["-hide_banner", "-encoders"])
        .ok_or(format!("{shown} could not list its encoders"))?;
    for needed in ["ffv1", "libx264"] {
        if !has_encoder(&encoders, needed) {
            return Err(format!(
                "the ffmpeg at {shown} was built without the {needed} encoder"
            ));
        }
    }

    Ok(FfmpegTools {
        ffmpeg: ffmpeg.to_path_buf(),
        ffprobe,
        version,
        // `-fps_mode` arrived in ffmpeg 5.1. Builds with an unusual version
        // text (nightly or git builds) are recent, so assume they have it.
        fps_mode: number.is_none_or(|number| number >= (5, 1)),
        av1: has_encoder(&encoders, "libaom-av1"),
    })
}

/// Run a program and return what it printed, or `None` if it failed.
fn run_for_stdout(program: &Path, args: &[&str]) -> Option<String> {
    let output = quiet_command(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Pull the version out of the first line of `ffmpeg -version`, both as text
/// and, when it starts with numbers, as (major, minor).
fn parse_version(output: &str) -> (String, Option<(u32, u32)>) {
    let text = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("ffmpeg version "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("unknown")
        .to_string();
    // Release builds look like "8.1", "6.1.1-3ubuntu5" or "n7.0.2".
    let digits = text.trim_start_matches(['n', 'v']);
    let mut parts = digits.split(|c: char| !c.is_ascii_digit());
    let major = parts.next().and_then(|part| part.parse().ok());
    let minor = parts.next().and_then(|part| part.parse().ok());
    // A leading year such as "2024-05-01-git" is a dated build, not a version.
    let number = match (major, digits.contains('.')) {
        (Some(major), true) if major < 1000 => Some((major, minor.unwrap_or(0))),
        _ => None,
    };
    (text, number)
}

/// True when `ffmpeg -encoders` lists an encoder with exactly this name.
fn has_encoder(listing: &str, name: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(name))
}

/// A command that does not open a console window when run from a GUI app.
fn quiet_command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn spawn_failed(program: &Path, error: io::Error) -> AtvError {
    AtvError::FfmpegNotFound {
        detail: format!("{} could not be started ({error})", program.display()),
    }
}

/// An absolute path, so a name starting with `-` can never be read as an option.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Keep the end of a stream as text. Runs on its own thread until the stream closes.
fn collect_tail(mut stream: impl Read + Send + 'static) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > 2 * STDERR_TAIL {
                        tail.drain(..tail.len() - STDERR_TAIL);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if tail.len() > STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
        String::from_utf8_lossy(&tail).trim().to_string()
    })
}

fn join_tail(handle: Option<JoinHandle<String>>) -> String {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn failed(stderr: String, status: Option<std::process::ExitStatus>) -> AtvError {
    let stderr = if !stderr.is_empty() {
        stderr
    } else if let Some(status) = status {
        format!("it ended with {status} and printed no message")
    } else {
        "it stopped reading and printed no message".to_string()
    };
    AtvError::FfmpegFailed { stderr }
}

/// An ffmpeg process that turns piped raw frames into a video file.
///
/// Dropping it without calling [`Encoder::finish`] kills the process.
pub(crate) struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Option<JoinHandle<String>>,
    pipe_broken: bool,
    reaped: bool,
}

impl Encoder {
    pub(crate) fn spawn(
        tools: &FfmpegTools,
        layout: Layout,
        mode: &Mode,
        frame_rate: u32,
        codec: VideoCodec,
        output: &Path,
    ) -> Result<Self, AtvError> {
        let mut command = quiet_command(&tools.ffmpeg);
        command
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "rawvideo", "-pix_fmt", "monob"])
            .arg("-s")
            .arg(format!("{}x{}", layout.width, layout.height))
            .arg("-r")
            .arg(frame_rate.to_string())
            .args(["-i", "-"]);
        match mode {
            Mode::Lossless => {
                command
                    .args([
                        "-c:v", "ffv1", "-level", "3", "-coder", "1", "-context", "1",
                    ])
                    .args(["-g", "1", "-slices", "16", "-slicecrc", "1"])
                    .args(["-pix_fmt", "gray"]);
            }
            Mode::Robust { .. } if codec == VideoCodec::Av1 => {
                // AV1's screen-content tools (palette mode) store two-colour
                // blocks in about half the bytes of H.264 (spec 7.1).
                command
                    .args(["-c:v", "libaom-av1", "-cpu-used", "6", "-crf", "50"])
                    .args(["-row-mt", "1", "-tiles", "2x2", "-enable-palette", "1"])
                    .args(["-aom-params", "tune-content=screen"])
                    .args(["-g", "30", "-pix_fmt", "yuv420p"])
                    .args(["-movflags", "+faststart"]);
            }
            Mode::Robust {
                cell: crate::types::Cell::C8,
                ..
            } => {
                // 8 x 8 blocks (the WhatsApp route): tuned H.264, measured 10%
                // smaller than CRF 12 with the same read-back, and still
                // passing the harsh WhatsApp imitation (spec 7.1, 8.6):
                // constant quantiser, no adaptive or psychovisual tricks
                // (they spend bits on looks, not edges), no deblocking (it
                // blurs block edges), trellis quantisation. 4 x 4 blocks lost
                // their margin through the harsh YouTube 720p imitation with
                // these settings, so they keep CRF 12 below.
                command
                    .args(["-c:v", "libx264", "-preset", "slow", "-qp", "40"])
                    .args([
                        "-x264-params",
                        "aq-mode=0:psy-rd=0,0:no-deblock=1:trellis=2",
                    ])
                    .args(["-g", "30", "-pix_fmt", "yuv420p"])
                    .args(["-movflags", "+faststart"]);
            }
            Mode::Robust { .. } => {
                // The cells are already drawn at full size; H.264 at high
                // quality keeps them readable and plays everywhere.
                command
                    .args(["-c:v", "libx264", "-preset", "medium", "-crf", "12"])
                    .args(["-g", "30", "-pix_fmt", "yuv420p"])
                    .args(["-movflags", "+faststart"]);
            }
        }
        command
            .arg(absolute(output))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| spawn_failed(&tools.ffmpeg, error))?;
        let stdin = child.stdin.take();
        let stderr = child.stderr.take().map(collect_tail);
        Ok(Encoder {
            child,
            stdin,
            stderr,
            pipe_broken: false,
            reaped: false,
        })
    }

    /// Send one raw frame. If this fails, ffmpeg has most likely died; ask
    /// [`Encoder::failure`] for the reason.
    pub(crate) fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let result = match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(frame),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        };
        if result.is_err() {
            self.pipe_broken = true;
        }
        result
    }

    /// True once a write to ffmpeg has failed.
    pub(crate) fn pipe_broken(&self) -> bool {
        self.pipe_broken
    }

    /// Close the input, let ffmpeg finish the file, and check that it succeeded.
    pub(crate) fn finish(mut self) -> Result<(), AtvError> {
        drop(self.stdin.take());
        let status = self.child.wait();
        self.reaped = true;
        let stderr = join_tail(self.stderr.take());
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(failed(stderr, Some(status))),
            Err(error) => Err(AtvError::Io(error)),
        }
    }

    /// After a failed write: collect what ffmpeg said and return it as the error.
    pub(crate) fn failure(mut self) -> AtvError {
        drop(self.stdin.take());
        let deadline = Instant::now() + EXIT_GRACE;
        let mut status = None;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(exited)) => {
                    status = Some(exited);
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
        if status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.reaped = true;
        failed(join_tail(self.stderr.take()), status)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.reaped {
            drop(self.stdin.take());
            let _ = self.child.kill();
            let _ = self.child.wait();
            join_tail(self.stderr.take());
        }
    }
}

/// An ffmpeg process that decodes a video to raw 8-bit grey frames on a pipe.
///
/// Dropping it kills the process, so reading can stop at any frame.
pub(crate) struct Decoder {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<JoinHandle<String>>,
}

impl Decoder {
    /// `max_frames` stops ffmpeg after that many frames (used to read just the header).
    /// `scale_to` makes ffmpeg scale every frame to that size first, for a
    /// copy that a site resized (spec 5.5).
    pub(crate) fn spawn(
        tools: &FfmpegTools,
        video: &Path,
        max_frames: Option<u32>,
        scale_to: Option<Resolution>,
    ) -> Result<Self, AtvError> {
        let mut command = quiet_command(&tools.ffmpeg);
        command
            .args(["-hide_banner", "-loglevel", "error"])
            .arg("-i")
            .arg(absolute(video))
            .args(["-map", "0:v:0"]);
        // Stop ffmpeg from inventing or dropping frames.
        if tools.fps_mode {
            command.args(["-fps_mode", "passthrough"]);
        } else {
            command.args(["-vsync", "0"]);
        }
        if let Some(count) = max_frames {
            command.arg("-frames:v").arg(count.to_string());
        }
        if let Some(size) = scale_to {
            // Lanczos keeps the edges between cells sharpest when scaling up.
            command.arg("-vf").arg(format!(
                "scale={}:{}:flags=lanczos",
                size.width(),
                size.height()
            ));
        }
        command
            .args(["-f", "rawvideo", "-pix_fmt", "gray", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| spawn_failed(&tools.ffmpeg, error))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take().map(collect_tail);
        Ok(Decoder {
            child,
            stdout,
            stderr,
        })
    }

    /// Fill `frame` with the next frame. `Ok(false)` means the video has ended
    /// (a final partial frame counts as the end).
    pub(crate) fn read_frame(&mut self, frame: &mut [u8]) -> io::Result<bool> {
        let Some(stdout) = self.stdout.as_mut() else {
            return Ok(false);
        };
        let mut filled = 0;
        while filled < frame.len() {
            match stdout.read(&mut frame[filled..]) {
                Ok(0) => return Ok(false),
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        drop(self.stdout.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        join_tail(self.stderr.take());
    }
}

/// What ffprobe says about the first video stream of a file.
#[derive(Debug, Clone)]
pub(crate) struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub codec: String,
    /// Length in seconds, when the container records it.
    pub seconds: Option<f64>,
    /// Number of frames. Counted when the container records it, otherwise
    /// estimated from the length; good enough for a progress bar only.
    pub frames: Option<u64>,
    pub file_bytes: u64,
}

/// Ask ffprobe about a video. `NotAtvVideo` when the file has no video stream
/// ffprobe can read.
pub(crate) fn probe_stream(tools: &FfmpegTools, video: &Path) -> Result<StreamInfo, AtvError> {
    let metadata = fs::metadata(video)?;
    if !metadata.is_file() {
        return Err(AtvError::invalid(format!(
            "{} is not a file",
            video.display()
        )));
    }
    let output = quiet_command(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0"])
        .arg("-show_entries")
        // `format=duration` is added to the spec's list because MKV keeps the
        // length on the file, not on the stream.
        .arg("stream=width,height,nb_frames,avg_frame_rate,duration,codec_name:format=duration")
        .args(["-of", "json"])
        .arg(absolute(video))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| spawn_failed(&tools.ffprobe, error))?;
    if !output.status.success() {
        return Err(AtvError::NotAtvVideo);
    }
    parse_probe(&output.stdout, metadata.len()).ok_or(AtvError::NotAtvVideo)
}

fn parse_probe(json: &[u8], file_bytes: u64) -> Option<StreamInfo> {
    let root: serde_json::Value = serde_json::from_slice(json).ok()?;
    let stream = root.get("streams")?.get(0)?;
    let width = u32::try_from(stream.get("width")?.as_u64()?).ok()?;
    let height = u32::try_from(stream.get("height")?.as_u64()?).ok()?;
    let text = |value: Option<&serde_json::Value>| value?.as_str().map(str::to_owned);
    let number = |value: Option<&serde_json::Value>| -> Option<f64> {
        text(value)?.parse::<f64>().ok().filter(|n| n.is_finite())
    };
    let seconds = number(stream.get("duration"))
        .or_else(|| number(root.get("format").and_then(|format| format.get("duration"))))
        .filter(|seconds| *seconds >= 0.0);
    let rate = text(stream.get("avg_frame_rate")).and_then(|rate| parse_rate(&rate));
    let counted = text(stream.get("nb_frames")).and_then(|count| count.parse::<u64>().ok());
    let frames = counted.or_else(|| Some((seconds? * rate?).round() as u64));
    Some(StreamInfo {
        width,
        height,
        codec: text(stream.get("codec_name")).unwrap_or_default(),
        seconds,
        frames,
        file_bytes,
    })
}

/// Parse a frame rate written as a fraction, such as `30/1` or `30000/1001`.
fn parse_rate(rate: &str) -> Option<f64> {
    let (top, bottom) = rate.split_once('/').unwrap_or((rate, "1"));
    let top: f64 = top.trim().parse().ok()?;
    let bottom: f64 = bottom.trim().parse().ok()?;
    (bottom > 0.0 && top > 0.0).then_some(top / bottom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_parsed_from_the_first_line() {
        let cases = [
            (
                "ffmpeg version 8.1-full_build-www.gyan.dev Copyright (c) 2000-2026",
                "8.1-full_build-www.gyan.dev",
                Some((8, 1)),
            ),
            (
                "ffmpeg version 6.1.1-3ubuntu5 Copyright",
                "6.1.1-3ubuntu5",
                Some((6, 1)),
            ),
            ("ffmpeg version n7.0.2 Copyright", "n7.0.2", Some((7, 0))),
            (
                "ffmpeg version 4.4.2\nbuilt with gcc",
                "4.4.2",
                Some((4, 4)),
            ),
            (
                "ffmpeg version N-113007-g8d24a28d06 Copyright",
                "N-113007-g8d24a28d06",
                None,
            ),
            (
                "ffmpeg version 2024-05-01-git-abc Copyright",
                "2024-05-01-git-abc",
                None,
            ),
            ("something else", "unknown", None),
        ];
        for (output, text, number) in cases {
            assert_eq!(
                parse_version(output),
                (text.to_string(), number),
                "{output}"
            );
        }
    }

    #[test]
    fn encoders_are_matched_by_exact_name() {
        let listing = " V....D libx264rgb          libx264 H.264 RGB\n V.S..D ffv1                 FFmpeg video codec #1\n";
        assert!(has_encoder(listing, "ffv1"));
        assert!(has_encoder(listing, "libx264rgb"));
        assert!(
            !has_encoder(listing, "libx264"),
            "a longer name must not match"
        );
    }

    #[test]
    fn probe_output_is_parsed() {
        let mkv = br#"{"streams":[{"codec_name":"ffv1","width":1920,"height":1080,"avg_frame_rate":"30/1"}],"format":{"duration":"2.500000"}}"#;
        let info = parse_probe(mkv, 99).unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
        assert_eq!(info.codec, "ffv1");
        assert_eq!(info.seconds, Some(2.5));
        assert_eq!(info.frames, Some(75), "estimated from length x rate");
        assert_eq!(info.file_bytes, 99);

        let mp4 = br#"{"streams":[{"codec_name":"h264","width":3840,"height":2160,"avg_frame_rate":"30/1","duration":"1.0","nb_frames":"31"}],"format":{"duration":"1.03"}}"#;
        let info = parse_probe(mp4, 0).unwrap();
        assert_eq!(info.frames, Some(31), "a real count wins over the estimate");
        assert_eq!(info.seconds, Some(1.0));
    }

    #[test]
    fn probe_output_without_a_video_stream_is_refused() {
        assert!(parse_probe(br#"{"streams":[],"format":{}}"#, 0).is_none());
        assert!(parse_probe(b"not json", 0).is_none());
        assert!(parse_probe(br#"{"streams":[{"codec_name":"aac"}]}"#, 0).is_none());
    }

    #[test]
    fn frame_rates_are_parsed() {
        assert_eq!(parse_rate("30/1"), Some(30.0));
        assert_eq!(parse_rate("25"), Some(25.0));
        assert!((parse_rate("30000/1001").unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parse_rate("0/0"), None);
    }
}
