//! The tar layer (spec 4.1 and 4.2): packing the inputs into one stream on
//! encode, and unpacking that stream safely on decode.
//!
//! Every input is stored under its own base name, so the name always travels
//! with the data. Symbolic links are never stored and never created.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::AtvError;
use crate::progress::Ctx;
use crate::types::RestoredFile;

/// Buffer used when writing restored files.
const COPY_BUFFER: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
}

/// One file or folder that will go into the archive.
#[derive(Debug, Clone)]
pub(crate) struct PlanEntry {
    /// Where it is on disk.
    pub source: PathBuf,
    /// Its path inside the archive, always relative.
    pub name: PathBuf,
    pub kind: Kind,
    pub size: u64,
    /// Modification time, seconds since 1970.
    pub mtime: u64,
    /// Unix permission bits.
    pub mode: u32,
}

/// Everything that will be archived, listed before any data is read so the
/// total size is known for the progress bar.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub entries: Vec<PlanEntry>,
    pub total_bytes: u64,
    pub file_count: u64,
    /// Symbolic links found inside folders; they are left out.
    pub skipped_links: u64,
}

/// List the inputs. Folders are walked in name order so the same inputs
/// always give the same archive.
pub(crate) fn plan(inputs: &[PathBuf], ctx: &Ctx) -> Result<Plan, AtvError> {
    if inputs.is_empty() {
        return Err(AtvError::invalid(
            "nothing to encode: add a file or a folder",
        ));
    }
    let mut plan = Plan::default();
    let mut used_names: HashSet<String> = HashSet::new();
    for input in inputs {
        ctx.check_cancel()?;
        // A top-level input that is a link is followed: the person picked it.
        let metadata = fs::metadata(input).map_err(|error| unreadable(input, &error))?;
        let name = unique_top_name(&base_name(input)?, &mut used_names);
        let name = PathBuf::from(name);
        if metadata.is_dir() {
            add_dir(&mut plan, input, name, &metadata, ctx)?;
        } else if metadata.is_file() {
            add_file(&mut plan, input, name, &metadata);
        } else {
            return Err(AtvError::invalid(format!(
                "{} is not a file or a folder",
                input.display()
            )));
        }
    }
    Ok(plan)
}

fn unreadable(path: &Path, error: &io::Error) -> AtvError {
    AtvError::invalid(format!("cannot read {}: {error}", path.display()))
}

/// The last part of a path. For paths such as `.` the real location is looked up first.
fn base_name(input: &Path) -> Result<OsString, AtvError> {
    if let Some(name) = input.file_name() {
        return Ok(name.to_os_string());
    }
    let resolved = fs::canonicalize(input).map_err(|error| unreadable(input, &error))?;
    resolved
        .file_name()
        .map(OsStr::to_os_string)
        .ok_or_else(|| {
            AtvError::invalid(format!(
                "{} is a whole drive; choose a folder inside it instead",
                input.display()
            ))
        })
}

/// Two inputs may share a base name (`a/notes.txt` and `b/notes.txt`). The
/// second becomes `notes (2).txt`. Names are compared without case because
/// Windows and macOS file systems ignore it.
fn unique_top_name(name: &OsStr, used: &mut HashSet<String>) -> OsString {
    let mut candidate = name.to_os_string();
    let mut counter = 2u32;
    while !used.insert(candidate.to_string_lossy().to_lowercase()) {
        candidate = numbered_name(name, counter);
        counter += 1;
    }
    candidate
}

/// `report.pdf` with 2 becomes `report (2).pdf`.
fn numbered_name(name: &OsStr, counter: u32) -> OsString {
    let path = Path::new(name);
    let mut numbered = path.file_stem().unwrap_or(name).to_os_string();
    numbered.push(format!(" ({counter})"));
    if let Some(extension) = path.extension() {
        numbered.push(".");
        numbered.push(extension);
    }
    numbered
}

fn add_file(plan: &mut Plan, source: &Path, name: PathBuf, metadata: &fs::Metadata) {
    plan.total_bytes = plan.total_bytes.saturating_add(metadata.len());
    plan.file_count += 1;
    plan.entries.push(PlanEntry {
        source: source.to_path_buf(),
        name,
        kind: Kind::File,
        size: metadata.len(),
        mtime: mtime_of(metadata),
        mode: mode_of(metadata, 0o644),
    });
}

fn add_dir(
    plan: &mut Plan,
    source: &Path,
    name: PathBuf,
    metadata: &fs::Metadata,
    ctx: &Ctx,
) -> Result<(), AtvError> {
    ctx.check_cancel()?;
    plan.entries.push(PlanEntry {
        source: source.to_path_buf(),
        name: name.clone(),
        kind: Kind::Dir,
        size: 0,
        mtime: mtime_of(metadata),
        mode: mode_of(metadata, 0o755),
    });
    let mut children = Vec::new();
    for child in fs::read_dir(source).map_err(|error| unreadable(source, &error))? {
        children.push(child.map_err(|error| unreadable(source, &error))?);
    }
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let path = child.path();
        // `file_type` does not follow links, so a link is seen as a link.
        let file_type = child
            .file_type()
            .map_err(|error| unreadable(&path, &error))?;
        if file_type.is_symlink() {
            plan.skipped_links += 1;
            continue;
        }
        let child_name = name.join(child.file_name());
        let child_metadata = child
            .metadata()
            .map_err(|error| unreadable(&path, &error))?;
        if file_type.is_dir() {
            add_dir(plan, &path, child_name, &child_metadata, ctx)?;
        } else if file_type.is_file() {
            add_file(plan, &path, child_name, &child_metadata);
        }
        // Anything else (sockets, devices) is not data and is left out.
    }
    Ok(())
}

fn mtime_of(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs())
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata, _fallback: u32) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata, fallback: u32) -> u32 {
    fallback
}

/// Write the planned entries as a tar stream into `out`, reading each file
/// in small pieces. Returns `out` once the archive is complete.
pub(crate) fn write_tar<W: Write>(plan: &Plan, out: W, ctx: &Ctx) -> Result<W, AtvError> {
    let mut builder = tar::Builder::new(out);
    for entry in &plan.entries {
        ctx.check_cancel()?;
        let mut header = tar::Header::new_gnu();
        header.set_mtime(entry.mtime);
        header.set_mode(entry.mode);
        header.set_uid(0);
        header.set_gid(0);
        match entry.kind {
            Kind::Dir => {
                header.set_entry_type(tar::EntryType::dir());
                header.set_size(0);
                builder.append_data(&mut header, &entry.name, io::empty())?;
            }
            Kind::File => {
                let file =
                    File::open(&entry.source).map_err(|error| unreadable(&entry.source, &error))?;
                header.set_entry_type(tar::EntryType::file());
                header.set_size(entry.size);
                let reader = SourceReader {
                    file,
                    remaining: entry.size,
                    path: &entry.source,
                    ctx,
                };
                builder.append_data(&mut header, &entry.name, reader)?;
            }
        }
    }
    Ok(builder.into_inner()?)
}

/// Reads exactly the number of bytes the tar header promised, counting
/// progress and noticing cancellation as it goes.
struct SourceReader<'a, 'c> {
    file: File,
    remaining: u64,
    path: &'a Path,
    ctx: &'a Ctx<'c>,
}

impl Read for SourceReader<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        self.ctx.check_cancel()?;
        let want = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let count = self.file.read(&mut buffer[..want])?;
        if count == 0 {
            // The file is shorter than when it was listed. A tar entry cannot
            // change size after its header is written, so stop here.
            return Err(self.ctx.fail(AtvError::invalid(format!(
                "{} changed while it was being read",
                self.path.display()
            ))));
        }
        self.remaining -= count as u64;
        self.ctx.add_bytes(count as u64);
        Ok(count)
    }
}

/// What came out of an archive.
#[derive(Debug, Default)]
pub(crate) struct Extracted {
    /// Every restored file, with its path relative to the destination.
    pub files: Vec<RestoredFile>,
    pub total_bytes: u64,
    /// Entries that were not restored: links and special files.
    pub skipped: u64,
}

/// Unpack a tar stream into `dest`, which must be an empty folder.
///
/// Safety rules: an entry with an absolute path or a `..` part is refused, a
/// link that points outside `dest` is refused, no link is ever created, and
/// no existing file is ever replaced.
pub(crate) fn extract<R: Read>(reader: R, dest: &Path, ctx: &Ctx) -> Result<Extracted, AtvError> {
    let mut extracted = Extracted::default();
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut archive = tar::Archive::new(reader);
    // The archive not parsing means the data is not what ATV wrote. An
    // earlier failure (damage, cancel) is kept, as `fail` keeps the first.
    let corrupt = |_: io::Error| ctx.fail(AtvError::Corrupt);
    for entry in archive.entries().map_err(corrupt)? {
        ctx.check_cancel()?;
        let mut entry = entry.map_err(corrupt)?;
        let raw_path = entry.path().map_err(corrupt)?.into_owned();
        let relative = safe_relative(&raw_path)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(dest.join(&relative))?;
        } else if kind.is_file() || kind.is_contiguous() {
            let target = dest.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            // `create_new` fails instead of replacing a file that is already there.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            let mut size = 0u64;
            loop {
                ctx.check_cancel()?;
                let count = entry.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                file.write_all(&buffer[..count])?;
                size += count as u64;
                ctx.add_bytes(count as u64);
            }
            restore_metadata(&file, entry.header());
            extracted.total_bytes = extracted.total_bytes.saturating_add(size);
            extracted.files.push(RestoredFile {
                path: relative,
                size,
            });
        } else if kind.is_symlink() || kind.is_hard_link() {
            if let Some(link_target) = entry.link_name().map_err(corrupt)? {
                check_link(&raw_path, &relative, &link_target, kind.is_hard_link())?;
            }
            extracted.skipped += 1;
        } else {
            extracted.skipped += 1;
        }
    }
    Ok(extracted)
}

/// Put back the modification time and, on Unix, the permission bits. These
/// are extras: failing to set them does not fail the restore.
fn restore_metadata(file: &File, header: &tar::Header) {
    if let Ok(seconds) = header.mtime()
        && seconds > 0
    {
        let _ = file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
    }
    #[cfg(unix)]
    if let Ok(mode) = header.mode() {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(mode & 0o777));
    }
}

fn unsafe_path(path: &Path) -> AtvError {
    AtvError::UnsafePath {
        path: path.display().to_string(),
    }
}

/// Check an archive path and rebuild it from its plain parts. Anything that
/// could leave the destination folder is refused.
pub(crate) fn safe_relative(raw: &Path) -> Result<PathBuf, AtvError> {
    let mut clean = PathBuf::new();
    for part in raw.components() {
        match part {
            Component::Normal(name) => {
                // On Windows a colon inside a name addresses a hidden stream of another file.
                if cfg!(windows) && name.to_string_lossy().contains(':') {
                    return Err(unsafe_path(raw));
                }
                clean.push(name);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path(raw));
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(unsafe_path(raw));
    }
    Ok(clean)
}

/// Refuse a link whose target is outside the destination folder. A symbolic
/// link's target is relative to the folder the link is in; a hard link's
/// target is relative to the top of the archive.
fn check_link(raw: &Path, relative: &Path, target: &Path, hard: bool) -> Result<(), AtvError> {
    let mut depth: i64 = if hard {
        0
    } else {
        relative.components().count() as i64 - 1
    };
    for part in target.components() {
        match part {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(unsafe_path(raw));
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(unsafe_path(raw)),
        }
    }
    Ok(())
}

/// A private folder inside the output folder where files are restored first.
///
/// Nothing appears under its final name until the whole video has been read
/// and its checksum has matched. If the job fails or is cancelled, dropping
/// this removes everything that was written.
pub(crate) struct Staging {
    dir: PathBuf,
    /// Set when this job created the output folder itself, so a failed job
    /// can take it away again.
    created_output: Option<PathBuf>,
}

impl Staging {
    /// Create the output folder if needed and a fresh staging folder inside it.
    pub(crate) fn create(output_dir: &Path) -> io::Result<Self> {
        let existed = output_dir.is_dir();
        fs::create_dir_all(output_dir)?;
        let created_output = (!existed).then(|| output_dir.to_path_buf());
        let process = std::process::id();
        for attempt in 0..1000u32 {
            let dir = output_dir.join(format!(".atv-partial-{process}-{attempt}"));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    return Ok(Staging {
                        dir,
                        created_output,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temporary folder in the output folder",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// Move everything into the output folder. An item whose name is already
    /// taken there gets a numbered name instead; nothing is overwritten.
    /// Returns each top-level item as (name in the archive, final name).
    pub(crate) fn commit(mut self, output_dir: &Path) -> io::Result<Vec<(OsString, OsString)>> {
        let mut names = Vec::new();
        for item in fs::read_dir(&self.dir)? {
            names.push(item?.file_name());
        }
        names.sort();
        let mut moved = Vec::with_capacity(names.len());
        for name in names {
            let final_name = free_name(output_dir, &name);
            fs::rename(self.dir.join(&name), output_dir.join(&final_name))?;
            // Something of ours is in the output folder now, so it stays.
            self.created_output = None;
            moved.push((name, final_name));
        }
        self.created_output = None;
        Ok(moved)
        // Dropping `self` removes the now empty staging folder.
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        if let Some(output_dir) = &self.created_output {
            // Only succeeds if the folder is empty, which is what we want.
            let _ = fs::remove_dir(output_dir);
        }
    }
}

/// `name` if nothing in `dir` has it, otherwise the first free numbered name.
fn free_name(dir: &Path, name: &OsStr) -> OsString {
    let mut candidate = name.to_os_string();
    let mut counter = 2u32;
    // `symlink_metadata` also sees a broken link, which `exists` would miss.
    while fs::symlink_metadata(dir.join(&candidate)).is_ok() {
        candidate = numbered_name(name, counter);
        counter += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CancelToken, Event, Stage};

    /// Run `body` with a job context that ignores events.
    fn with_ctx<T>(body: impl FnOnce(&Ctx) -> T) -> T {
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::Preparing);
        body(&ctx)
    }

    fn try_extract(tar_bytes: &[u8], dest: &Path) -> Result<Extracted, AtvError> {
        with_ctx(|ctx| extract(tar_bytes, dest, ctx).map_err(|error| ctx.resolve(error)))
    }

    /// Build a tar with one hand-made entry, bypassing the checks a normal
    /// tar writer applies, the way a hostile archive would.
    fn raw_tar(path: &[u8], kind: tar::EntryType, link: Option<&[u8]>, data: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        {
            let old = header.as_old_mut();
            old.name[..path.len()].copy_from_slice(path);
            if let Some(link) = link {
                old.linkname[..link.len()].copy_from_slice(link);
            }
        }
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, data).unwrap();
        builder.into_inner().unwrap()
    }

    fn assert_unsafe(result: Result<Extracted, AtvError>) {
        assert!(
            matches!(result, Err(AtvError::UnsafePath { .. })),
            "expected UnsafePath, got {result:?}"
        );
    }

    /// Nothing may be written outside `dest`: its parent must hold only `dest`.
    fn assert_nothing_escaped(parent: &Path) {
        let names: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("out")]);
    }

    #[test]
    fn a_tree_round_trips_through_tar() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("project");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("readme.txt"), b"hello").unwrap();
        fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("src/deep/zero.bin"), b"").unwrap();
        let single = source.path().join("photo.jpg");
        fs::write(&single, [1u8, 2, 3, 4]).unwrap();

        let dest = tempfile::tempdir().unwrap();
        let extracted = with_ctx(|ctx| {
            let plan = plan(&[root.clone(), single.clone()], ctx).unwrap();
            assert_eq!(plan.total_bytes, 5 + 12 + 4);
            assert_eq!(plan.file_count, 4);
            let tar_bytes = write_tar(&plan, Vec::new(), ctx).unwrap();
            extract(&tar_bytes[..], dest.path(), ctx).unwrap()
        });

        assert_eq!(extracted.total_bytes, 21);
        assert_eq!(extracted.files.len(), 4);
        assert_eq!(
            fs::read(dest.path().join("project/readme.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            fs::read(dest.path().join("project/src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert_eq!(
            fs::read(dest.path().join("project/src/deep/zero.bin")).unwrap(),
            b""
        );
        assert!(
            dest.path().join("project/empty").is_dir(),
            "empty folders are kept"
        );
        assert_eq!(
            fs::read(dest.path().join("photo.jpg")).unwrap(),
            [1, 2, 3, 4]
        );
    }

    #[test]
    fn modification_times_are_restored() {
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("old.txt");
        fs::write(&file, b"x").unwrap();
        let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        let dest = tempfile::tempdir().unwrap();
        with_ctx(|ctx| {
            let plan = plan(std::slice::from_ref(&file), ctx).unwrap();
            let tar_bytes = write_tar(&plan, Vec::new(), ctx).unwrap();
            extract(&tar_bytes[..], dest.path(), ctx).unwrap();
        });
        let restored = fs::metadata(dest.path().join("old.txt")).unwrap();
        assert_eq!(restored.modified().unwrap(), long_ago);
    }

    #[test]
    fn inputs_with_the_same_name_get_numbered() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("a")).unwrap();
        fs::create_dir_all(source.path().join("b")).unwrap();
        let first = source.path().join("a/notes.txt");
        let second = source.path().join("b/Notes.txt");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        with_ctx(|ctx| {
            let plan = plan(&[first.clone(), second.clone()], ctx).unwrap();
            let names: Vec<_> = plan
                .entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect();
            assert_eq!(
                names,
                [PathBuf::from("notes.txt"), PathBuf::from("Notes (2).txt")]
            );
        });
    }

    #[test]
    fn numbered_names_keep_the_extension() {
        assert_eq!(numbered_name(OsStr::new("report.pdf"), 2), "report (2).pdf");
        assert_eq!(numbered_name(OsStr::new("folder"), 3), "folder (3)");
        assert_eq!(numbered_name(OsStr::new(".profile"), 2), ".profile (2)");
        assert_eq!(numbered_name(OsStr::new("a.tar.gz"), 2), "a.tar (2).gz");
    }

    #[test]
    fn missing_and_empty_inputs_are_refused() {
        with_ctx(|ctx| {
            assert!(matches!(plan(&[], ctx), Err(AtvError::InvalidInput { .. })));
            let missing = PathBuf::from("this/path/does/not/exist.bin");
            assert!(matches!(
                plan(&[missing], ctx),
                Err(AtvError::InvalidInput { .. })
            ));
        });
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_folders_are_skipped_and_counted() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", root.join("link")).unwrap();
        with_ctx(|ctx| {
            let plan = plan(&[root.clone()], ctx).unwrap();
            assert_eq!(plan.skipped_links, 1);
            assert_eq!(plan.file_count, 1);
        });
    }

    #[test]
    fn a_file_that_shrinks_while_being_read_stops_the_job() {
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("shrinks.bin");
        fs::write(&file, vec![5u8; 10_000]).unwrap();
        with_ctx(|ctx| {
            let plan = plan(std::slice::from_ref(&file), ctx).unwrap();
            fs::write(&file, vec![5u8; 100]).unwrap();
            let error = write_tar(&plan, Vec::new(), ctx)
                .map(|_| ())
                .map_err(|error| ctx.resolve(error))
                .unwrap_err();
            assert!(matches!(error, AtvError::InvalidInput { .. }), "{error:?}");
        });
    }

    #[test]
    fn safe_relative_accepts_plain_paths_only() {
        assert_eq!(
            safe_relative(Path::new("a/b/c.txt")).unwrap(),
            Path::new("a/b/c.txt")
        );
        assert_eq!(
            safe_relative(Path::new("./a/./b")).unwrap(),
            Path::new("a/b")
        );
        for bad in ["../x", "a/../../x", "a/..", "/etc/passwd", "", "."] {
            assert!(
                matches!(
                    safe_relative(Path::new(bad)),
                    Err(AtvError::UnsafePath { .. })
                ),
                "{bad:?} must be refused"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn safe_relative_refuses_windows_escapes() {
        for bad in [
            r"C:\Windows\x",
            r"C:x",
            r"\\server\share\x",
            r"\x",
            r"a\..\..\x",
            "a:stream",
        ] {
            assert!(
                matches!(
                    safe_relative(Path::new(bad)),
                    Err(AtvError::UnsafePath { .. })
                ),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn entries_that_climb_out_are_refused() {
        for path in [
            &b"../evil.txt"[..],
            b"a/../../evil.txt",
            b"/tmp/atv-evil.txt",
        ] {
            let parent = tempfile::tempdir().unwrap();
            let dest = parent.path().join("out");
            fs::create_dir(&dest).unwrap();
            let tar_bytes = raw_tar(path, tar::EntryType::file(), None, b"evil");
            assert_unsafe(try_extract(&tar_bytes, &dest));
            assert_nothing_escaped(parent.path());
            assert_eq!(
                fs::read_dir(&dest).unwrap().count(),
                0,
                "nothing was written"
            );
        }
    }

    #[test]
    fn links_pointing_outside_are_refused() {
        let cases: [(&[u8], tar::EntryType, &[u8]); 4] = [
            (b"link", tar::EntryType::symlink(), b"../outside"),
            (b"dir/link", tar::EntryType::symlink(), b"../../outside"),
            (b"link", tar::EntryType::symlink(), b"/etc/passwd"),
            (b"dir/hard", tar::EntryType::hard_link(), b"../outside"),
        ];
        for (path, kind, target) in cases {
            let parent = tempfile::tempdir().unwrap();
            let dest = parent.path().join("out");
            fs::create_dir(&dest).unwrap();
            let tar_bytes = raw_tar(path, kind, Some(target), b"");
            assert_unsafe(try_extract(&tar_bytes, &dest));
            assert_nothing_escaped(parent.path());
        }
    }

    #[test]
    fn links_pointing_inside_are_skipped_not_created() {
        let dest = tempfile::tempdir().unwrap();
        let tar_bytes = raw_tar(
            b"dir/link",
            tar::EntryType::symlink(),
            Some(b"../file"),
            b"",
        );
        let extracted = try_extract(&tar_bytes, dest.path()).unwrap();
        assert_eq!(extracted.skipped, 1);
        assert!(extracted.files.is_empty());
        assert!(fs::symlink_metadata(dest.path().join("dir/link")).is_err());
    }

    #[test]
    fn an_existing_file_is_never_replaced() {
        let dest = tempfile::tempdir().unwrap();
        fs::write(dest.path().join("keep.txt"), b"original").unwrap();
        let tar_bytes = raw_tar(b"keep.txt", tar::EntryType::file(), None, b"replacement");
        assert!(try_extract(&tar_bytes, dest.path()).is_err());
        assert_eq!(fs::read(dest.path().join("keep.txt")).unwrap(), b"original");
    }

    #[test]
    fn committing_never_overwrites_and_numbers_clashes() {
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("report.pdf"), b"mine").unwrap();
        fs::create_dir(out.path().join("photos")).unwrap();

        let staging = Staging::create(out.path()).unwrap();
        fs::write(staging.path().join("report.pdf"), b"restored").unwrap();
        fs::create_dir(staging.path().join("photos")).unwrap();
        fs::write(staging.path().join("photos/a.jpg"), b"a").unwrap();
        fs::write(staging.path().join("new.txt"), b"new").unwrap();
        let staging_path = staging.path().to_path_buf();

        let moved = staging.commit(out.path()).unwrap();
        let final_names: Vec<_> = moved.iter().map(|(_, name)| name.clone()).collect();
        assert_eq!(final_names, ["new.txt", "photos (2)", "report (2).pdf"]);
        assert_eq!(fs::read(out.path().join("report.pdf")).unwrap(), b"mine");
        assert_eq!(
            fs::read(out.path().join("report (2).pdf")).unwrap(),
            b"restored"
        );
        assert_eq!(fs::read(out.path().join("photos (2)/a.jpg")).unwrap(), b"a");
        assert!(!staging_path.exists(), "the staging folder is removed");
    }

    #[test]
    fn dropping_staging_removes_everything_in_it() {
        let out = tempfile::tempdir().unwrap();
        let staging = Staging::create(out.path()).unwrap();
        fs::write(staging.path().join("half.bin"), b"partial").unwrap();
        drop(staging);
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
        assert!(out.path().exists(), "a folder that was already there stays");
    }

    #[test]
    fn a_failed_job_removes_an_output_folder_it_created() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("new/nested");
        let staging = Staging::create(&out).unwrap();
        fs::write(staging.path().join("half.bin"), b"partial").unwrap();
        drop(staging);
        assert!(!out.exists(), "the folder this job made is gone");

        // After a successful commit the folder stays, even if nothing was in the video.
        let staging = Staging::create(&out).unwrap();
        staging.commit(&out).unwrap();
        assert!(out.is_dir());
    }
}
