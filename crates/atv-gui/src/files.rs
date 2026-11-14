//! Looking at the files the person picked: sizes, kinds, free names.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::text;

/// One item on the Encode list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub path: String,
    pub name: String,
    /// `folder`, `archive` or `document`: which icon to show.
    pub icon: &'static str,
    /// `Folder · 214 files`, `Archive`, `PDF document`, `File`.
    pub kind: String,
    pub bytes: u64,
    pub size: String,
}

/// The last part of a path, for showing.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Describe an input, walking folders to add up their size.
pub fn describe(path: &Path) -> Item {
    let (bytes, files) = measure(path);
    let folder = path.is_dir();
    let (icon, kind) = if folder {
        (
            "folder",
            format!("Folder · {}", text::plural(files, "file")),
        )
    } else {
        kind_of(path)
    };
    Item {
        path: path.display().to_string(),
        name: display_name(path),
        icon,
        kind,
        bytes,
        size: text::bytes(bytes),
    }
}

fn kind_of(path: &Path) -> (&'static str, String) {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" => {
            ("archive", "Archive".to_string())
        }
        "pdf" => ("document", "PDF document".to_string()),
        "" => ("document", "File".to_string()),
        other => ("document", format!("{} file", other.to_uppercase())),
    }
}

/// Total bytes and file count under a path. Links inside folders are not
/// followed, matching what the engine stores.
pub fn measure(path: &Path) -> (u64, u64) {
    let Ok(metadata) = fs::metadata(path) else {
        return (0, 0);
    };
    if metadata.is_file() {
        return (metadata.len(), 1);
    }
    let mut total = (0u64, 0u64);
    let mut folders = vec![path.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                folders.push(entry.path());
            } else if kind.is_file() {
                let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                total = (total.0.saturating_add(size), total.1 + 1);
            }
        }
    }
    total
}

/// `folder/stem.ext`, or `stem (2).ext` and so on if that is taken.
pub fn free_path(folder: &Path, stem: &str, extension: &str) -> PathBuf {
    let mut candidate = folder.join(format!("{stem}.{extension}"));
    let mut counter = 2;
    while candidate.exists() {
        candidate = folder.join(format!("{stem} ({counter}).{extension}"));
        counter += 1;
    }
    candidate
}

/// The name a new video gets: after the single input, or "ATV video".
pub fn video_stem(inputs: &[PathBuf]) -> String {
    match inputs {
        [only] => only
            .file_stem()
            .or_else(|| only.file_name())
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ATV video".to_string()),
        _ => "ATV video".to_string(),
    }
}

/// A quick check before a job starts; the engine checks the key fully.
pub fn looks_like_key_file(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut first = String::new();
    BufReader::new(file.take(64))
        .read_line(&mut first)
        .is_ok_and(|_| first.trim() == "ATV-KEY-1")
}

/// A folder path written for people: `Videos › ATV` style for the home
/// folder's own folders, the full path otherwise.
pub fn friendly_folder(folder: &Path) -> String {
    let home = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    match home
        .as_deref()
        .and_then(|home| folder.strip_prefix(home).ok())
    {
        Some(inside) if !inside.as_os_str().is_empty() => inside
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" › "),
        _ => folder.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measuring_counts_files_in_folders() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/one.bin"), [0u8; 10]).unwrap();
        fs::write(dir.path().join("a/b/two.bin"), [0u8; 5]).unwrap();
        fs::write(dir.path().join("loose.pdf"), [0u8; 7]).unwrap();
        assert_eq!(measure(&dir.path().join("a")), (15, 2));
        assert_eq!(measure(&dir.path().join("missing")), (0, 0));

        let folder = describe(&dir.path().join("a"));
        assert_eq!(
            (folder.icon, folder.kind.as_str()),
            ("folder", "Folder · 2 files")
        );
        let pdf = describe(&dir.path().join("loose.pdf"));
        assert_eq!(
            (pdf.icon, pdf.kind.as_str(), pdf.bytes),
            ("document", "PDF document", 7)
        );
    }

    #[test]
    fn kinds_follow_the_extension() {
        assert_eq!(
            kind_of(Path::new("taxes.ZIP")),
            ("archive", "Archive".to_string())
        );
        assert_eq!(
            kind_of(Path::new("photo.jpg")),
            ("document", "JPG file".to_string())
        );
        assert_eq!(
            kind_of(Path::new("README")),
            ("document", "File".to_string())
        );
    }

    #[test]
    fn free_paths_and_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            free_path(dir.path(), "report", "mkv"),
            dir.path().join("report.mkv")
        );
        fs::write(dir.path().join("report.mkv"), b"").unwrap();
        fs::write(dir.path().join("report (2).mkv"), b"").unwrap();
        assert_eq!(
            free_path(dir.path(), "report", "mkv"),
            dir.path().join("report (3).mkv")
        );
        assert_eq!(
            video_stem(&[PathBuf::from("x/Family Photos")]),
            "Family Photos"
        );
        assert_eq!(
            video_stem(&[PathBuf::from("a.pdf"), PathBuf::from("b.pdf")]),
            "ATV video"
        );
    }

    #[test]
    fn key_files_are_recognised_by_their_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("k.atvkey");
        atv_core::generate_key_file(&key).unwrap();
        assert!(looks_like_key_file(&key));
        fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        assert!(!looks_like_key_file(&dir.path().join("notes.txt")));
    }

    #[test]
    fn folders_inside_home_are_written_with_arrows() {
        if let Some(dirs) = directories::UserDirs::new() {
            let videos = dirs.home_dir().join("Videos").join("ATV");
            assert_eq!(friendly_folder(&videos), "Videos › ATV");
        }
        assert_eq!(
            friendly_folder(Path::new("Z:/elsewhere")),
            Path::new("Z:/elsewhere").display().to_string()
        );
    }
}
