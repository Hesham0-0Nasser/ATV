//! Encoding (spec section 4):
//! inputs -> tar -> [zstd] -> SHA-256 trailer -> [encrypt] -> frames -> ffmpeg.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::archive::{self, Plan};
use crate::crypto::{self, CHUNK_LEN, Key, SealWriter, TAG_LEN};
use crate::decode::read_video;
use crate::error::AtvError;
use crate::ffmpeg::{Decoder, Encoder};
use crate::format::block::BLOCK_HEADER_LEN;
use crate::format::header::{HEADER_LEN, Header, VERSION_3};
use crate::format::{Layout, payload_len};
use crate::frames::{FrameSink, FrameSource, FrameWriter, HEADER_FRAMES};
use crate::progress::Ctx;
use crate::robust::{GROUP_FRAMES, GroupRepair};
use crate::stream::{HASH_LEN, HashingWriter};
use crate::types::{
    CancelToken, Cell, EncodeOptions, EncodeReport, Estimate, Event, FPS, Mode, Resolution, Stage,
    VideoCodec,
};

/// Size of a tar block. Headers and file contents are padded to this.
const TAR_BLOCK: u64 = 512;
/// zstd compression level (spec 4.1).
pub(crate) const ZSTD_LEVEL: i32 = 3;
/// zstd level for "smallest video": about 17% smaller on documents, much slower.
pub(crate) const ZSTD_LEVEL_SMALLEST: i32 = 19;
/// Largest zstd window the encoder uses, as a power of two: 512 MiB (spec 4.1).
/// With long-distance matching on, data repeated anywhere within this distance
/// is stored once. Restoring needs this much memory at most.
pub(crate) const ZSTD_WINDOW_LOG: u32 = 29;
/// Largest window the decoder accepts: 2 GiB, the same bound as Argon2's
/// memory, so a hostile video cannot demand more (spec 4.1 and 6.3).
pub(crate) const ZSTD_WINDOW_LOG_READ: u32 = 31;
/// zstd's smallest window.
const ZSTD_WINDOW_LOG_MIN: u32 = 10;

/// The zstd window for an archive of about `archive_bytes`: just big enough to
/// see all of it, up to `ZSTD_WINDOW_LOG`. A small input then needs little
/// memory to create or restore.
fn zstd_window_log(archive_bytes: u64) -> u32 {
    let bits = u64::BITS - archive_bytes.saturating_sub(1).leading_zeros();
    bits.clamp(ZSTD_WINDOW_LOG_MIN, ZSTD_WINDOW_LOG)
}

/// About how long the tar stream of `plan` is: the data plus a header and
/// padding for each entry. Only the zstd window depends on it.
fn archive_len(plan: &Plan) -> u64 {
    let entries = plan.entries.len() as u64;
    plan.total_bytes
        .saturating_add(entries.saturating_add(2).saturating_mul(2 * TAR_BLOCK))
}

/// The zstd stream ATV writes: `level`, with long-distance matching and a
/// window sized to the archive, so repeats far apart, such as copies of a
/// file in different folders, are stored once (spec 4.1).
fn zstd_writer<W: Write>(
    inner: W,
    level: i32,
    plan: &Plan,
) -> io::Result<zstd::stream::write::Encoder<'static, W>> {
    let mut zstd = zstd::stream::write::Encoder::new(inner, level)?;
    zstd.long_distance_matching(true)?;
    zstd.window_log(zstd_window_log(archive_len(plan)))?;
    Ok(zstd)
}

/// Counts what is written to it and keeps nothing.
#[derive(Default)]
struct ByteCount(u64);

impl Write for ByteCount {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(data.len() as u64);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How many bytes compression makes of `inputs`, measured by compressing
/// them without keeping anything, so the video can be predicted before it is
/// made (spec 4.1; see [`estimate_packed`]). Uses level 3, so with "smallest
/// video" the real size is at most this. Reads every file once; `cancel`
/// stops it with `Cancelled`.
pub fn measure_packed(inputs: &[PathBuf], cancel: &CancelToken) -> Result<u64, AtvError> {
    let mut ignore = |_event: Event| {};
    let ctx = Ctx::new(&mut ignore, cancel, Stage::Preparing);
    let measure = || -> Result<u64, AtvError> {
        let plan = archive::plan(inputs, &ctx)?;
        let zstd = zstd_writer(ByteCount::default(), ZSTD_LEVEL, &plan)?;
        let zstd = archive::write_tar(&plan, zstd, &ctx)?;
        Ok(zstd.finish()?.0)
    };
    measure().map_err(|error| ctx.resolve(error))
}

/// Turn the inputs into a video.
///
/// `on_event` is called on this thread with progress and preview frames.
/// If the job fails or `cancel` is used, ffmpeg is stopped and the partial
/// video file is deleted.
pub fn encode(
    opts: &EncodeOptions,
    on_event: &mut dyn FnMut(Event),
    cancel: &CancelToken,
) -> Result<EncodeReport, AtvError> {
    let ctx = Ctx::new(on_event, cancel, Stage::Preparing);
    run(opts, &ctx).map_err(|error| ctx.resolve(error))
}

fn run(opts: &EncodeOptions, ctx: &Ctx) -> Result<EncodeReport, AtvError> {
    check_output_name(&opts.output, &opts.mode)?;
    let frame_rate = frame_rate_for(&opts.mode, opts.frame_rate)?;
    let codec = match opts.mode {
        Mode::Robust { .. } => opts.video_codec,
        Mode::Lossless => VideoCodec::H264,
    };
    if codec == VideoCodec::Av1 && !opts.tools.av1 {
        return Err(AtvError::invalid(
            "this ffmpeg has no AV1 encoder (libaom-av1); choose H.264 or install a full ffmpeg build",
        ));
    }
    ctx.emit_now();
    let plan = archive::plan(&opts.inputs, ctx)?;
    check_output_is_not_an_input(&opts.output, &opts.inputs)?;

    // The header, and the key if the video is to be encrypted. Deriving the
    // key from a password takes a moment, so it happens before ffmpeg starts.
    let mut header = Header::plain(opts.mode);
    header.compressed = opts.compress;
    let key = match &opts.protection {
        Some(protection) => Some(crypto::lock(protection, &mut header)?),
        None => None,
    };
    let raw = header.to_bytes();
    ctx.check_cancel()?;

    if let Some(parent) = opts.output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let layout = Layout::for_mode(opts.resolution, &opts.mode);
    ctx.set_bytes_total(plan.total_bytes);
    ctx.set_frames_total(Some(data_frames_for(
        sealed_len(tar_len(&plan) + HASH_LEN as u64, key.is_some()),
        layout,
        &opts.mode,
    )));

    // Declared before the encoder so it is dropped after it: ffmpeg must be
    // gone before its half-written file can be deleted.
    let mut partial = PartialOutput::new(&opts.output);
    let mut encoder = Encoder::spawn(
        &opts.tools,
        layout,
        &opts.mode,
        frame_rate,
        codec,
        &opts.output,
    )?;
    partial.arm();

    ctx.set_stage(Stage::WritingFrames);
    let level = if opts.smallest {
        ZSTD_LEVEL_SMALLEST
    } else {
        ZSTD_LEVEL
    };
    let written = write_video(
        &plan,
        &header,
        &raw,
        key.as_ref(),
        opts.resolution,
        level,
        ctx,
        &mut encoder,
    );
    let (sha256, data_frames) = match written {
        Ok(done) => done,
        Err(error) => {
            let error = ctx.resolve(error);
            // A failed write to ffmpeg means ffmpeg died (a full disk, for
            // example). What it printed explains more than "broken pipe".
            if encoder.pipe_broken() && !matches!(error, AtvError::Cancelled) {
                return Err(encoder.failure());
            }
            return Err(error);
        }
    };
    encoder.finish()?;
    ctx.set_frames_total(Some(data_frames));
    ctx.emit_now();

    let mut verify_repaired_bytes = 0;
    if opts.verify {
        ctx.reset_frames();
        ctx.reset_repaired();
        ctx.set_stage(Stage::Verifying);
        let mut decoder = Decoder::spawn(&opts.tools, &opts.output, None, None)?;
        let checked = verify(
            &mut decoder,
            opts.resolution,
            &raw,
            key.as_ref(),
            &sha256,
            ctx,
        );
        // Stop ffmpeg before the file is kept or deleted.
        drop(decoder);
        if let Err(error) = checked {
            if matches!(error, AtvError::VerifyFailed) && opts.keep_unverified {
                partial.keep();
            }
            return Err(error);
        }
        verify_repaired_bytes = ctx.repaired_bytes();
        ctx.emit_now();
    }

    let video_bytes = fs::metadata(&opts.output)?.len();
    partial.keep();
    let frames = data_frames + HEADER_FRAMES;
    Ok(EncodeReport {
        output: opts.output.clone(),
        resolution: opts.resolution,
        mode: opts.mode,
        format_version: header.version,
        compressed: header.compressed,
        encrypted: header.encrypted,
        verified: opts.verify,
        frame_rate,
        verify_repaired_bytes,
        input_bytes: plan.total_bytes,
        file_count: plan.file_count,
        frames,
        seconds: frames as f64 / f64::from(frame_rate),
        video_bytes,
        sha256,
        skipped_links: plan.skipped_links,
    })
}

/// The body on its way to the frames: sealed in chunks, or as it is.
enum Body<W: Write> {
    Plain(W),
    Sealed(Box<SealWriter<W>>),
}

impl<W: Write> Body<W> {
    fn finish(self) -> io::Result<()> {
        match self {
            Body::Plain(_) => Ok(()),
            Body::Sealed(sealer) => sealer.finish().map(|_| ()),
        }
    }
}

impl<W: Write> Write for Body<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Body::Plain(inner) => inner.write(data),
            Body::Sealed(sealer) => sealer.write(data),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The tar stream on its way to the body: compressed, or as it is.
enum Packed<W: Write> {
    Raw(W),
    Zstd(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> Packed<W> {
    /// End the compressed stream and hand back the inner writer.
    fn finish(self) -> io::Result<W> {
        match self {
            Packed::Raw(inner) => Ok(inner),
            Packed::Zstd(encoder) => encoder.finish(),
        }
    }
}

impl<W: Write> Write for Packed<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Packed::Raw(inner) => inner.write(data),
            Packed::Zstd(encoder) => encoder.write(data),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stream the whole video into `sink`: two header frames, then the sealed
/// body cut into data frames. `header` decides compression; `key` decides
/// encryption. Returns the SHA-256 of the tar stream and the number of data
/// frames.
// Each argument is a separate, already-prepared piece; bundling them would
// only move the list into a struct used once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_video<S: FrameSink>(
    plan: &Plan,
    header: &Header,
    raw: &[u8; HEADER_LEN],
    key: Option<&Key>,
    resolution: Resolution,
    level: i32,
    ctx: &Ctx,
    sink: &mut S,
) -> Result<([u8; HASH_LEN], u64), AtvError> {
    let mut frames = FrameWriter::new(sink, resolution, &header.mode, header.version, ctx);
    frames.write_header(raw, resolution)?;
    let body = match key {
        Some(key) => Body::Sealed(Box::new(SealWriter::new(&mut frames, key, header, raw)?)),
        None => Body::Plain(&mut frames),
    };
    let packed = if header.compressed {
        Packed::Zstd(zstd_writer(body, level, plan)?)
    } else {
        Packed::Raw(body)
    };
    let hashing = archive::write_tar(plan, HashingWriter::new(packed), ctx)?;
    let (sha256, packed) = hashing.finish();
    let mut body = packed.finish()?;
    // The body always ends with the hash of the tar stream.
    body.write_all(&sha256)?;
    body.finish()?;
    let data_frames = frames.finish()?;
    Ok((sha256, data_frames))
}

/// Verify after encode: read the finished video back through the whole decode
/// pipeline, throwing the files away, and check that it carries the header
/// that was written and data with the hash that was computed while encoding.
///
/// Any sign that the video does not decode to the same data is `VerifyFailed`.
pub(crate) fn verify<S: FrameSource>(
    source: &mut S,
    resolution: Resolution,
    expected_raw: &[u8; HEADER_LEN],
    key: Option<&Key>,
    expected_sha256: &[u8; HASH_LEN],
    ctx: &Ctx,
) -> Result<(), AtvError> {
    let result = read_video(
        source,
        resolution,
        ctx,
        None,
        |_header, raw| {
            if raw == expected_raw {
                Ok(key.cloned())
            } else {
                Err(AtvError::VerifyFailed)
            }
        },
        |tar| Ok(io::copy(tar, &mut io::sink()).map(|_| ())?),
    );
    match result.map_err(|error| ctx.resolve(error)) {
        Ok(((), sha256, _)) if &sha256 == expected_sha256 => Ok(()),
        Ok(_) => Err(AtvError::VerifyFailed),
        // Problems with the machine rather than with the video keep their own message.
        Err(
            error @ (AtvError::Cancelled
            | AtvError::Io(_)
            | AtvError::FfmpegNotFound { .. }
            | AtvError::FfmpegFailed { .. }),
        ) => Err(error),
        Err(_) => Err(AtvError::VerifyFailed),
    }
}

/// Size of the body once sealed: every chunk of up to 65,536 bytes gains a
/// 16-byte tag, and there is always at least one chunk.
fn sealed_len(body_bytes: u64, encrypted: bool) -> u64 {
    if encrypted {
        let chunks = body_bytes.div_ceil(CHUNK_LEN as u64).max(1);
        body_bytes.saturating_add(chunks.saturating_mul(TAG_LEN as u64))
    } else {
        body_bytes
    }
}

// Deletes the output file when dropped, unless the job finished.
struct PartialOutput<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> PartialOutput<'a> {
    fn new(path: &'a Path) -> Self {
        PartialOutput { path, armed: false }
    }

    /// From now on a failure deletes the file. Called once ffmpeg has been
    /// started, so a job that never got that far leaves an existing file alone.
    fn arm(&mut self) {
        self.armed = true;
    }

    fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for PartialOutput<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

/// The frame rate to use: always 30 for Lossless; 30 unless told otherwise
/// for Robust.
fn frame_rate_for(mode: &Mode, requested: Option<u32>) -> Result<u32, AtvError> {
    match (mode, requested) {
        (_, None) => Ok(FPS),
        (Mode::Lossless, Some(rate)) if rate == FPS => Ok(FPS),
        (Mode::Lossless, Some(_)) => Err(AtvError::invalid(
            "Lossless videos are always 30 frames per second",
        )),
        (Mode::Robust { .. }, Some(rate)) if (1..=60).contains(&rate) => Ok(rate),
        (Mode::Robust { .. }, Some(_)) => Err(AtvError::invalid(
            "the frame rate must be between 1 and 60 frames per second",
        )),
    }
}

/// ffmpeg picks the container from the file name, so the name must match the mode.
fn check_output_name(output: &Path, mode: &Mode) -> Result<(), AtvError> {
    let wanted = mode.extension();
    let matches = output
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(wanted));
    if matches {
        Ok(())
    } else {
        let mode_name = match mode {
            Mode::Lossless => "Lossless",
            Mode::Robust { .. } => "Robust",
        };
        Err(AtvError::invalid(format!(
            "the video file name must end in .{wanted} for {mode_name} mode"
        )))
    }
}

/// Writing the video over one of its own inputs, or into a folder that is
/// being encoded, would read a file while it is being replaced.
fn check_output_is_not_an_input(output: &Path, inputs: &[PathBuf]) -> Result<(), AtvError> {
    let target = resolve_new_path(output);
    for input in inputs {
        let Ok(input) = fs::canonicalize(input) else {
            continue;
        };
        if target.starts_with(&input) {
            return Err(AtvError::invalid(
                "the video cannot be saved over, or inside, something that is being encoded; \
                 choose another place for it",
            ));
        }
    }
    Ok(())
}

/// The real location of a path whose last parts may not exist yet: the
/// nearest folder that does exist is resolved, and the rest is put back on.
fn resolve_new_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        if let Ok(real) = fs::canonicalize(existing) {
            return missing
                .iter()
                .rev()
                .fold(real, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return absolute,
        }
    }
}

fn round_up_to_block(size: u64) -> u64 {
    size.div_ceil(TAR_BLOCK).saturating_mul(TAR_BLOCK)
}

/// Size of the tar stream for a plan: a header block per entry, the padded
/// contents, and the two empty blocks at the end. Very long names take a
/// little more; this is for the progress bar, not for correctness.
fn tar_len(plan: &Plan) -> u64 {
    plan.entries
        .iter()
        .map(|entry| TAR_BLOCK + round_up_to_block(entry.size))
        .fold(2 * TAR_BLOCK, u64::saturating_add)
}

/// Data frames needed for `body_bytes` of sealed stream, in the format this
/// version writes: one block per frame for Lossless, one block per group of
/// 15 frames for Robust (format version 3, spec 5.4b). There is always at
/// least one data frame.
fn data_frames_for(body_bytes: u64, layout: Layout, mode: &Mode) -> u64 {
    match mode {
        Mode::Lossless => body_bytes
            .div_ceil((payload_len(layout, mode) as u64).max(1))
            .max(1),
        Mode::Robust { parity, .. } => {
            let group = GROUP_FRAMES as u64;
            let per_group = (GroupRepair::new(layout, *parity, VERSION_3).block_len() as u64)
                .saturating_sub(BLOCK_HEADER_LEN as u64)
                .max(1);
            body_bytes.div_ceil(per_group).max(1) * group
        }
    }
}

/// Predict the video for inputs of a given total size, without compression.
/// Meant for a live summary while options are being chosen; the real numbers
/// come back in [`EncodeReport`].
pub fn estimate(inputs_total_bytes: u64, resolution: Resolution, mode: &Mode) -> Estimate {
    // One archive entry, the end-of-archive blocks, and the hash.
    let body = (TAR_BLOCK + round_up_to_block(inputs_total_bytes))
        .saturating_add(2 * TAR_BLOCK + HASH_LEN as u64);
    estimate_body(body, resolution, mode)
}

/// As [`estimate`], for inputs whose compressed size `packed_bytes` was found
/// by [`measure_packed`].
pub fn estimate_packed(packed_bytes: u64, resolution: Resolution, mode: &Mode) -> Estimate {
    estimate_body(
        packed_bytes.saturating_add(HASH_LEN as u64),
        resolution,
        mode,
    )
}

/// The video for a body (the packed data and its hash) of `body` bytes.
fn estimate_body(body: u64, resolution: Resolution, mode: &Mode) -> Estimate {
    let layout = Layout::for_mode(resolution, mode);
    let frames = data_frames_for(body, layout, mode) + HEADER_FRAMES;
    let video_bytes = match mode {
        // FFV1 stores random-looking bits in about 4% more than the bits.
        Mode::Lossless => (body as f64 * 1.04) as u64,
        Mode::Robust { cell, .. } => frames.saturating_mul(robust_frame_bytes(resolution, *cell)),
    };
    Estimate {
        frames,
        seconds: frames as f64 / f64::from(FPS),
        video_bytes,
    }
}

/// H.264 bytes per Robust frame, measured with format 3 (spec 4.4). Every
/// frame looks like random blocks whatever the data and repair strength, so
/// the size follows the frame count; the frame rate does not change it.
fn robust_frame_bytes(resolution: Resolution, cell: Cell) -> u64 {
    match (resolution, cell) {
        (Resolution::P1080, Cell::C8) => 11_200,
        (Resolution::P1080, Cell::C4) => 59_650,
        (Resolution::P2160, Cell::C8) => 44_550,
        (Resolution::P2160, Cell::C4) => 236_450,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zstd_window_fits_the_archive() {
        assert_eq!(zstd_window_log(0), ZSTD_WINDOW_LOG_MIN);
        assert_eq!(zstd_window_log(1000), ZSTD_WINDOW_LOG_MIN);
        assert_eq!(zstd_window_log(1 << 20), 20, "exactly 1 MiB");
        assert_eq!(zstd_window_log((1 << 20) + 1), 21);
        assert_eq!(zstd_window_log(300 << 20), 29);
        assert_eq!(
            zstd_window_log(u64::MAX),
            ZSTD_WINDOW_LOG,
            "never above 512 MiB"
        );
        const { assert!(ZSTD_WINDOW_LOG <= ZSTD_WINDOW_LOG_READ) };
    }

    #[test]
    fn the_brief_example_holds() {
        // "A 1 GB input at 1080p Lossless is about 2 minutes of video."
        let one = estimate(1 << 30, Resolution::P1080, &Mode::Lossless);
        assert!((130.0..150.0).contains(&one.seconds), "{}", one.seconds);
        assert_eq!(
            one.frames,
            4143 + 2,
            "4,143 data frames plus two header frames"
        );
        let ratio = one.video_bytes as f64 / (1u64 << 30) as f64;
        assert!((1.03..1.05).contains(&ratio));
    }

    #[test]
    fn an_empty_input_still_needs_one_data_frame() {
        for resolution in [Resolution::P1080, Resolution::P2160] {
            let empty = estimate(0, resolution, &Mode::Lossless);
            assert_eq!(empty.frames, 3);
            assert!((empty.seconds - 0.1).abs() < 1e-9);
        }
    }

    #[test]
    fn robust_mode_needs_many_more_frames() {
        let megabyte = 1 << 20;
        let lossless = estimate(megabyte, Resolution::P1080, &Mode::Lossless);
        let robust = estimate(
            megabyte,
            Resolution::P1080,
            &Mode::Robust {
                cell: Cell::C8,
                parity: 32,
            },
        );
        assert_eq!(lossless.frames, 5 + 2);
        // Format version 3: whole groups of 15 frames, each carrying one
        // block of 59 codewords x 911 bytes less its 16-byte header.
        assert_eq!(
            robust.frames,
            (megabyte + 1568).div_ceil(59 * 911 - 16) * 15 + 2
        );
    }

    #[test]
    fn robust_sizes_match_the_measured_settings() {
        // 20 MB of random data (spec 7.1): the WhatsApp setting made a video
        // 3.2 times the data and the YouTube setting (H.264) 3.9 times.
        let data = 20_000_000;
        let ratio = |resolution, cell, parity| {
            estimate(data, resolution, &Mode::Robust { cell, parity }).video_bytes as f64
                / data as f64
        };
        let whatsapp = ratio(Resolution::P1080, Cell::C8, 64);
        let youtube = ratio(Resolution::P2160, Cell::C4, 32);
        assert!((3.1..3.5).contains(&whatsapp), "{whatsapp}");
        assert!((3.8..4.3).contains(&youtube), "{youtube}");
    }

    #[test]
    fn output_name_must_match_the_mode() {
        let robust = Mode::Robust {
            cell: Cell::C4,
            parity: 32,
        };
        assert!(check_output_name(Path::new("a.mkv"), &Mode::Lossless).is_ok());
        assert!(check_output_name(Path::new("a.MKV"), &Mode::Lossless).is_ok());
        assert!(check_output_name(Path::new("a.mp4"), &robust).is_ok());
        for (name, mode) in [
            ("a.mp4", Mode::Lossless),
            ("a", Mode::Lossless),
            ("a.mkv", robust),
        ] {
            assert!(matches!(
                check_output_name(Path::new(name), &mode),
                Err(AtvError::InvalidInput { .. })
            ));
        }
    }

    #[test]
    fn output_inside_an_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("data");
        fs::create_dir(&folder).unwrap();
        let file = dir.path().join("a.bin");
        fs::write(&file, b"x").unwrap();
        let inputs = [folder.clone(), file.clone()];

        let refused = |output: PathBuf| {
            matches!(
                check_output_is_not_an_input(&output, &inputs),
                Err(AtvError::InvalidInput { .. })
            )
        };
        assert!(refused(folder.join("out.mkv")), "inside an input folder");
        assert!(refused(folder.join("deeper").join("out.mkv")));
        assert!(refused(file.clone()), "over an input file");
        assert!(
            !refused(dir.path().join("out.mkv")),
            "beside the inputs is fine"
        );
        assert!(
            !refused(dir.path().join("data2.mkv")),
            "a similar name is fine"
        );
    }

    #[test]
    fn partial_output_is_deleted_unless_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mkv");

        fs::write(&path, b"half").unwrap();
        drop(PartialOutput::new(&path));
        assert!(path.exists(), "not armed: an existing file is left alone");

        let mut armed = PartialOutput::new(&path);
        armed.arm();
        drop(armed);
        assert!(!path.exists(), "armed and dropped: deleted");

        fs::write(&path, b"whole").unwrap();
        let mut kept = PartialOutput::new(&path);
        kept.arm();
        kept.keep();
        assert!(path.exists(), "kept: stays");
    }
}
