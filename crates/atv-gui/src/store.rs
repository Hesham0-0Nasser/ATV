//! Settings and history, saved as JSON in the per-user application folders
//! (spec 9.5). Nothing secret is ever stored here: no passwords, no key files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Keep at most this many history entries; the oldest are dropped first.
const HISTORY_LIMIT: usize = 500;

/// Where the two files live.
#[derive(Debug, Clone)]
pub struct Locations {
    pub settings: PathBuf,
    pub history: PathBuf,
    /// Videos downloaded from YouTube, kept only until the next download or
    /// the next start of the app.
    pub downloads: PathBuf,
    /// ATV's own copies of ffmpeg, yt-dlp and Deno (spec 9.5b).
    pub tools: PathBuf,
}

impl Locations {
    /// The system's folders for this app, or `None` if the system has no home folder.
    pub fn for_this_user() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("", "", "ATV")?;
        Some(Locations {
            settings: dirs.config_dir().join("settings.json"),
            history: dirs.data_dir().join("history.json"),
            downloads: dirs.cache_dir().join("downloads"),
            tools: dirs.data_local_dir().join("tools"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// The material behind the window (Windows). The owner asked for this
/// choice after trying Acrylic, which is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Material {
    /// See-through, blurred glass: whatever is behind the window shows.
    #[default]
    Acrylic,
    /// The blurred wallpaper's colours only, never other windows.
    Mica,
    /// No transparency: the design's solid colours.
    Solid,
}

/// Everything on the Settings screen. Missing fields in an older file take
/// their defaults, so the file can grow without breaking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub material: Material,
    /// `"1080p"` or `"4k"`.
    pub resolution: String,
    /// `"lossless"` or `"robust"`.
    pub mode: String,
    /// Robust block size: 4 or 8.
    pub cell: u32,
    /// Robust repair strength: 64 parity bytes (strong) or 32 (standard).
    /// Older settings files without it get strong.
    pub parity: u32,
    pub compress: bool,
    /// Compress harder ("Smallest video"): zstd 19 instead of 3.
    pub smallest: bool,
    /// Where the last video was made for (Smart delivery): `keep`,
    /// `whatsapp`, `youtube` or `custom`.
    pub destination: String,
    pub verify: bool,
    /// Where new videos and restored files go. `None` means the system's Videos folder.
    pub output_folder: Option<PathBuf>,
    /// An ffmpeg chosen by hand. `None` means search for it.
    pub ffmpeg_path: Option<PathBuf>,
    /// When the tools were last checked for updates (Unix seconds).
    pub last_tool_check: Option<i64>,
    /// The newest official version of each tool at that check (`ffmpeg`,
    /// `ytdlp`, `deno`), so the Tools card can say "Up to date" after a
    /// restart without asking online again.
    pub latest_tools: std::collections::BTreeMap<String, String>,
    /// The first-run setup wizard was finished or skipped (owner's request,
    /// 14 Nov 2026). Settings files from before it have `false`, so it shows once.
    pub setup_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            appearance: Appearance::System,
            material: Material::Acrylic,
            resolution: "1080p".to_string(),
            mode: "lossless".to_string(),
            cell: 8,
            parity: 64,
            compress: true,
            smallest: false,
            destination: "keep".to_string(),
            verify: true,
            output_folder: None,
            ffmpeg_path: None,
            last_tool_check: None,
            latest_tools: Default::default(),
            setup_done: false,
        }
    }
}

impl Settings {
    /// The folder to save into: the chosen one, else Videos, else the home folder.
    pub fn output_folder(&self) -> PathBuf {
        if let Some(folder) = &self.output_folder {
            return folder.clone();
        }
        directories::UserDirs::new()
            .and_then(|dirs| {
                dirs.video_dir()
                    .map(Path::to_path_buf)
                    .or_else(|| Some(dirs.home_dir().to_path_buf()))
            })
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Encode,
    Decode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Cancelled,
}

/// One past job. Holds names and numbers only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub direction: Direction,
    /// What the job was about: the video's name, or the first restored item.
    pub name: String,
    /// Seconds since 1970, UTC.
    pub date: i64,
    /// `1080p` or `4K`.
    pub resolution: String,
    /// `Lossless` or `Robust (8 × 8)`.
    pub mode: String,
    pub encrypted: bool,
    /// Size of the input (encode) or of the restored files (decode).
    pub size: u64,
    pub outcome: Outcome,
    /// What "Show in folder" reveals: the video, or the first restored item.
    pub path: Option<PathBuf>,
}

/// Read a JSON file, or `None` if it is missing or unreadable. A damaged
/// file is not fatal: the app starts with defaults and overwrites it later.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Write a JSON file so that a crash part-way never leaves a broken file:
/// write a temporary file next to it, then move it into place.
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)?;
    }
    let text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

/// Empty the downloads folder. Errors are ignored: a file still open
/// elsewhere is simply left for next time.
pub fn clear_downloads(folder: &Path) {
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn load_settings(path: &Path) -> Settings {
    read_json(path).unwrap_or_default()
}

pub fn save_settings(path: &Path, settings: &Settings) -> io::Result<()> {
    write_json(path, settings)
}

/// Newest first.
pub fn load_history(path: &Path) -> Vec<Entry> {
    read_json(path).unwrap_or_default()
}

pub fn save_history(path: &Path, entries: &[Entry]) -> io::Result<()> {
    write_json(path, &entries)
}

/// Put a new entry at the top and drop the oldest beyond the limit.
pub fn push_entry(entries: &mut Vec<Entry>, entry: Entry) {
    entries.insert(0, entry);
    entries.truncate(HISTORY_LIMIT);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            direction: Direction::Encode,
            name: name.to_string(),
            date: 1_800_000_000,
            resolution: "1080p".to_string(),
            mode: "Lossless".to_string(),
            encrypted: true,
            size: 1234,
            outcome: Outcome::Done,
            path: Some(PathBuf::from("v.mkv")),
        }
    }

    #[test]
    fn settings_round_trip_and_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        assert_eq!(load_settings(&path), Settings::default());

        let settings = Settings {
            appearance: Appearance::Dark,
            material: Material::Mica,
            resolution: "4k".to_string(),
            mode: "robust".to_string(),
            cell: 4,
            parity: 32,
            compress: false,
            smallest: true,
            destination: "youtube".to_string(),
            verify: false,
            output_folder: Some(PathBuf::from("D:/out")),
            ffmpeg_path: Some(PathBuf::from("C:/ffmpeg/bin/ffmpeg.exe")),
            last_tool_check: Some(1_794_560_000),
            latest_tools: [("ffmpeg".to_string(), "9.0.2".to_string())].into(),
            setup_done: true,
        };
        save_settings(&path, &settings).unwrap();
        assert_eq!(load_settings(&path), settings);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn an_old_or_broken_settings_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"appearance":"light"}"#).unwrap();
        let loaded = load_settings(&path);
        assert_eq!(loaded.appearance, Appearance::Light);
        assert!(
            !loaded.setup_done,
            "an older settings file shows the setup wizard once"
        );
        assert!(
            loaded.compress && loaded.verify,
            "missing fields take defaults"
        );
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_settings(&path), Settings::default());
    }

    #[test]
    fn history_round_trips_newest_first_and_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        assert!(load_history(&path).is_empty());

        let mut entries = Vec::new();
        for i in 0..HISTORY_LIMIT + 5 {
            push_entry(&mut entries, entry(&format!("job {i}")));
        }
        assert_eq!(entries.len(), HISTORY_LIMIT);
        assert_eq!(entries[0].name, format!("job {}", HISTORY_LIMIT + 4));
        save_history(&path, &entries).unwrap();
        assert_eq!(load_history(&path), entries);
    }

    #[test]
    fn nothing_secret_is_in_the_history_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        save_history(&path, &[entry("holiday.mkv")]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for field in ["password", "key", "secret"] {
            assert!(!text.to_lowercase().contains(field), "{field} in {text}");
        }
    }
}
