//! Decoding (spec section 4):
//! ffmpeg -> frames -> [decrypt] -> body -> [zstd] -> SHA-256 check -> files,
//! and `probe`, which reads only the header frame.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::archive::{self, Staging};
use crate::crypto::{self, Key, OpenReader, Protection};
use crate::error::AtvError;
use crate::ffmpeg::{self, Decoder, FfmpegTools, StreamInfo};
use crate::format::header::{HEADER_LEN, Header};
use crate::format::{Layout, grid};
use crate::frames::{self, FrameReader, FrameSource, HEADER_FRAMES};
use crate::progress::Ctx;
use crate::stream::{HASH_LEN, HashingReader, TrailerReader};
use crate::types::{
    CancelToken, DecodeOptions, DecodeReport, Event, Resolution, RestoredFile, Stage, VideoInfo,
};

/// The zstd stream inside a video. zstd itself failing means the data is not
/// what ATV wrote: damaged, or changed after it was made. A failure from
/// further up (damage, a wrong key, cancel) was parked first and is kept.
struct Unpacking<'a, 'c, R: Read> {
    zstd: zstd::stream::read::Decoder<'static, io::BufReader<R>>,
    ctx: &'a Ctx<'c>,
}

impl<R: Read> Read for Unpacking<'_, '_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.zstd
            .read(out)
            .map_err(|_| self.ctx.fail(AtvError::Corrupt))
    }
}

/// Turn a video back into the original files.
///
/// Files are restored into a hidden temporary folder inside the output folder
/// and only moved into place once the whole video has been read and its
/// checksum has matched. A failed or cancelled job leaves nothing behind, and
/// an existing file is never replaced.
///
/// For an encrypted video, `KeyRequired` says which secrets are missing and
/// `WrongKey` says the ones given do not open it; both are reported before
/// anything is written.
pub fn decode(
    opts: &DecodeOptions,
    on_event: &mut dyn FnMut(Event),
    cancel: &CancelToken,
) -> Result<DecodeReport, AtvError> {
    let ctx = Ctx::new(on_event, cancel, Stage::ReadingFrames);
    run(opts, &ctx).map_err(|error| ctx.resolve(error))
}

fn run(opts: &DecodeOptions, ctx: &Ctx) -> Result<DecodeReport, AtvError> {
    ctx.emit_now();
    let stream = ffmpeg::probe_stream(&opts.tools, &opts.video)?;
    let sizing = sizing(&opts.tools, &opts.video, &stream)?;
    ctx.set_frames_total(
        stream
            .frames
            .map(|frames| frames.saturating_sub(HEADER_FRAMES)),
    );
    let mut decoder = Decoder::spawn(&opts.tools, &opts.video, None, sizing.scale_to)?;
    restore(
        &mut decoder,
        sizing.resolution,
        &opts.output_dir,
        opts.protection.as_ref(),
        ctx,
    )
}

/// At which ATV size a video's frames are read, and whether ffmpeg must
/// scale them back to that size first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sizing {
    pub resolution: Resolution,
    /// `Some` when the file is not at an ATV size: a site resized it.
    pub scale_to: Option<Resolution>,
}

/// Work out how to read a video (spec 5.1 and 5.5).
///
/// A video at an ATV size is read as it is. A video with the same 16:9
/// shape at another size is a copy that a site resized: it is scaled back,
/// to 1080p or to 4K, whichever gives a readable header frame. The header's
/// CRC makes a wrong guess fail instead of reading garbage. Any other shape
/// is refused.
pub(crate) fn sizing(
    tools: &FfmpegTools,
    video: &Path,
    stream: &StreamInfo,
) -> Result<Sizing, AtvError> {
    let (width, height) = (stream.width, stream.height);
    if let Some(native) = Resolution::from_dimensions(width, height) {
        // Almost always an ATV video at its own size. But a site can also
        // shrink a 4K video to exactly 1920 x 1080 (YouTube serves that copy
        // while it still makes the 4K one), or enlarge a 1080p one to 4K: if
        // no header is found as it is, try the other ATV size.
        if header_found(tools, video, native, None)? {
            return Ok(Sizing {
                resolution: native,
                scale_to: None,
            });
        }
        let other = match native {
            Resolution::P1080 => Resolution::P2160,
            Resolution::P2160 => Resolution::P1080,
        };
        if header_found(tools, video, other, Some(other))? {
            return Ok(Sizing {
                resolution: other,
                scale_to: Some(other),
            });
        }
        return Err(AtvError::NotAtvVideo);
    }
    if !is_16_by_9(width, height) {
        return Err(AtvError::UnsupportedResolution { width, height });
    }
    // Try the nearer size first.
    let order = if height > 1080 {
        [Resolution::P2160, Resolution::P1080]
    } else {
        [Resolution::P1080, Resolution::P2160]
    };
    for candidate in order {
        if header_found(tools, video, candidate, Some(candidate))? {
            return Ok(Sizing {
                resolution: candidate,
                scale_to: Some(candidate),
            });
        }
    }
    Err(AtvError::NotAtvVideo)
}

/// Whether an ATV header frame can be read with the frames seen at
/// `resolution`, scaled there first when `scale_to` is set. Errors other
/// than "no header" (ffmpeg failing, a newer format version) are passed on.
fn header_found(
    tools: &FfmpegTools,
    video: &Path,
    resolution: Resolution,
    scale_to: Option<Resolution>,
) -> Result<bool, AtvError> {
    let mut decoder = Decoder::spawn(tools, video, Some(HEADER_FRAMES as u32), scale_to)?;
    match frames::read_header(&mut decoder, resolution, &mut Vec::new(), &mut Vec::new()) {
        Ok(_) => Ok(true),
        Err(AtvError::NotAtvVideo) => Ok(false),
        Err(error) => Err(error),
    }
}

/// True for a 16:9 frame, allowing 1% for sizes rounded to whole or even pixels.
pub(crate) fn is_16_by_9(width: u32, height: u32) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    let wide = u64::from(width) * 9;
    let tall = u64::from(height) * 16;
    wide.abs_diff(tall) * 100 <= tall
}

/// Read a whole video from `source`, hand its tar stream to `consume`, and
/// check the SHA-256 that ends the body.
///
/// `unlock` is asked for the key once the header is known; it returns `None`
/// for a video that is not encrypted. The first encrypted chunk is opened
/// before `consume` is called, so a wrong key is reported before `consume`
/// has done anything. `check_stage`, if given, is announced when the final
/// hash is compared.
///
/// Returns what `consume` returned, the SHA-256 of the tar stream, and the header.
pub(crate) fn read_video<S: FrameSource, T>(
    source: &mut S,
    resolution: Resolution,
    ctx: &Ctx,
    check_stage: Option<Stage>,
    unlock: impl FnOnce(&Header, &[u8; HEADER_LEN]) -> Result<Option<Key>, AtvError>,
    consume: impl FnOnce(&mut dyn Read) -> Result<T, AtvError>,
) -> Result<(T, [u8; HASH_LEN], Header), AtvError> {
    let (mut frames, header, raw) = FrameReader::open(source, resolution, ctx)?;
    let key = unlock(&header, &raw)?;
    let mut body: Box<dyn Read + '_> = match (&key, header.encrypted) {
        (Some(key), true) => Box::new(OpenReader::open(&mut frames, key, &header, &raw, ctx)?),
        (None, false) => Box::new(&mut frames),
        // `unlock` and the header disagree; never read ciphertext as data.
        _ => {
            return Err(AtvError::KeyRequired {
                password: header.password_used,
                key_file: header.key_file_used,
            });
        }
    };

    // The body is the packed data followed by the SHA-256 of the tar stream.
    let mut trailer = TrailerReader::new(&mut body);
    let (value, actual) = {
        let packed: Box<dyn Read + '_> = if header.compressed {
            let mut zstd = zstd::stream::read::Decoder::new(&mut trailer)?;
            // Videos may use windows above zstd's default 128 MiB (spec 4.1).
            zstd.window_log_max(crate::encode::ZSTD_WINDOW_LOG_READ)?;
            Box::new(Unpacking { zstd, ctx })
        } else {
            Box::new(&mut trailer)
        };
        let mut hashing = HashingReader::new(packed);
        let value = consume(&mut hashing)?;
        // Whatever follows the end of the archive is still part of the hash.
        io::copy(&mut hashing, &mut io::sink())?;
        (value, hashing.finish())
    };

    if let Some(stage) = check_stage {
        ctx.set_stage(stage);
    }
    let stored = trailer.trailer().ok_or(AtvError::IntegrityFailed)?;
    if stored != actual {
        return Err(AtvError::IntegrityFailed);
    }
    Ok((value, actual, header))
}

/// Read a whole video from `source` and restore its files into `output_dir`.
pub(crate) fn restore<S: FrameSource>(
    source: &mut S,
    resolution: Resolution,
    output_dir: &Path,
    protection: Option<&Protection>,
    ctx: &Ctx,
) -> Result<DecodeReport, AtvError> {
    let ((staging, extracted), sha256, _header) = read_video(
        source,
        resolution,
        ctx,
        Some(Stage::CheckingIntegrity),
        |header, _raw| {
            if header.encrypted {
                crypto::unlock(protection, header).map(Some)
            } else {
                Ok(None)
            }
        },
        |tar| {
            let staging = Staging::create(output_dir)?;
            let extracted = archive::extract(tar, staging.path(), ctx)?;
            Ok((staging, extracted))
        },
    )?;

    let moved = staging.commit(output_dir)?;
    let files = extracted
        .files
        .into_iter()
        .map(|file| RestoredFile {
            path: renamed(&file.path, &moved),
            size: file.size,
        })
        .collect();
    ctx.emit_now();
    Ok(DecodeReport {
        output_dir: output_dir.to_path_buf(),
        items: moved
            .iter()
            .map(|(_, final_name)| output_dir.join(final_name))
            .collect(),
        files,
        total_bytes: extracted.total_bytes,
        sha256,
        repaired_bytes: ctx.repaired_bytes(),
        skipped_entries: extracted.skipped,
    })
}

/// Apply the top-level renames made while moving items into the output folder.
fn renamed(path: &Path, moved: &[(OsString, OsString)]) -> PathBuf {
    let mut parts = path.components();
    let Some(first) = parts.next() else {
        return path.to_path_buf();
    };
    let first = first.as_os_str();
    let final_name = moved
        .iter()
        .find(|(original, _)| original.as_os_str() == first)
        .map_or(first, |(_, final_name)| final_name.as_os_str());
    let rest = parts.as_path();
    if rest.as_os_str().is_empty() {
        // Joining an empty path would add a trailing separator.
        PathBuf::from(final_name)
    } else {
        Path::new(final_name).join(rest)
    }
}

/// Read what a video says about itself, without decoding its data.
///
/// The header frames are read, and the first data frame for a thumbnail
/// picture. For an encrypted video this tells which of password and key file
/// are needed; it never reveals file names or sizes.
pub fn probe(video: &Path, tools: &FfmpegTools) -> Result<VideoInfo, AtvError> {
    let stream = ffmpeg::probe_stream(tools, video)?;
    let sizing = sizing(tools, video, &stream)?;
    let resolution = sizing.resolution;
    let mut decoder = Decoder::spawn(
        tools,
        video,
        Some(HEADER_FRAMES as u32 + 1),
        sizing.scale_to,
    )?;
    let mut gray = Vec::new();
    let (header, _raw) = frames::read_header(&mut decoder, resolution, &mut gray, &mut Vec::new())?;
    // The frames left of the first three end with the first data frame. The
    // thumbnail is a nicety, so a failure to read it is not an error.
    let mut preview = None;
    while decoder.read_frame(&mut gray).unwrap_or(false) {
        preview = Some(grid::preview_from_gray(
            &gray,
            Layout::for_mode(resolution, &header.mode),
        ));
    }
    Ok(VideoInfo {
        preview,
        resized_from: sizing.scale_to.map(|_| (stream.width, stream.height)),
        resolution,
        mode: header.mode,
        format_version: header.version,
        compressed: header.compressed,
        encrypted: header.encrypted,
        needs_password: header.password_used,
        needs_key_file: header.key_file_used,
        codec: stream.codec,
        seconds: stream.seconds,
        frames: stream.frames,
        file_bytes: stream.file_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::crypto::{CHUNK_LEN, TAG_LEN, generate_key_file};
    use crate::encode::{verify, write_video};
    use crate::format::block::{self, BLOCK_HEADER_LEN};
    use crate::format::{Layout, grid};
    use crate::frames::tests::{MemorySink, MemorySource};
    use crate::testutil::noise;
    use crate::types::{Cell, Mode};

    const RES: Resolution = Resolution::P1080;
    /// Payload bytes in one frame at this resolution.
    const PAYLOAD: usize = 259_184;

    fn with_ctx<T>(body: impl FnOnce(&Ctx) -> T) -> T {
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::Preparing);
        body(&ctx)
    }

    /// A video held in memory, with what is needed to verify it.
    struct Encoded {
        frames: Vec<Vec<u8>>,
        raw: [u8; HEADER_LEN],
        key: Option<Key>,
        sha256: [u8; 32],
    }

    fn encode_to_memory(
        inputs: &[PathBuf],
        compress: bool,
        protection: Option<&Protection>,
    ) -> Encoded {
        encode_mode_to_memory(inputs, Mode::Lossless, compress, protection)
    }

    fn encode_mode_to_memory(
        inputs: &[PathBuf],
        mode: Mode,
        compress: bool,
        protection: Option<&Protection>,
    ) -> Encoded {
        encode_version_to_memory(inputs, mode, compress, protection, None)
    }

    /// As above, but `version` overrides the format version, to make the
    /// version-1 Robust videos that earlier releases wrote.
    fn encode_version_to_memory(
        inputs: &[PathBuf],
        mode: Mode,
        compress: bool,
        protection: Option<&Protection>,
        version: Option<u8>,
    ) -> Encoded {
        with_ctx(|ctx| {
            let plan = archive::plan(inputs, ctx).unwrap();
            let mut header = Header::plain(mode);
            if let Some(version) = version {
                header.version = version;
            }
            header.compressed = compress;
            let key = protection.map(|protection| crypto::lock(protection, &mut header).unwrap());
            let raw = header.to_bytes();
            let mut sink = MemorySink::default();
            let (sha256, _) = write_video(
                &plan,
                &header,
                &raw,
                key.as_ref(),
                RES,
                crate::encode::ZSTD_LEVEL,
                ctx,
                &mut sink,
            )
            .unwrap();
            Encoded {
                frames: sink.frames,
                raw,
                key,
                sha256,
            }
        })
    }

    fn decode_from_memory(
        frames: Vec<Vec<u8>>,
        out: &Path,
        protection: Option<&Protection>,
    ) -> Result<DecodeReport, AtvError> {
        with_ctx(|ctx| {
            let mut source = MemorySource::new(frames);
            restore(&mut source, RES, out, protection, ctx).map_err(|error| ctx.resolve(error))
        })
    }

    fn verify_in_memory(encoded: &Encoded, frames: Vec<Vec<u8>>) -> Result<(), AtvError> {
        with_ctx(|ctx| {
            let mut source = MemorySource::new(frames);
            verify(
                &mut source,
                RES,
                &encoded.raw,
                encoded.key.as_ref(),
                &encoded.sha256,
                ctx,
            )
        })
    }

    fn sample_inputs(dir: &Path) -> Vec<PathBuf> {
        let folder = dir.join("album");
        fs::create_dir_all(folder.join("raw")).unwrap();
        fs::write(folder.join("one.bin"), noise(300_000, 1)).unwrap();
        fs::write(folder.join("raw/two.bin"), noise(10, 2)).unwrap();
        let single = dir.join("notes.txt");
        fs::write(&single, b"remember the milk").unwrap();
        vec![folder, single]
    }

    fn assert_sample_restored(out: &Path) {
        assert_eq!(
            fs::read(out.join("album/one.bin")).unwrap(),
            noise(300_000, 1)
        );
        assert_eq!(
            fs::read(out.join("album/raw/two.bin")).unwrap(),
            noise(10, 2)
        );
        assert_eq!(
            fs::read(out.join("notes.txt")).unwrap(),
            b"remember the milk"
        );
    }

    /// Change one payload byte of a frame and redo the frame's own CRC, so
    /// only the layers above the frame can notice.
    fn tamper(frame: &mut [u8], payload_offset: usize) {
        let info = block::open(frame).unwrap();
        assert!(payload_offset < info.payload_len);
        frame[BLOCK_HEADER_LEN + payload_offset] ^= 0x01;
        block::seal(frame, info.index, info.payload_len, info.last);
    }

    /// A password-only protection. A short word becomes a long phrase, so it
    /// passes the strength rule (spec 6.4); "right" and "wrong" stay different,
    /// and an empty one stays empty.
    fn password(text: &str) -> Protection {
        let phrase = if text.is_empty() {
            String::new()
        } else {
            format!("ATV test passphrase: {text}")
        };
        Protection {
            password: Some(phrase.into()),
            key_file: None,
        }
    }

    fn key_file_in(dir: &Path, name: &str) -> Protection {
        let path = dir.join(name);
        generate_key_file(&path).unwrap();
        Protection {
            password: None,
            key_file: Some(path),
        }
    }

    #[test]
    fn the_whole_pipeline_round_trips_in_memory() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let encoded = encode_to_memory(&inputs, false, None);
        assert!(
            encoded.frames.len() >= 4,
            "two header frames, two or more data frames"
        );

        let out = tempfile::tempdir().unwrap();
        let report = decode_from_memory(encoded.frames, out.path(), None).unwrap();
        assert_eq!(report.sha256, encoded.sha256);
        assert_eq!(report.total_bytes, 300_000 + 10 + 17);
        assert_eq!(report.files.len(), 3);
        assert_eq!(
            report.items,
            [out.path().join("album"), out.path().join("notes.txt")]
        );
        assert_sample_restored(out.path());
        let names: Vec<_> = fs::read_dir(out.path())
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "the staging folder is gone: {names:?}");
    }

    #[test]
    fn every_combination_of_compression_and_protection_round_trips() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let keys = tempfile::tempdir().unwrap();
        let key_only = key_file_in(keys.path(), "a.atvkey");
        let both = Protection {
            password: Some("both of them".into()),
            key_file: key_only.key_file.clone(),
        };
        let protections = [
            None,
            Some(password("only a password")),
            Some(key_only),
            Some(both),
        ];
        for compress in [false, true] {
            for protection in &protections {
                let encoded = encode_to_memory(&inputs, compress, protection.as_ref());
                // Read the header back the way a decoder would.
                let gray = crate::testutil::mono_to_gray(&encoded.frames[0]);
                let (header, _) =
                    crate::format::header::read_frame(&gray, RES, &mut Vec::new()).unwrap();
                assert_eq!(header.compressed, compress);
                assert_eq!(header.encrypted, protection.is_some());

                let out = tempfile::tempdir().unwrap();
                let report =
                    decode_from_memory(encoded.frames.clone(), out.path(), protection.as_ref())
                        .unwrap_or_else(|error| {
                            panic!("compress {compress}, {protection:?}: {error}")
                        });
                assert_eq!(report.sha256, encoded.sha256);
                assert_sample_restored(out.path());
                verify_in_memory(&encoded, encoded.frames.clone()).unwrap();
            }
        }
    }

    #[test]
    fn compression_makes_compressible_data_smaller() {
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("log.txt");
        fs::write(&file, "the same line again and again\n".repeat(100_000)).unwrap();
        let plain = encode_to_memory(std::slice::from_ref(&file), false, None);
        let packed = encode_to_memory(std::slice::from_ref(&file), true, None);
        assert_eq!(
            plain.frames.len(),
            2 + 12,
            "3 MB of text without compression"
        );
        assert_eq!(packed.frames.len(), 2 + 1, "and with it");
        assert_eq!(
            plain.sha256, packed.sha256,
            "the hash is of the data before compression"
        );

        let out = tempfile::tempdir().unwrap();
        decode_from_memory(packed.frames, out.path(), None).unwrap();
        assert_eq!(
            fs::read_to_string(out.path().join("log.txt")).unwrap(),
            "the same line again and again\n".repeat(100_000)
        );
    }

    #[test]
    fn a_copy_far_away_is_stored_once() {
        // a/copy.bin, a/other.bin, b/copy.bin: the two copies are 8 MiB apart,
        // beyond zstd's usual 2 MiB window at level 3 (spec 4.1).
        const MIB: usize = 1 << 20;
        let source = tempfile::tempdir().unwrap();
        let (a, b) = (source.path().join("a"), source.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let copy = noise(4 * MIB, 1);
        fs::write(a.join("copy.bin"), &copy).unwrap();
        fs::write(a.join("other.bin"), noise(4 * MIB, 2)).unwrap();
        fs::write(b.join("copy.bin"), &copy).unwrap();

        let packed = encode_to_memory(&[a, b], true, None);
        let data_frames = packed.frames.len() - 2;
        let two_files = (8 * MIB).div_ceil(PAYLOAD);
        assert!(
            data_frames <= two_files + 1,
            "12 MiB with 4 MiB repeated should take about 8 MiB: {data_frames} frames, {two_files} for 8 MiB"
        );

        let out = tempfile::tempdir().unwrap();
        decode_from_memory(packed.frames, out.path(), None).unwrap();
        assert_eq!(
            fs::read(out.path().join("b").join("copy.bin")).unwrap(),
            copy
        );
    }

    #[test]
    fn measuring_the_compressed_size_predicts_the_frames_exactly() {
        let source = tempfile::tempdir().unwrap();
        let (a, b) = (source.path().join("a"), source.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let copy = noise(700_000, 3);
        fs::write(a.join("copy.bin"), &copy).unwrap();
        fs::write(a.join("log.txt"), "a line of text\n".repeat(200_000)).unwrap();
        fs::write(b.join("copy.bin"), &copy).unwrap();
        let inputs = [a, b];

        let packed = crate::encode::measure_packed(&inputs, &CancelToken::new()).unwrap();
        let predicted = crate::encode::estimate_packed(packed, RES, &Mode::Lossless);
        let unmeasured = crate::encode::estimate(3_400_000, RES, &Mode::Lossless);
        let made = encode_to_memory(&inputs, true, None);
        assert_eq!(predicted.frames, made.frames.len() as u64);
        assert!(
            predicted.frames < unmeasured.frames,
            "{predicted:?} {unmeasured:?}"
        );

        let cancel = CancelToken::new();
        cancel.cancel();
        let stopped = crate::encode::measure_packed(&inputs, &cancel);
        assert!(matches!(stopped, Err(AtvError::Cancelled)), "{stopped:?}");
    }

    #[test]
    fn the_wrong_password_is_refused_before_anything_is_written() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let encoded = encode_to_memory(&inputs, true, Some(&password("right")));
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("out");
        let result = decode_from_memory(encoded.frames, &out, Some(&password("wrong")));
        assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");
        assert!(!out.exists(), "the output folder was not even created");
    }

    #[test]
    fn the_wrong_key_file_is_refused() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let keys = tempfile::tempdir().unwrap();
        let right = key_file_in(keys.path(), "right.atvkey");
        let wrong = key_file_in(keys.path(), "wrong.atvkey");
        let encoded = encode_to_memory(&inputs, false, Some(&right));
        let out = tempfile::tempdir().unwrap();
        let result = decode_from_memory(encoded.frames.clone(), out.path(), Some(&wrong));
        assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");

        // With both in use, having only one right is not enough.
        let both = Protection {
            password: Some("pw".into()),
            key_file: right.key_file.clone(),
        };
        let encoded = encode_to_memory(&inputs, false, Some(&both));
        let half_right = Protection {
            password: Some("pw".into()),
            key_file: wrong.key_file.clone(),
        };
        let result = decode_from_memory(encoded.frames.clone(), out.path(), Some(&half_right));
        assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");
        let other_half = Protection {
            password: Some("not pw".into()),
            key_file: right.key_file.clone(),
        };
        let result = decode_from_memory(encoded.frames, out.path(), Some(&other_half));
        assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    }

    #[test]
    fn missing_secrets_are_asked_for() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let keys = tempfile::tempdir().unwrap();
        let both = Protection {
            password: Some("pw".into()),
            key_file: key_file_in(keys.path(), "k.atvkey").key_file,
        };
        let encoded = encode_to_memory(&inputs, false, Some(&both));
        let out = tempfile::tempdir().unwrap();
        for given in [None, Some(password("pw"))] {
            let result = decode_from_memory(encoded.frames.clone(), out.path(), given.as_ref());
            assert!(
                matches!(
                    result,
                    Err(AtvError::KeyRequired {
                        password: true,
                        key_file: true
                    })
                ),
                "{result:?}"
            );
        }
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_encrypted_video_reveals_no_names_or_content() {
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("very-secret-name.txt");
        fs::write(&file, "TOP-SECRET-CONTENT ".repeat(2000)).unwrap();
        let contains = |frames: &[Vec<u8>], needle: &[u8]| {
            frames
                .iter()
                .any(|frame| frame.windows(needle.len()).any(|window| window == needle))
        };

        // Without encryption both are plainly there, so the search itself works.
        let plain = encode_to_memory(std::slice::from_ref(&file), false, None);
        assert!(contains(&plain.frames, b"very-secret-name.txt"));
        assert!(contains(&plain.frames, b"TOP-SECRET-CONTENT"));

        let secret = encode_to_memory(std::slice::from_ref(&file), false, Some(&password("pw")));
        assert!(!contains(&secret.frames, b"very-secret-name"));
        assert!(!contains(&secret.frames, b"TOP-SECRET"));

        // The header says only how to read the video: everything in its
        // reserved area is zero, and no field holds a size or a name.
        assert_eq!(&secret.raw[9..12], &[0; 3]);
        assert_eq!(&secret.raw[59..124], &[0; 65]);
    }

    #[test]
    fn tampering_with_encrypted_data_is_caught() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let protection = password("pw");
        let encoded = encode_to_memory(&inputs, false, Some(&protection));
        let out = tempfile::tempdir().unwrap();

        // In the first encrypted chunk: indistinguishable from a wrong key.
        let mut frames = encoded.frames.clone();
        tamper(&mut frames[2], 100);
        let result = decode_from_memory(frames, out.path(), Some(&protection));
        assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");

        // In a later chunk: damage.
        let mut frames = encoded.frames.clone();
        tamper(&mut frames[2], 2 * (CHUNK_LEN + TAG_LEN) + 100);
        let result = decode_from_memory(frames, out.path(), Some(&protection));
        assert!(
            matches!(result, Err(AtvError::Damaged { .. })),
            "{result:?}"
        );
        assert_eq!(
            fs::read_dir(out.path()).unwrap().count(),
            0,
            "nothing is left behind"
        );
    }

    #[test]
    fn swapped_encrypted_chunks_are_caught() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let protection = password("pw");
        let encoded = encode_to_memory(&inputs, false, Some(&protection));
        let mut frames = encoded.frames.clone();
        // Swap the second and third sealed chunks inside the first data frame.
        let info = block::open(&frames[2]).unwrap();
        let sealed_chunk = CHUNK_LEN + TAG_LEN;
        let start = BLOCK_HEADER_LEN + sealed_chunk;
        let (second, third) = frames[2][start..start + 2 * sealed_chunk].split_at_mut(sealed_chunk);
        second.swap_with_slice(third);
        block::seal(&mut frames[2], info.index, info.payload_len, info.last);

        let out = tempfile::tempdir().unwrap();
        let result = decode_from_memory(frames, out.path(), Some(&protection));
        assert!(
            matches!(result, Err(AtvError::Damaged { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_cut_off_encrypted_video_is_caught() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let protection = password("pw");
        let encoded = encode_to_memory(&inputs, false, Some(&protection));
        let out = tempfile::tempdir().unwrap();

        let mut frames = encoded.frames.clone();
        frames.pop();
        let result = decode_from_memory(frames, out.path(), Some(&protection));
        assert!(matches!(result, Err(AtvError::Truncated)), "{result:?}");

        // Cut off by marking an earlier frame as the last one: every frame
        // still checks out, but the encrypted stream has no final chunk.
        let mut frames = encoded.frames.clone();
        frames.pop();
        let info = block::open(&frames[2]).unwrap();
        block::seal(&mut frames[2], info.index, info.payload_len, true);
        let result = decode_from_memory(frames, out.path(), Some(&protection));
        assert!(matches!(result, Err(AtvError::Truncated)), "{result:?}");
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    }

    #[test]
    fn restoring_twice_never_overwrites() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let encoded = encode_to_memory(&inputs, false, None);
        let out = tempfile::tempdir().unwrap();
        decode_from_memory(encoded.frames.clone(), out.path(), None).unwrap();
        fs::write(out.path().join("notes.txt"), b"edited since").unwrap();

        let second = decode_from_memory(encoded.frames, out.path(), None).unwrap();
        assert_eq!(
            second.items,
            [
                out.path().join("album (2)"),
                out.path().join("notes (2).txt")
            ]
        );
        assert!(
            second
                .files
                .iter()
                .any(|file| file.path == Path::new("album (2)/raw/two.bin")),
            "file paths follow the rename: {:?}",
            second.files
        );
        assert_eq!(
            fs::read(out.path().join("notes.txt")).unwrap(),
            b"edited since"
        );
        assert_eq!(
            fs::read(out.path().join("notes (2).txt")).unwrap(),
            b"remember the milk"
        );
    }

    #[test]
    fn data_that_does_not_match_its_hash_is_refused_and_cleaned_up() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        for compress in [false, true] {
            let mut encoded = encode_to_memory(&inputs, compress, None);
            tamper(&mut encoded.frames[2], 2000);
            let parent = tempfile::tempdir().unwrap();
            let out = parent.path().join("out");
            let result = decode_from_memory(encoded.frames, &out, None);
            // Uncompressed, only the final hash can notice. Compressed, zstd
            // usually notices first. Either way nothing may be kept.
            assert!(result.is_err(), "compress {compress}: {result:?}");
            if !compress {
                assert!(
                    matches!(result, Err(AtvError::IntegrityFailed)),
                    "{result:?}"
                );
            }
            assert!(!out.exists(), "nothing is left behind, not even the folder");
        }
    }

    #[test]
    fn a_damaged_frame_leaves_nothing_behind() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let mut encoded = encode_to_memory(&inputs, false, None);
        encoded.frames[3][9000] ^= 0x80;
        let out = tempfile::tempdir().unwrap();
        let result = decode_from_memory(encoded.frames, out.path(), None);
        assert!(
            matches!(result, Err(AtvError::Damaged { frame: 1 })),
            "{result:?}"
        );
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    }

    #[test]
    fn verify_passes_on_a_good_video() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        for protection in [None, Some(password("pw"))] {
            let encoded = encode_to_memory(&inputs, true, protection.as_ref());
            verify_in_memory(&encoded, encoded.frames.clone()).unwrap();
        }
    }

    #[test]
    fn verify_fails_on_a_tampered_video() {
        let source = tempfile::tempdir().unwrap();
        let big = source.path().join("big.bin");
        fs::write(&big, noise(3 * PAYLOAD, 5)).unwrap();
        let inputs = [big];
        let failed = |encoded: &Encoded, frames: Vec<Vec<u8>>| {
            let result = verify_in_memory(encoded, frames);
            assert!(matches!(result, Err(AtvError::VerifyFailed)), "{result:?}");
        };

        for protection in [None, Some(password("pw"))] {
            let encoded = encode_to_memory(&inputs, false, protection.as_ref());
            assert!(encoded.frames.len() >= 5);

            // A payload byte changed, with the frame CRC redone to match.
            let mut frames = encoded.frames.clone();
            tamper(&mut frames[3], 5000);
            failed(&encoded, frames);

            // A frame damaged, a frame missing, the end cut off.
            let mut frames = encoded.frames.clone();
            frames[3][70_000] ^= 0xFF;
            failed(&encoded, frames);
            let mut frames = encoded.frames.clone();
            frames.remove(3);
            failed(&encoded, frames);
            let mut frames = encoded.frames.clone();
            frames.pop();
            failed(&encoded, frames);

            // No header at all.
            let mut frames = encoded.frames.clone();
            frames[0].fill(0);
            frames[1].fill(0);
            failed(&encoded, frames);
        }

        // A perfectly good video, but not the one that was just written.
        let first = encode_to_memory(&inputs, false, None);
        let other_file = source.path().join("other.bin");
        fs::write(&other_file, noise(1000, 6)).unwrap();
        let second = encode_to_memory(&[other_file], false, None);
        failed(&first, second.frames.clone());
        // And one whose header differs (compressed where it should not be).
        let compressed = encode_to_memory(&inputs, true, None);
        failed(&first, compressed.frames);
    }

    #[test]
    fn cancelling_a_verify_is_reported_as_cancelled() {
        let source = tempfile::tempdir().unwrap();
        let inputs = sample_inputs(source.path());
        let encoded = encode_to_memory(&inputs, false, None);
        let mut ignore = |_event: Event| {};
        let cancel = CancelToken::new();
        cancel.cancel();
        let ctx = Ctx::new(&mut ignore, &cancel, Stage::Verifying);
        let mut source = MemorySource::new(encoded.frames.clone());
        let result = verify(&mut source, RES, &encoded.raw, None, &encoded.sha256, &ctx);
        assert!(matches!(result, Err(AtvError::Cancelled)), "{result:?}");
    }

    const ROBUST_MODES: [Mode; 4] = [
        Mode::Robust {
            cell: Cell::C4,
            parity: 32,
        },
        Mode::Robust {
            cell: Cell::C8,
            parity: 32,
        },
        Mode::Robust {
            cell: Cell::C4,
            parity: 64,
        },
        Mode::Robust {
            cell: Cell::C8,
            parity: 64,
        },
    ];

    /// A small input, so Robust videos stay a handful of frames long.
    fn small_inputs(dir: &Path) -> Vec<PathBuf> {
        let file = dir.join("small.bin");
        fs::write(&file, noise(40_000, 77)).unwrap();
        vec![file]
    }

    /// Paint a solid rectangle over a `monob` frame, as a logo or a blotch would.
    fn blot(frame: &mut [u8], left: usize, top: usize, width: usize, height: usize, white: bool) {
        let stride = 1920 / 8;
        for y in top..top + height {
            for x in left..left + width {
                let mask = 0x80u8 >> (x % 8);
                if white {
                    frame[y * stride + x / 8] |= mask;
                } else {
                    frame[y * stride + x / 8] &= !mask;
                }
            }
        }
    }

    #[test]
    fn robust_videos_round_trip_in_memory() {
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        for mode in ROBUST_MODES {
            for (compress, protection) in [(false, None), (true, Some(password("pw")))] {
                let encoded = encode_mode_to_memory(&inputs, mode, compress, protection.as_ref());
                let out = tempfile::tempdir().unwrap();
                let report =
                    decode_from_memory(encoded.frames.clone(), out.path(), protection.as_ref())
                        .unwrap_or_else(|error| panic!("{mode:?}: {error}"));
                assert_eq!(report.repaired_bytes, 0, "a clean video needs no repair");
                assert_eq!(
                    fs::read(out.path().join("small.bin")).unwrap(),
                    noise(40_000, 77)
                );
                verify_in_memory(&encoded, encoded.frames.clone()).unwrap();
            }
        }
    }

    #[test]
    fn robust_frames_show_blocks_of_the_right_size() {
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        for mode in [ROBUST_MODES[0], ROBUST_MODES[1]] {
            let cell = mode.cell_pixels() as usize;
            let encoded = encode_mode_to_memory(&inputs, mode, false, None);
            let frame = &encoded.frames[2];
            assert_eq!(frame.len(), 1920 * 1080 / 8, "a full-size picture");
            // Every cell is one solid colour: all its pixels equal its first pixel.
            let layout = Layout::for_mode(RES, &mode);
            let mut bits = vec![0u8; layout.capacity()];
            grid::read(&crate::testutil::mono_to_gray(frame), layout, &mut bits);
            let mut redrawn = vec![0u8; frame.len()];
            grid::draw(&bits, layout, &mut redrawn);
            assert_eq!(&redrawn, frame, "{cell} x {cell} cells");
        }
    }

    #[test]
    fn damage_within_the_limit_is_repaired_and_counted() {
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        for mode in ROBUST_MODES {
            let encoded = encode_mode_to_memory(&inputs, mode, false, None);
            let mut frames = encoded.frames.clone();
            // A black blotch and a white one on every data frame, each 64 by
            // 96 pixels. (Kept narrow on purpose: see the note on tall narrow
            // damage in `robust.rs`.)
            for frame in frames.iter_mut().skip(2) {
                blot(frame, 400, 300, 64, 96, false);
                blot(frame, 1200, 700, 64, 96, true);
            }
            let out = tempfile::tempdir().unwrap();
            let report = decode_from_memory(frames.clone(), out.path(), None)
                .unwrap_or_else(|error| panic!("{mode:?}: {error}"));
            assert!(report.repaired_bytes > 0, "{mode:?}: repairs are counted");
            assert_eq!(
                fs::read(out.path().join("small.bin")).unwrap(),
                noise(40_000, 77)
            );

            // Verify sees the same repairs and still passes.
            verify_in_memory(&encoded, frames).unwrap();
        }
    }

    #[test]
    fn damage_beyond_the_limit_is_reported_never_wrong_output() {
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        for mode in ROBUST_MODES {
            let encoded = encode_mode_to_memory(&inputs, mode, false, None);
            let mut frames = encoded.frames.clone();
            // Wipe the top 40% of seven data frames of the first group: far
            // past what parity can rebuild, even woven across the group.
            for frame in &mut frames[3..10] {
                blot(frame, 0, 0, 1920, 432, false);
            }
            let parent = tempfile::tempdir().unwrap();
            let out = parent.path().join("out");
            let result = decode_from_memory(frames.clone(), &out, None);
            assert!(
                matches!(result, Err(AtvError::Damaged { frame: 0 })),
                "{mode:?}: {result:?}"
            );
            assert!(!out.exists(), "{mode:?}: nothing is written");
            let verified = verify_in_memory(&encoded, frames);
            assert!(
                matches!(verified, Err(AtvError::VerifyFailed)),
                "{verified:?}"
            );
        }
    }

    #[test]
    fn version_1_robust_videos_still_restore() {
        // Videos made before format version 2 (13 Nov 2026) must keep working,
        // damage repair included.
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        let password = password("old");
        for mode in ROBUST_MODES {
            let encoded = encode_version_to_memory(&inputs, mode, true, Some(&password), Some(1));
            assert_eq!(encoded.raw[4], 1, "written as version 1");
            let mut frames = encoded.frames.clone();
            blot(&mut frames[2], 0, 0, 300, 60, false);
            let out = tempfile::tempdir().unwrap();
            decode_from_memory(frames, out.path(), Some(&password))
                .unwrap_or_else(|error| panic!("{mode:?}: {error}"));
            assert_eq!(
                fs::read(out.path().join("small.bin")).unwrap(),
                noise(40_000, 77)
            );
        }
    }

    #[test]
    fn frames_with_unreadable_tags_are_placed_by_their_neighbours() {
        // Found with the YouTube 720p imitation at 4K 4 x 4, where tags have
        // the least room: compression broke a few tags while the data was
        // repairable. Here the tags of four frames of one group are destroyed
        // and their data left alone. As missing frames that would be past
        // repair in both versions (68 erasures per codeword against 64
        // parity bytes; 27% of every LDPC codeword); placed by their
        // neighbours, nothing is lost.
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        let mode = Mode::Robust {
            cell: Cell::C8,
            parity: 64,
        };
        let layout = Layout::for_mode(RES, &mode);
        for version in [2u8, 3] {
            let encoded = encode_version_to_memory(&inputs, mode, false, None, Some(version));
            let positions: Vec<u32> = if version == 2 {
                crate::robust::GroupCodec::new(layout, 64)
                    .tag_positions()
                    .to_vec()
            } else {
                crate::robust::LdpcGroupCodec::new(layout, 64)
                    .tag_positions()
                    .to_vec()
            };
            let row_bytes = layout.width / layout.cell / 8;
            let mono_row = layout.width / 8;
            let mut frames = encoded.frames.clone();
            // Data frames 3, 4, 9 and 14 (after the two header frames).
            for data_frame in [3usize, 4, 9, 14] {
                let frame = &mut frames[2 + data_frame];
                for &position in &positions {
                    // Byte `position` is 8 cells in a row; with 8 x 8 cells each
                    // cell is one byte of the 1-bit picture, 8 rows tall.
                    let position = position as usize;
                    let (row, column) = (position / row_bytes, position % row_bytes);
                    for cell in 0..8 {
                        let x = (column * 8 + cell) * layout.cell / 8;
                        for y in 0..layout.cell {
                            frame[(row * layout.cell + y) * mono_row + x] ^= 0xFF;
                        }
                    }
                }
            }
            let out = tempfile::tempdir().unwrap();
            decode_from_memory(frames, out.path(), None)
                .unwrap_or_else(|error| panic!("version {version}: {error}"));
            assert_eq!(
                fs::read(out.path().join("small.bin")).unwrap(),
                noise(40_000, 77)
            );
        }
    }

    #[test]
    fn groups_rebuild_a_wiped_frame_and_a_lost_one() {
        // What one frame alone could never survive in format version 1: its
        // damage is shared with the other 14 frames of its group (spec 5.4,
        // 5.4b). Version 3 treats the wiped frame as lost on a second try.
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        for (mode, version) in ROBUST_MODES
            .into_iter()
            .flat_map(|mode| [(mode, 2u8), (mode, 3)])
        {
            let encoded = encode_version_to_memory(&inputs, mode, false, None, Some(version));
            let mut wiped = encoded.frames.clone();
            blot(&mut wiped[5], 0, 0, 1920, 432, false);
            let out = tempfile::tempdir().unwrap();
            let report = decode_from_memory(wiped, out.path(), None)
                .unwrap_or_else(|error| panic!("{mode:?} v{version} wiped: {error}"));
            assert!(report.repaired_bytes > 0);
            assert_eq!(
                fs::read(out.path().join("small.bin")).unwrap(),
                noise(40_000, 77)
            );

            if let Mode::Robust { parity: 64, .. } = mode {
                let mut lost = encoded.frames.clone();
                lost.remove(8);
                let out = tempfile::tempdir().unwrap();
                decode_from_memory(lost, out.path(), None)
                    .unwrap_or_else(|error| panic!("{mode:?} v{version} lost: {error}"));
                assert_eq!(
                    fs::read(out.path().join("small.bin")).unwrap(),
                    noise(40_000, 77)
                );
            }
        }
    }

    #[test]
    fn a_ruined_frame_followed_by_a_good_copy_is_forgiven() {
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        let mode = ROBUST_MODES[0];
        let encoded = encode_mode_to_memory(&inputs, mode, false, None);
        let mut frames = encoded.frames.clone();
        let mut ruined = frames[3].clone();
        blot(&mut ruined, 0, 0, 1920, 600, true);
        frames.insert(3, ruined);
        // And every frame shown twice, as a frame-rate-doubling site would do.
        let doubled: Vec<_> = frames.iter().flat_map(|f| [f.clone(), f.clone()]).collect();
        let out = tempfile::tempdir().unwrap();
        let report = decode_from_memory(doubled, out.path(), None).unwrap();
        assert_eq!(
            report.repaired_bytes, 0,
            "repairs in unused frames are not counted"
        );
        assert_eq!(
            fs::read(out.path().join("small.bin")).unwrap(),
            noise(40_000, 77)
        );
    }

    #[test]
    fn a_robust_header_survives_damage_to_every_copy() {
        let source = tempfile::tempdir().unwrap();
        let inputs = small_inputs(source.path());
        let encoded = encode_mode_to_memory(&inputs, ROBUST_MODES[1], false, None);
        let mut frames = encoded.frames.clone();
        // A vertical stripe through both header frames hits every copy of the
        // record somewhere; the vote across copies still recovers it.
        for frame in frames.iter_mut().take(2) {
            blot(frame, 960, 0, 8, 1080, true);
        }
        let out = tempfile::tempdir().unwrap();
        decode_from_memory(frames, out.path(), None).unwrap();
        assert_eq!(
            fs::read(out.path().join("small.bin")).unwrap(),
            noise(40_000, 77)
        );
    }

    #[test]
    fn only_the_16_by_9_shape_counts_as_resized() {
        for (width, height) in [
            (1280, 720),
            (854, 480),
            (2560, 1440),
            (1366, 768),
            (640, 360),
        ] {
            assert!(is_16_by_9(width, height), "{width} x {height}");
        }
        for (width, height) in [
            (1080, 1080),
            (1280, 1024),
            (1920, 1200),
            (720, 1280),
            (0, 0),
        ] {
            assert!(!is_16_by_9(width, height), "{width} x {height}");
        }
    }

    #[test]
    fn top_level_renames_are_applied_to_file_paths() {
        let moved = vec![
            (OsString::from("album"), OsString::from("album (2)")),
            (OsString::from("notes.txt"), OsString::from("notes.txt")),
        ];
        // Compared as text: `Path` equality ignores a trailing separator,
        // which is exactly the mistake this test must catch.
        let text = |path: PathBuf| path.into_os_string();
        let expected = |parts: &[&str]| parts.iter().collect::<PathBuf>().into_os_string();
        assert_eq!(
            text(renamed(
                &PathBuf::from(expected(&["album", "raw", "two.bin"])),
                &moved
            )),
            expected(&["album (2)", "raw", "two.bin"])
        );
        assert_eq!(
            text(renamed(Path::new("notes.txt"), &moved)),
            expected(&["notes.txt"])
        );
        assert_eq!(
            text(renamed(Path::new("album"), &moved)),
            expected(&["album (2)"])
        );
        assert_eq!(
            text(renamed(&PathBuf::from(expected(&["other", "x"])), &moved)),
            expected(&["other", "x"])
        );
    }
}
