//! Keeping ATV's helper programs present and up to date (owner's request,
//! 13 Nov 2026; spec section 9.5b).
//!
//! ATV needs ffmpeg (always), yt-dlp (for YouTube links) and a JavaScript
//! runtime for yt-dlp (Deno, or a Node.js or Bun the person already has).
//! This module asks each project's **official** release channel for its
//! newest version and installs it into ATV's own tools folder:
//!
//! - ffmpeg (Windows): the gyan.dev "release essentials" build, the Windows
//!   build ffmpeg.org links to, checked against gyan.dev's SHA-256 file.
//! - yt-dlp: the single-file program from the yt-dlp GitHub release,
//!   checked against the release's `SHA2-256SUMS`.
//! - Deno: the zip from the Deno GitHub release, checked against its
//!   `.sha256sum` file.
//!
//! Nothing is run until its checksum matches, files are written under a
//! temporary name and moved into place only when complete, and the new
//! program must report its version before the install counts as done.
//! ffmpeg on macOS and Linux is left to the system's package manager, since
//! ffmpeg.org links no official build with a checksum for those.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::AtvError;
use crate::types::CancelToken;

/// The helper programs ATV can install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Ffmpeg,
    YtDlp,
    Deno,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Ffmpeg, Tool::YtDlp, Tool::Deno];

    /// The name people know it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::YtDlp => "yt-dlp",
            Tool::Deno => "Deno",
        }
    }

    /// The project's own download page, for people who want to look.
    pub fn official_page(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "https://ffmpeg.org/download.html",
            Tool::YtDlp => "https://github.com/yt-dlp/yt-dlp/releases/latest",
            Tool::Deno => "https://github.com/denoland/deno/releases/latest",
        }
    }

    /// The program files this tool puts in the tools folder.
    pub fn programs(self) -> &'static [&'static str] {
        match self {
            Tool::Ffmpeg => &["ffmpeg", "ffprobe"],
            Tool::YtDlp => &["yt-dlp"],
            Tool::Deno => &["deno"],
        }
    }

    /// Whether ATV can install this tool on this system itself.
    pub fn installable_here(self) -> bool {
        match self {
            Tool::Ffmpeg => cfg!(windows) && cfg!(target_arch = "x86_64"),
            Tool::YtDlp => cfg!(windows) || cfg!(target_os = "macos"),
            Tool::Deno => deno_target().is_some(),
        }
    }
}

/// A program file name on this system: `ffmpeg.exe` on Windows.
pub fn program_file(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// ATV's own copy of a tool's main program in `folder`, if there is one.
pub fn managed_program(folder: &Path, tool: Tool) -> Option<PathBuf> {
    let main = tool.programs().first()?;
    let path = folder.join(program_file(main));
    path.is_file().then_some(path)
}

/// The newest official release of a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tool: Tool,
    /// As the project writes it: `9.0.2`, `2026.10.26`, `2.9.8`.
    pub version: String,
    /// The file to download.
    pub url: String,
    /// The project's checksum file for it.
    pub checksum_url: String,
    /// The download's name, to find its line in a checksum list.
    pub file_name: String,
}

const USER_AGENT: &str = concat!("ATV/", env!("CARGO_PKG_VERSION"));
/// No official download is anywhere near this; a bigger answer is refused.
const MAX_DOWNLOAD: u64 = 600 * 1024 * 1024;

fn agent(follow_redirects: bool, overall: Option<Duration>) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(overall)
        .max_redirects(if follow_redirects { 10 } else { 0 })
        .http_status_as_error(follow_redirects)
        .build();
    ureq::Agent::new_with_config(config)
}

fn net_error(what: &str, error: ureq::Error) -> AtvError {
    AtvError::ToolInstallFailed {
        detail: format!("{what}: {error}"),
    }
}

/// A short text file, such as a version number or a checksum list.
fn fetch_text(url: &str) -> Result<String, AtvError> {
    let mut response = agent(true, Some(Duration::from_secs(60)))
        .get(url)
        .call()
        .map_err(|error| net_error(&format!("could not read {url}"), error))?;
    response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|error| net_error(&format!("could not read {url}"), error))
}

/// The tag GitHub's "latest release" link points to, without the API (which
/// limits how often it may be asked).
fn latest_github_tag(repo: &str) -> Result<String, AtvError> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let response = agent(false, Some(Duration::from_secs(60)))
        .get(&url)
        .call()
        .map_err(|error| net_error(&format!("could not ask GitHub about {repo}"), error))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    location
        .rsplit_once("/releases/tag/")
        .map(|(_, tag)| tag.trim().to_string())
        .filter(|tag| !tag.is_empty() && tag.len() < 64 && !tag.contains('/'))
        .ok_or_else(|| AtvError::ToolInstallFailed {
            detail: format!("GitHub did not say which {repo} release is the latest"),
        })
}

/// Deno's name for this system, such as `x86_64-pc-windows-msvc`.
fn deno_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// Ask the tool's official release channel for its newest version.
/// `Ok(None)` means ATV does not install this tool on this system.
pub fn latest_release(tool: Tool) -> Result<Option<Release>, AtvError> {
    if !tool.installable_here() {
        return Ok(None);
    }
    let release = match tool {
        Tool::Ffmpeg => {
            let version = fetch_text("https://www.gyan.dev/ffmpeg/builds/release-version")?
                .trim()
                .to_string();
            if version.is_empty()
                || version.len() > 32
                || !version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return Err(AtvError::ToolInstallFailed {
                    detail: "gyan.dev gave an unexpected version number".to_string(),
                });
            }
            let file_name = format!("ffmpeg-{version}-essentials_build.zip");
            let url = format!("https://www.gyan.dev/ffmpeg/builds/packages/{file_name}");
            Release {
                tool,
                version,
                checksum_url: format!("{url}.sha256"),
                url,
                file_name,
            }
        }
        Tool::YtDlp => {
            let tag = latest_github_tag("yt-dlp/yt-dlp")?;
            let file_name = if cfg!(windows) {
                "yt-dlp.exe"
            } else {
                "yt-dlp_macos"
            }
            .to_string();
            let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{tag}");
            Release {
                tool,
                url: format!("{base}/{file_name}"),
                checksum_url: format!("{base}/SHA2-256SUMS"),
                version: tag,
                file_name,
            }
        }
        Tool::Deno => {
            let tag = latest_github_tag("denoland/deno")?;
            let target = deno_target().unwrap_or_default();
            let file_name = format!("deno-{target}.zip");
            let url =
                format!("https://github.com/denoland/deno/releases/download/{tag}/{file_name}");
            Release {
                tool,
                version: tag.trim_start_matches('v').to_string(),
                checksum_url: format!("{url}.sha256sum"),
                url,
                file_name,
            }
        }
    };
    Ok(Some(release))
}

/// The SHA-256 for `file_name` in a checksum file. Handles the three forms
/// the projects use: a bare hash (gyan.dev), `hash  name` lines (yt-dlp,
/// Deno on macOS), and PowerShell's `Hash : HASH` listing (Deno on Windows).
pub fn expected_sha256(text: &str, file_name: &str) -> Option<String> {
    let is_hash = |word: &str| word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit());
    let named = text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let hash = words.next()?;
        let name = words.next()?.trim_start_matches('*');
        (is_hash(hash) && name == file_name).then(|| hash.to_ascii_lowercase())
    });
    if named.is_some() {
        return named;
    }
    // A file for a single download: the only hash in it.
    let hashes: Vec<&str> = text
        .split(|c: char| !c.is_ascii_hexdigit())
        .filter(|word| is_hash(word))
        .collect();
    match hashes.as_slice() {
        [only] => Some(only.to_ascii_lowercase()),
        _ => None,
    }
}

/// The version numbers in a version text, for comparing: `9.0.2-essentials`
/// gives `[9, 0, 2]`, `2026.10.26` gives `[2026, 10, 26]`, `v25.2.1` gives
/// `[25, 2, 1]`. `None` for builds without a release number, such as
/// ffmpeg's `N-121000-g…` snapshots.
pub fn version_numbers(text: &str) -> Option<Vec<u64>> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let leading: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let numbers: Vec<u64> = leading
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    (!numbers.is_empty()).then_some(numbers)
}

/// Whether `latest` is newer than `installed`. `None` when either cannot be
/// read as a release number.
pub fn is_newer(latest: &str, installed: &str) -> Option<bool> {
    let latest = version_numbers(latest)?;
    let installed = version_numbers(installed)?;
    let length = latest.len().max(installed.len());
    let pad = |mut numbers: Vec<u64>| {
        numbers.resize(length, 0);
        numbers
    };
    Some(pad(latest) > pad(installed))
}

/// Download a release, check it against the project's checksum, and put its
/// programs in `folder`. Returns the main program's path.
///
/// `on_progress(downloaded, total)` is called as bytes arrive. Cancelling
/// stops the download and removes everything written.
pub fn install(
    release: &Release,
    folder: &Path,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &CancelToken,
) -> Result<PathBuf, AtvError> {
    fs::create_dir_all(folder)?;
    let partial = folder.join(format!(".{}.download", release.file_name));
    let result = install_from(release, folder, &partial, on_progress, cancel);
    let _ = fs::remove_file(&partial);
    if result.is_err() {
        for program in release.tool.programs() {
            let _ = fs::remove_file(folder.join(format!(".{}.new", program_file(program))));
        }
    }
    result
}

fn install_from(
    release: &Release,
    folder: &Path,
    partial: &Path,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &CancelToken,
) -> Result<PathBuf, AtvError> {
    let checksums = fetch_text(&release.checksum_url)?;
    let expected = expected_sha256(&checksums, &release.file_name).ok_or_else(|| {
        AtvError::ToolInstallFailed {
            detail: format!(
                "the official checksum for {} was not found",
                release.file_name
            ),
        }
    })?;

    let actual = download(&release.url, partial, on_progress, cancel)?;
    if actual != expected {
        return Err(AtvError::ToolInstallFailed {
            detail: format!(
                "the download of {} did not match its official checksum, so it was deleted",
                release.file_name
            ),
        });
    }

    // Write each program under a temporary name first.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    if release.file_name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(File::open(partial)?).map_err(|error| {
            AtvError::ToolInstallFailed {
                detail: format!("{} could not be opened: {error}", release.file_name),
            }
        })?;
        for program in release.tool.programs() {
            let wanted = program_file(program);
            let index = (0..archive.len()).find(|&index| {
                archive
                    .by_index(index)
                    .ok()
                    .and_then(|entry| entry.enclosed_name())
                    .and_then(|path| path.file_name().map(|name| name.to_os_string()))
                    .is_some_and(|name| name == wanted.as_str())
            });
            let index = index.ok_or_else(|| AtvError::ToolInstallFailed {
                detail: format!("{wanted} was not in {}", release.file_name),
            })?;
            let mut entry =
                archive
                    .by_index(index)
                    .map_err(|error| AtvError::ToolInstallFailed {
                        detail: format!("{wanted} could not be read: {error}"),
                    })?;
            let temporary = folder.join(format!(".{wanted}.new"));
            let mut out = File::create(&temporary)?;
            io::copy(&mut entry, &mut out)?;
            out.sync_all()?;
            staged.push((temporary, folder.join(&wanted)));
        }
    } else {
        let wanted = program_file(release.tool.programs().first().copied().unwrap_or("tool"));
        let temporary = folder.join(format!(".{wanted}.new"));
        fs::copy(partial, &temporary)?;
        staged.push((temporary, folder.join(&wanted)));
    }

    for (temporary, _) in &staged {
        make_runnable(temporary)?;
    }
    // The new program must run before it replaces anything.
    let check = staged
        .first()
        .map(|(temporary, _)| temporary.clone())
        .ok_or_else(|| AtvError::ToolInstallFailed {
            detail: "nothing was installed".to_string(),
        })?;
    if program_version(release.tool, &check).is_none() {
        return Err(AtvError::ToolInstallFailed {
            detail: format!("the new {} did not start", release.tool.name()),
        });
    }
    for (temporary, target) in &staged {
        if let Err(error) = replace(temporary, target) {
            return Err(AtvError::ToolInstallFailed {
                detail: format!(
                    "{} could not be replaced; close anything that is using it and try again ({error})",
                    target.display()
                ),
            });
        }
    }
    staged
        .first()
        .map(|(_, target)| target.clone())
        .ok_or_else(|| AtvError::ToolInstallFailed {
            detail: "nothing was installed".to_string(),
        })
}

/// Stream a download to `path`, hashing as it goes. Returns the SHA-256.
fn download(
    url: &str,
    path: &Path,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &CancelToken,
) -> Result<String, AtvError> {
    let response = agent(true, None)
        .get(url)
        .call()
        .map_err(|error| net_error(&format!("could not download {url}"), error))?;
    let total = response.body().content_length();
    if total.is_some_and(|total| total > MAX_DOWNLOAD) {
        return Err(AtvError::ToolInstallFailed {
            detail: format!("{url} is far larger than expected"),
        });
    }
    let mut reader = response.into_body().into_reader();
    let mut file = File::create(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(AtvError::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| AtvError::ToolInstallFailed {
                detail: format!("the download stopped: {error}"),
            })?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).unwrap_or_default();
        hasher.update(chunk);
        file.write_all(chunk)?;
        done += read as u64;
        if done > MAX_DOWNLOAD {
            return Err(AtvError::ToolInstallFailed {
                detail: format!("{url} is far larger than expected"),
            });
        }
        on_progress(done, total);
    }
    file.sync_all()?;
    if total.is_some_and(|total| total != done) {
        return Err(AtvError::ToolInstallFailed {
            detail: "the download ended early".to_string(),
        });
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(unix)]
fn make_runnable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_runnable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Move `temporary` over `target`. Windows will not rename over a file in
/// use, so the old file is moved aside first and removed afterwards.
fn replace(temporary: &Path, target: &Path) -> io::Result<()> {
    if target.exists() {
        let old = target.with_extension("old");
        let _ = fs::remove_file(&old);
        fs::rename(target, &old)?;
        if let Err(error) = fs::rename(temporary, target) {
            let _ = fs::rename(&old, target);
            return Err(error);
        }
        let _ = fs::remove_file(&old);
        Ok(())
    } else {
        fs::rename(temporary, target)
    }
}

/// The version a program reports, such as `9.0.2-essentials_build-www.gyan.dev`
/// for ffmpeg, `2026.10.26` for yt-dlp or `2.9.8` for Deno. `None` if it does
/// not run.
pub fn program_version(tool: Tool, program: &Path) -> Option<String> {
    let flag = match tool {
        Tool::Ffmpeg => "-version",
        Tool::YtDlp | Tool::Deno => "--version",
    };
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command
        .arg(flag)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next()?.trim();
    let version = match tool {
        // "ffmpeg version 9.0.2-essentials_build-www.gyan.dev Copyright …"
        Tool::Ffmpeg => first
            .strip_prefix("ffmpeg version ")?
            .split_whitespace()
            .next()?,
        Tool::YtDlp => first,
        // "deno 2.9.8 (stable, release, x86_64-pc-windows-msvc)"
        Tool::Deno => first.strip_prefix("deno ")?.split_whitespace().next()?,
    };
    Some(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_read_in_every_official_form() {
        let hash = "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba";
        // gyan.dev: the hash alone.
        assert_eq!(
            expected_sha256(&format!("{hash}\n"), "ffmpeg.zip").as_deref(),
            Some(hash)
        );
        // yt-dlp: a list, one line per file.
        let list = format!(
            "{} yt-dlp\n{hash}  yt-dlp.exe\n{}  yt-dlp_macos\n",
            "1".repeat(64),
            "2".repeat(64)
        );
        assert_eq!(expected_sha256(&list, "yt-dlp.exe").as_deref(), Some(hash));
        assert_eq!(
            expected_sha256(&list, "yt-dlp_x86.exe"),
            None,
            "no guessing in a list"
        );
        // Deno on Windows: PowerShell's listing, upper case.
        let powershell = format!(
            "\nAlgorithm : SHA256\nHash      : {}\nPath      : C:\\a\\deno.zip\n",
            hash.to_uppercase()
        );
        assert_eq!(
            expected_sha256(&powershell, "deno-x86_64-pc-windows-msvc.zip").as_deref(),
            Some(hash)
        );
        assert_eq!(expected_sha256("not a checksum", "x"), None);
    }

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(
            version_numbers("9.0.2-essentials_build-www.gyan.dev"),
            Some(vec![9, 0, 2])
        );
        assert_eq!(
            version_numbers("8.1-full_build-www.gyan.dev"),
            Some(vec![8, 1])
        );
        assert_eq!(version_numbers("v25.2.1"), Some(vec![25, 2, 1]));
        assert_eq!(version_numbers("N-121000-g1234abcd"), None);
        assert_eq!(is_newer("9.0.2", "8.1-full_build-www.gyan.dev"), Some(true));
        assert_eq!(is_newer("8.1", "8.1-full_build-www.gyan.dev"), Some(false));
        assert_eq!(is_newer("8.1.0", "8.1"), Some(false));
        assert_eq!(is_newer("2026.10.26", "2026.07.04"), Some(true));
        assert_eq!(is_newer("2026.10.26", "2026.10.26"), Some(false));
        assert_eq!(is_newer("2.9.8", "2.10.0"), Some(false));
        assert_eq!(is_newer("9.0", "N-121000-g1234abcd"), None);
    }

    #[test]
    fn replacing_keeps_the_old_file_until_the_new_one_is_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("tool.exe");
        let temporary = dir.path().join(".tool.exe.new");
        fs::write(&target, b"old").unwrap();
        fs::write(&temporary, b"new").unwrap();
        replace(&temporary, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!temporary.exists());
        assert!(!dir.path().join("tool.old").exists());
    }

    /// Asks the real release channels. Run by hand:
    /// `cargo test -p atv-core tools -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the internet"]
    fn the_official_channels_answer() {
        for tool in Tool::ALL {
            println!("{tool:?}: {:?}", latest_release(tool));
        }
    }

    /// Installs every tool ATV can install here into a temporary folder.
    /// Run by hand: `cargo test -p atv-core installs_every_tool -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the internet; downloads about 180 MB"]
    fn installs_every_tool() {
        for tool in Tool::ALL {
            let Some(release) = latest_release(tool).unwrap() else {
                println!("{tool:?}: not installed by ATV on this system");
                continue;
            };
            let dir = tempfile::tempdir().unwrap();
            let mut last = 0;
            let path = install(
                &release,
                dir.path(),
                &mut |done, _| last = done,
                &CancelToken::new(),
            )
            .unwrap();
            let version = program_version(tool, &path);
            println!("{tool:?}: {last} bytes, {version:?}");
            assert_eq!(
                is_newer(&release.version, version.as_deref().unwrap()),
                Some(false)
            );
            for program in tool.programs() {
                assert!(
                    dir.path().join(program_file(program)).is_file(),
                    "{program}"
                );
            }
            let left: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(
                left.len(),
                tool.programs().len(),
                "only the programs are left: {left:?}"
            );
        }
    }
}
