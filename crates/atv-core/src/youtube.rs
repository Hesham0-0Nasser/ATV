//! Getting an ATV video back from YouTube with the yt-dlp program (an
//! owner's decision after the YouTube test; spec section 13).
//!
//! Like ffmpeg, yt-dlp is not bundled: the person installs it. ATV only ever
//! passes it a link it has checked and rebuilt itself, after `--`, so nothing
//! typed can be read as an option.
//!
//! The download is the best video stream YouTube offers (up to 4K), without
//! sound and without any re-encoding by ATV. If YouTube has not made the full
//! size yet, the smaller copy still decodes, because ATV reads resized copies.

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::error::AtvError;
use crate::types::CancelToken;

#[cfg(windows)]
const EXE: &str = "yt-dlp.exe";
#[cfg(not(windows))]
const EXE: &str = "yt-dlp";

/// A checked YouTube link, rebuilt from its video id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YouTubeLink {
    /// The 11-character video id.
    pub id: String,
}

impl YouTubeLink {
    /// Recognise a YouTube video link in what the person pasted: watch,
    /// youtu.be, shorts, live and embed links, with or without `https://`,
    /// `www.` or `m.`, and with any extra parameters. `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let rest = text
            .strip_prefix("https://")
            .or_else(|| text.strip_prefix("http://"))
            .unwrap_or(text);
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.to_ascii_lowercase();
        let host = host
            .strip_prefix("www.")
            .or_else(|| host.strip_prefix("m."))
            .unwrap_or(&host);
        let id = match host {
            "youtu.be" => path.split(['?', '&', '#', '/']).next(),
            "youtube.com" | "youtube-nocookie.com" => {
                let (route, query) = path.split_once('?').unwrap_or((path, ""));
                match route.trim_end_matches('/') {
                    "watch" => query.split('&').find_map(|pair| pair.strip_prefix("v=")),
                    other => ["shorts/", "live/", "embed/", "v/"]
                        .iter()
                        .find_map(|prefix| other.strip_prefix(prefix))
                        .map(|id| id.split(['/', '#']).next().unwrap_or(id)),
                }
            }
            _ => None,
        }?;
        let id = id.split(['#', '&']).next()?;
        let valid = id.len() == 11
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        valid.then(|| YouTubeLink { id: id.to_string() })
    }

    /// The link as ATV gives it to yt-dlp and to the browser.
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
}

/// A found, working yt-dlp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtDlp {
    pub path: PathBuf,
    pub version: String,
    /// The JavaScript program yt-dlp is told to use (`--js-runtimes`), as
    /// `name:path`. YouTube now hides its download links behind a puzzle that
    /// only JavaScript can solve; without one YouTube often refuses (HTTP 403).
    pub js_runtime: Option<String>,
}

/// Look for yt-dlp: the given path, ATV's own tools folder (spec 9.5b), the
/// `PATH`, then the usual install places (winget, Python's user scripts,
/// Scoop, Homebrew). The JavaScript runtime is looked for the same way.
pub fn find_ytdlp(
    override_path: Option<&Path>,
    tools_folder: Option<&Path>,
) -> Result<YtDlp, AtvError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = override_path {
        candidates.push(if path.is_dir() {
            path.join(EXE)
        } else {
            path.to_path_buf()
        });
    }
    if let Some(folder) = tools_folder {
        candidates.push(folder.join(EXE));
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|folder| folder.join(EXE)));
    }
    candidates.extend(common_places());
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        if let Some(text) = run_text(&candidate, &["--version"]) {
            return Ok(YtDlp {
                path: candidate,
                version: text.trim().to_string(),
                js_runtime: find_js_runtime(tools_folder),
            });
        }
    }
    Err(AtvError::DownloaderNotFound {
        detail: "yt-dlp was not found on the PATH or in the usual install folders".to_string(),
    })
}

/// The JavaScript runtimes yt-dlp can use, best first.
const JS_RUNTIMES: [&str; 3] = ["deno", "node", "bun"];

/// Look for Deno, Node.js or Bun: ATV's tools folder first, then the `PATH`
/// and their usual folders. Returns `name:path`, as yt-dlp's `--js-runtimes` takes it.
pub fn find_js_runtime(tools_folder: Option<&Path>) -> Option<String> {
    let var = |name: &str| env::var_os(name).map(PathBuf::from);
    let path_folders: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    for name in JS_RUNTIMES {
        let exe = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        let mut folders: Vec<PathBuf> = tools_folder.map(Path::to_path_buf).into_iter().collect();
        folders.extend(path_folders.iter().cloned());
        if let Some(home) = var("USERPROFILE").or_else(|| var("HOME")) {
            folders.push(home.join(format!(".{name}")).join("bin"));
        }
        if cfg!(windows) {
            if let Some(programs) = var("ProgramFiles") {
                folders.push(programs.join("nodejs"));
            }
            if let Some(local) = var("LOCALAPPDATA") {
                folders.push(local.join(r"Microsoft\WinGet\Links"));
            }
        } else {
            folders.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
        }
        let found = folders
            .iter()
            .map(|folder| folder.join(&exe))
            .find(|path| path.is_file());
        if let Some(found) = found {
            return Some(format!("{name}:{}", found.display()));
        }
    }
    None
}

/// `--js-runtimes <name:path>` when a runtime was found.
fn js_args(ytdlp: &YtDlp) -> Vec<String> {
    ytdlp
        .js_runtime
        .iter()
        .flat_map(|runtime| ["--js-runtimes".to_string(), runtime.clone()])
        .collect()
}

fn common_places() -> Vec<PathBuf> {
    let mut places = Vec::new();
    let var = |name: &str| env::var_os(name).map(PathBuf::from);
    if cfg!(windows) {
        if let Some(local) = var("LOCALAPPDATA") {
            places.push(local.join(r"Microsoft\WinGet\Links").join(EXE));
            places.extend(python_scripts(&local.join(r"Programs\Python")));
        }
        if let Some(roaming) = var("APPDATA") {
            places.extend(python_scripts(&roaming.join("Python")));
        }
        if let Some(home) = var("USERPROFILE") {
            places.push(home.join(r"scoop\shims").join(EXE));
        }
        if let Some(data) = var("ProgramData") {
            places.push(data.join(r"chocolatey\bin").join(EXE));
        }
    } else {
        for folder in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            places.push(Path::new(folder).join(EXE));
        }
        if let Some(home) = var("HOME") {
            places.push(home.join(".local/bin").join(EXE));
        }
    }
    places
}

/// `<base>\Python3xx\Scripts\yt-dlp.exe` for each installed Python.
fn python_scripts(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path().join("Scripts").join(EXE))
        .collect()
}

fn quiet(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

fn run_text(program: &Path, args: &[&str]) -> Option<String> {
    let output = quiet(program)
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

/// What YouTube says about a video, for the preview.
#[derive(Debug, Clone, PartialEq)]
pub struct OnlineVideo {
    pub link: YouTubeLink,
    pub title: String,
    pub channel: String,
    pub seconds: Option<f64>,
    /// The thumbnail picture's web address.
    pub thumbnail: Option<String>,
    /// Height of the largest picture YouTube currently offers, such as 720 or 2160.
    pub best_height: Option<u32>,
}

/// Ask YouTube (through yt-dlp) about a video, without downloading it.
pub fn video_details(ytdlp: &YtDlp, link: &YouTubeLink) -> Result<OnlineVideo, AtvError> {
    let output = quiet(&ytdlp.path)
        .args([
            "-J",
            "--no-playlist",
            "--no-warnings",
            "--encoding",
            "utf-8",
        ])
        .args(js_args(ytdlp))
        .arg("--")
        .arg(link.url())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AtvError::DownloaderNotFound {
            detail: format!("{} could not be started ({error})", ytdlp.path.display()),
        })?;
    if !output.status.success() {
        return Err(AtvError::DownloadFailed {
            detail: last_error(&String::from_utf8_lossy(&output.stderr)),
        });
    }
    parse_details(link, &output.stdout).ok_or_else(|| AtvError::DownloadFailed {
        detail: "yt-dlp gave details ATV could not read".to_string(),
    })
}

fn parse_details(link: &YouTubeLink, json: &[u8]) -> Option<OnlineVideo> {
    let root: serde_json::Value = serde_json::from_slice(json).ok()?;
    let text = |key: &str| {
        root.get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    let best_height = root
        .get("formats")
        .and_then(|formats| formats.as_array())
        .into_iter()
        .flatten()
        .filter(|format| format.get("vcodec").and_then(|codec| codec.as_str()) != Some("none"))
        .filter_map(|format| format.get("height").and_then(|height| height.as_u64()))
        .max()
        .and_then(|height| u32::try_from(height).ok());
    Some(OnlineVideo {
        link: link.clone(),
        title: text("title").unwrap_or_else(|| link.id.clone()),
        channel: text("channel")
            .or_else(|| text("uploader"))
            .unwrap_or_default(),
        seconds: root.get("duration").and_then(|value| value.as_f64()),
        thumbnail: text("thumbnail"),
        best_height,
    })
}

/// The useful part of what yt-dlp printed when it failed: its last ERROR line.
fn last_error(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|line| line.starts_with("ERROR"))
        .or_else(|| stderr.lines().rev().find(|line| !line.trim().is_empty()))
        .map(|line| line.trim_start_matches("ERROR: ").trim().to_string())
        .unwrap_or_else(|| "yt-dlp stopped without saying why".to_string())
}

/// How far a download has got.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    /// The full size, or YouTube's estimate of it, when known.
    pub total: Option<u64>,
    /// Bytes per second, when known.
    pub speed: Option<f64>,
}

const PROGRESS_MARK: &str = "ATVPROGRESS|";
const FILE_MARK: &str = "ATVFILE|";

/// How many times a download is tried when YouTube refuses it.
const ATTEMPTS: u32 = 3;

/// Download the best video stream of `link` into `folder`. Returns the file.
///
/// `on_progress` is called on this thread. Cancelling stops yt-dlp and
/// removes what it had written. YouTube sometimes refuses a download link
/// (HTTP 403) and accepts a fresh one moments later, so a refusal is tried
/// again, up to three times in all, each time with new links.
pub fn download(
    ytdlp: &YtDlp,
    link: &YouTubeLink,
    folder: &Path,
    on_progress: &mut dyn FnMut(DownloadProgress),
    cancel: &CancelToken,
) -> Result<PathBuf, AtvError> {
    let mut attempt = 1;
    loop {
        match download_once(ytdlp, link, folder, on_progress, cancel) {
            Err(AtvError::DownloadFailed { detail }) if attempt < ATTEMPTS && refused(&detail) => {
                attempt += 1;
                thread::sleep(Duration::from_secs(2));
            }
            result => return result,
        }
    }
}

/// YouTube refused the download link itself.
fn refused(detail: &str) -> bool {
    detail.contains("HTTP Error 403")
}

fn download_once(
    ytdlp: &YtDlp,
    link: &YouTubeLink,
    folder: &Path,
    on_progress: &mut dyn FnMut(DownloadProgress),
    cancel: &CancelToken,
) -> Result<PathBuf, AtvError> {
    fs::create_dir_all(folder)?;
    let template = folder.join("%(id)s.%(ext)s");
    let mut child = quiet(&ytdlp.path)
        .args(["--no-playlist", "--no-warnings", "--newline", "--no-colors", "--encoding", "utf-8"])
        .args(js_args(ytdlp))
        // The largest video stream; a combined one only if there is none.
        .args(["-f", "bv/b"])
        .arg("-o")
        .arg(&template)
        .args(["--progress", "--progress-template"])
        .arg(format!(
            "download:{PROGRESS_MARK}%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s"
        ))
        .args(["--print"])
        .arg(format!("after_move:{FILE_MARK}%(filepath)s"))
        .arg("--")
        .arg(link.url())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AtvError::DownloaderNotFound {
            detail: format!("{} could not be started ({error})", ytdlp.path.display()),
        })?;

    // Both streams are read on their own threads, so neither can fill up.
    let (sender, lines) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(forward_lines(stdout, sender.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(forward_lines(stderr, sender));
    }

    let mut file: Option<PathBuf> = None;
    let mut errors: Vec<String> = Vec::new();
    loop {
        if cancel.is_cancelled() {
            stop(&mut child);
            remove_downloads(folder, &link.id);
            return Err(AtvError::Cancelled);
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if let Some(rest) = line.trim().strip_prefix(PROGRESS_MARK) {
                    if let Some(progress) = parse_progress(rest) {
                        on_progress(progress);
                    }
                } else if let Some(path) = line.trim().strip_prefix(FILE_MARK) {
                    file = Some(PathBuf::from(path));
                } else if line.starts_with("ERROR") {
                    errors.push(line);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait()?;
    for reader in readers {
        let _ = reader.join();
    }
    if !status.success() {
        remove_downloads(folder, &link.id);
        return Err(AtvError::DownloadFailed {
            detail: last_error(&errors.join("\n")),
        });
    }
    file.filter(|path| path.is_file())
        .or_else(|| find_download(folder, &link.id))
        .ok_or_else(|| AtvError::DownloadFailed {
            detail: "yt-dlp finished but the downloaded file was not found".to_string(),
        })
}

/// Stop yt-dlp and everything it started. On Windows, the yt-dlp.exe that
/// pip installs is a small launcher that runs Python as a second process, so
/// stopping only the launcher would leave the download running.
fn stop(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = quiet(Path::new("taskkill.exe"))
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn forward_lines(
    stream: impl Read + Send + 'static,
    sender: mpsc::Sender<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    })
}

fn parse_progress(rest: &str) -> Option<DownloadProgress> {
    let mut parts = rest.split('|');
    let number = |text: Option<&str>| text.and_then(|text| text.trim().parse::<f64>().ok());
    let downloaded = number(parts.next())?;
    let total = number(parts.next()).or(number(parts.next()));
    let speed = number(parts.next());
    Some(DownloadProgress {
        downloaded: downloaded as u64,
        total: total.map(|total| total as u64),
        speed,
    })
}

fn find_download(folder: &Path, id: &str) -> Option<PathBuf> {
    fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            name.starts_with(id) && !name.ends_with(".part") && !name.ends_with(".ytdl")
        })
}

/// Remove everything yt-dlp wrote for this video, finished or not. A file
/// can stay locked for a moment after its process ends, so this tries again
/// for up to two seconds.
fn remove_downloads(folder: &Path, id: &str) {
    for _ in 0..20 {
        let mut left = false;
        if let Ok(entries) = fs::read_dir(folder) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(id)
                    && fs::remove_file(entry.path()).is_err()
                {
                    left = true;
                }
            }
        }
        if !left {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_common_link_shape_is_recognised() {
        let id = "aqz-KE-bpKQ";
        for text in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtube.com/watch?v=aqz-KE-bpKQ&t=42s",
            "http://m.youtube.com/watch?feature=share&v=aqz-KE-bpKQ",
            "www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ?si=abcdef",
            "  https://youtu.be/aqz-KE-bpKQ  ",
            "https://www.youtube.com/shorts/aqz-KE-bpKQ",
            "https://www.youtube.com/live/aqz-KE-bpKQ?feature=share",
            "https://www.youtube.com/embed/aqz-KE-bpKQ",
            "https://www.youtube-nocookie.com/embed/aqz-KE-bpKQ",
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ#comments",
        ] {
            assert_eq!(
                YouTubeLink::parse(text).map(|link| link.id),
                Some(id.to_string()),
                "{text}"
            );
        }
        assert_eq!(
            YouTubeLink::parse("https://youtu.be/aqz-KE-bpKQ")
                .unwrap()
                .url(),
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ"
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for text in [
            "",
            "hello",
            "https://vimeo.com/123456789",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ1",
            "https://www.youtube.com/playlist?list=PL123",
            "https://evil.example/youtube.com/watch?v=aqz-KE-bpKQ",
            "https://www.youtube.com.evil.example/watch?v=aqz-KE-bpKQ",
            "--exec=calc",
            "https://youtu.be/aqz-KE-bp;Q",
        ] {
            assert_eq!(YouTubeLink::parse(text), None, "{text}");
        }
    }

    #[test]
    fn details_are_read_from_yt_dlp_json() {
        let link = YouTubeLink::parse("https://youtu.be/aqz-KE-bpKQ").unwrap();
        let json = br#"{"title":"A","channel":"B","duration":12.5,"thumbnail":"https://i.ytimg.com/x.jpg",
            "formats":[{"vcodec":"none","height":null},{"vcodec":"avc1","height":720},{"vcodec":"vp9","height":2160},{"vcodec":"none","height":4320}]}"#;
        let video = parse_details(&link, json).unwrap();
        assert_eq!((video.title.as_str(), video.channel.as_str()), ("A", "B"));
        assert_eq!(video.seconds, Some(12.5));
        assert_eq!(
            video.best_height,
            Some(2160),
            "sound-only formats do not count"
        );
        assert!(parse_details(&link, b"not json").is_none());
    }

    #[test]
    fn progress_lines_are_read() {
        assert_eq!(
            parse_progress("1048576|4194304|NA|524288.5"),
            Some(DownloadProgress {
                downloaded: 1_048_576,
                total: Some(4_194_304),
                speed: Some(524_288.5)
            })
        );
        assert_eq!(
            parse_progress("2048|NA|8192|NA"),
            Some(DownloadProgress {
                downloaded: 2048,
                total: Some(8192),
                speed: None
            })
        );
        assert_eq!(parse_progress("NA|NA|NA|NA"), None);
    }

    /// Downloads a real video. Run by hand:
    /// `ATV_TEST_YOUTUBE=<link> cargo test -p atv-core youtube -- --ignored --nocapture`
    #[test]
    #[ignore = "needs yt-dlp, the internet and ATV_TEST_YOUTUBE"]
    fn downloads_a_real_video() {
        let link = std::env::var("ATV_TEST_YOUTUBE").expect("set ATV_TEST_YOUTUBE to a link");
        let link = YouTubeLink::parse(&link).expect("a YouTube link");
        let ytdlp = find_ytdlp(None, None).expect("yt-dlp installed");
        println!("{ytdlp:?}");
        if std::env::var_os("ATV_TEST_DETAILS_FIRST").is_some() {
            println!(
                "{:?}",
                video_details(&ytdlp, &link).map(|video| video.best_height)
            );
        }
        let folder = std::env::temp_dir().join(format!("atv-yt-test-{}", std::process::id()));
        let cancel = CancelToken::new();
        let mut updates = 0;
        let stop_after = std::env::var("ATV_TEST_STOP_AFTER")
            .ok()
            .and_then(|n| n.parse::<u64>().ok());
        let result = download(
            &ytdlp,
            &link,
            &folder,
            &mut |progress| {
                updates += 1;
                if stop_after.is_some_and(|limit| progress.downloaded >= limit) {
                    cancel.cancel();
                }
            },
            &cancel,
        );
        println!("{result:?} after {updates} progress updates");
        let left: Vec<_> = fs::read_dir(&folder)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        println!("left in the folder: {left:?}");
        let _ = fs::remove_dir_all(&folder);
    }

    #[test]
    fn only_a_refused_link_is_tried_again() {
        assert!(refused(
            "unable to download video data: HTTP Error 403: Forbidden"
        ));
        assert!(!refused(
            "[youtube] abc: Private video. Sign in if you've been granted access"
        ));
        assert!(!refused(
            "unable to download video data: <urlopen error timed out>"
        ));
    }

    #[test]
    fn the_useful_error_line_is_kept() {
        let stderr = "WARNING: something\nERROR: [youtube] abc: Private video. Sign in if you've been granted access\n";
        assert_eq!(
            last_error(stderr),
            "[youtube] abc: Private video. Sign in if you've been granted access"
        );
        assert_eq!(last_error(""), "yt-dlp stopped without saying why");
    }
}
