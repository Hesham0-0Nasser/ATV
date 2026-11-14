//! The things that differ between macOS and Windows: labels, the ffmpeg
//! install command, revealing a file, the system accent colour, and whether
//! the window may be see-through.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

/// `windows`, `macos` or `linux`.
pub fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// The label of the button that opens the file's folder.
pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Show in Folder"
    }
}

/// Where the install command is typed, and the command that installs ffmpeg.
/// The Windows package is the gyan.dev build that ffmpeg.org links to, and
/// the one this project was developed against.
pub fn install_hint() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("On macOS, with Homebrew", "brew install ffmpeg")
    } else if cfg!(windows) {
        (
            "On Windows, in Terminal",
            "winget install --id Gyan.FFmpeg -e",
        )
    } else {
        ("On Linux, in a terminal", "sudo apt install ffmpeg")
    }
}

/// A command that does not flash a console window when run from the app.
fn quiet(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

/// Open the folder that holds `path`, with `path` selected where the system
/// allows it. If `path` is gone, its folder is opened instead.
pub fn reveal(path: &Path) {
    let exists = path.exists();
    let folder = path.parent().unwrap_or(path);
    if !exists && !folder.exists() {
        return;
    }
    // Errors are ignored: the worst case is that no window opens.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = quiet("explorer.exe");
        if exists {
            // Explorer wants the quotes around the path only, after the comma.
            command.raw_arg(format!("/select,\"{}\"", path.display()));
        } else {
            command.arg(folder);
        }
        let _ = command.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = quiet("open");
        if exists {
            command.arg("-R").arg(path);
        } else {
            command.arg(folder);
        }
        let _ = command.spawn();
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = quiet("xdg-open").arg(folder).spawn();
    }
}

/// Where yt-dlp's install command is typed, and the command.
pub fn ytdlp_install_hint() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("On macOS, with Homebrew", "brew install yt-dlp")
    } else if cfg!(windows) {
        ("On Windows, in Terminal", "winget install yt-dlp.yt-dlp")
    } else {
        (
            "On Linux, in a terminal",
            "python3 -m pip install -U yt-dlp",
        )
    }
}

/// Open the ffmpeg download page in the default browser.
pub fn open_ffmpeg_site() {
    open_url("https://ffmpeg.org/download.html");
}

/// The author's page, opened from the About card and the setup wizard.
pub const AUTHOR_PAGE: &str = "https://github.com/Hesham0-0Nasser";

pub fn open_author_page() {
    open_url(AUTHOR_PAGE);
}

/// Open a web address in the default browser. Only addresses ATV built
/// itself are passed here, never text the person typed.
pub fn open_url(url: &str) {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = quiet(program).arg(url).spawn();
}

/// The system accent colour, as `#RRGGBB` for light and for dark appearance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Accent {
    pub light: String,
    pub dark: String,
}

/// What the system says about the window's look.
#[derive(Debug, Clone, Serialize)]
pub struct Look {
    /// `None` when the system accent could not be read; the page then uses blue.
    pub accent: Option<Accent>,
    /// False when the person turned transparency off, or the system cannot
    /// show the window material. The page then uses the solid colours.
    pub transparency: bool,
}

pub fn look() -> Look {
    Look {
        accent: accent(),
        transparency: transparency(),
    }
}

/// Run a command and return what it printed, if it succeeded.
fn output(command: &mut Command) -> Option<String> {
    let result = command.output().ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).into_owned())
}

/// One value from `reg query`: the last word on the line that names it.
#[cfg(windows)]
fn registry(key: &str, name: &str) -> Option<String> {
    let text = output(quiet("reg").args(["query", key, "/v", name]))?;
    parse_registry(&text, name)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_registry(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find(|line| line.split_whitespace().next() == Some(name))
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_string)
}

#[cfg(windows)]
fn accent() -> Option<Accent> {
    let palette = registry(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Accent",
        "AccentPalette",
    )?;
    accent_from_palette(&palette)
}

/// Windows keeps eight shades of the accent, each as four bytes (R, G, B, A),
/// lightest first. Its own controls use the first darker shade on light
/// backgrounds and the second lighter shade on dark ones.
#[cfg_attr(not(windows), allow(dead_code))]
fn accent_from_palette(hex: &str) -> Option<Accent> {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|at| hex.get(at..at + 2))
        .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
        .collect();
    if bytes.len() < 32 {
        return None;
    }
    let shade = |index: usize| {
        let at = index * 4;
        format!(
            "#{:02X}{:02X}{:02X}",
            bytes[at],
            bytes[at + 1],
            bytes[at + 2]
        )
    };
    Some(Accent {
        light: shade(4),
        dark: shade(1),
    })
}

#[cfg(target_os = "macos")]
fn accent() -> Option<Accent> {
    // Absent means the default (multicolour), which is blue.
    let value = output(quiet("defaults").args(["read", "-g", "AppleAccentColor"]))
        .and_then(|text| text.trim().parse::<i32>().ok());
    let (light, dark) = match value {
        Some(-1) => ("#8E8E93", "#8E8E93"),
        Some(0) => ("#FF3B30", "#FF453A"),
        Some(1) => ("#FF9500", "#FF9F0A"),
        Some(2) => ("#FFCC00", "#FFD60A"),
        Some(3) => ("#28CD41", "#32D74B"),
        Some(5) => ("#AF52DE", "#BF5AF2"),
        Some(6) => ("#FF2D55", "#FF375F"),
        _ => ("#007AFF", "#0A84FF"),
    };
    Some(Accent {
        light: light.to_string(),
        dark: dark.to_string(),
    })
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn accent() -> Option<Accent> {
    None
}

/// The Windows build number, or 0 if it cannot be read.
#[cfg(windows)]
fn windows_build() -> u32 {
    registry(
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "CurrentBuildNumber",
    )
    .and_then(|text| text.parse::<u32>().ok())
    .unwrap_or(0)
}

/// Acrylic (see-through, blurred glass) is drawn by Windows itself from
/// build 22621 on. Older builds fall back to Mica, because their Acrylic
/// stutters when the window is dragged or resized.
#[cfg(windows)]
pub fn acrylic_supported() -> bool {
    windows_build() >= 22621
}

#[cfg(not(windows))]
pub fn acrylic_supported() -> bool {
    false
}

#[cfg(windows)]
fn transparency() -> bool {
    // The window materials need Windows 11 (build 22000 or later).
    let build = windows_build();
    let enabled = registry(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "EnableTransparency",
    )
    .is_none_or(|value| value != "0x0");
    build >= 22000 && enabled
}

#[cfg(target_os = "macos")]
fn transparency() -> bool {
    let reduced =
        output(quiet("defaults").args(["read", "com.apple.universalaccess", "reduceTransparency"]))
            .is_some_and(|text| text.trim() == "1");
    !reduced
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn transparency() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_palette_gives_the_designs_blues() {
        // The default Windows 11 blue, as found in the registry.
        let palette = "99EBFF004CC2FF000091F8000078D4000067C000003E9200001A6800F7630C00";
        assert_eq!(
            accent_from_palette(palette),
            Some(Accent {
                light: "#0067C0".to_string(),
                dark: "#4CC2FF".to_string()
            })
        );
        assert_eq!(accent_from_palette("99EB"), None);
    }

    #[test]
    fn registry_output_is_parsed() {
        let text = "\r\nHKEY_CURRENT_USER\\Software\\X\r\n    EnableTransparency    REG_DWORD    0x0\r\n\r\n";
        assert_eq!(
            parse_registry(text, "EnableTransparency"),
            Some("0x0".to_string())
        );
        assert_eq!(parse_registry(text, "Missing"), None);
    }
}
