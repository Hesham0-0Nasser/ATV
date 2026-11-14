//! End-to-end tests that run the real ffmpeg: encode to a video file, decode
//! it back, compare. Each test is skipped, with a message, when ffmpeg and
//! ffprobe are not installed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use atv_core::{AtvError, CancelToken, Event, FfmpegTools, Mode, Resolution, Stage};
use common::{
    BOTH, FRAME_1080, decode, decode_options, encode, encode_options, items_in, noise, snapshot,
    tools,
};

/// Encode `inputs` (all inside `source_root`), decode, and require the result
/// to be identical to `source_root`.
fn assert_round_trip(tools: &FfmpegTools, source_root: &Path, resolution: Resolution) {
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("out.mkv");
    let restored = work.path().join("restored");
    let inputs = items_in(source_root);

    let encoded = encode(tools, &inputs, &video, resolution).unwrap();
    let decoded = decode(tools, &video, &restored).unwrap();

    assert_eq!(snapshot(&restored), snapshot(source_root), "{resolution:?}");
    assert_eq!(decoded.sha256, encoded.sha256);
    assert_eq!(decoded.total_bytes, encoded.input_bytes);
    assert_eq!(decoded.files.len() as u64, encoded.file_count);
    assert_eq!(encoded.video_bytes, fs::metadata(&video).unwrap().len());
}

#[test]
fn a_single_file_round_trips() {
    let Some(tools) = tools() else { return };
    for resolution in BOTH {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("report.pdf"), noise(400_000, 1)).unwrap();
        assert_round_trip(&tools, source.path(), resolution);
    }
}

#[test]
fn several_files_round_trip() {
    let Some(tools) = tools() else { return };
    for resolution in BOTH {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("a.bin"), noise(100_000, 2)).unwrap();
        fs::write(source.path().join("b.txt"), "plain text\n".repeat(500)).unwrap();
        fs::write(
            source.path().join("c with spaces & (odd) name.dat"),
            noise(7, 3),
        )
        .unwrap();
        assert_round_trip(&tools, source.path(), resolution);
    }
}

#[test]
fn a_folder_tree_round_trips() {
    let Some(tools) = tools() else { return };
    for resolution in BOTH {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("project");
        fs::create_dir_all(root.join("src/deeply/nested/folders")).unwrap();
        fs::create_dir_all(root.join("empty folder")).unwrap();
        fs::write(root.join("readme.md"), b"# hello").unwrap();
        fs::write(root.join("src/main.rs"), noise(50_000, 4)).unwrap();
        fs::write(
            root.join("src/deeply/nested/folders/data.bin"),
            noise(300_000, 5),
        )
        .unwrap();
        fs::write(root.join("src/deeply/empty.txt"), b"").unwrap();
        // A name longer than the 100 bytes a plain tar header holds.
        fs::write(
            root.join(format!("{}.txt", "long-name-".repeat(15))),
            b"long",
        )
        .unwrap();
        fs::write(
            root.join("unicode-\u{00e9}\u{4e2d}\u{6587}.txt"),
            "caf\u{00e9}",
        )
        .unwrap();
        assert_round_trip(&tools, source.path(), resolution);
    }
}

#[test]
fn an_empty_file_round_trips() {
    let Some(tools) = tools() else { return };
    for resolution in BOTH {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("empty.bin"), b"").unwrap();
        assert_round_trip(&tools, source.path(), resolution);
    }
}

#[test]
fn a_file_smaller_than_one_frame_round_trips() {
    let Some(tools) = tools() else { return };
    for resolution in BOTH {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("tiny.txt"), b"just a few bytes").unwrap();
        let work = tempfile::tempdir().unwrap();
        let video = work.path().join("tiny.mkv");
        let report = encode(
            &tools,
            &[source.path().join("tiny.txt")],
            &video,
            resolution,
        )
        .unwrap();
        assert_eq!(report.frames, 3, "two header frames and one data frame");
        assert_round_trip(&tools, source.path(), resolution);
    }
}

#[test]
fn a_file_spanning_many_frames_round_trips() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let size = 12 * FRAME_1080 + 12_345;
    fs::write(source.path().join("big.bin"), noise(size, 6)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("big.mkv");
    let report = encode(
        &tools,
        &[source.path().join("big.bin")],
        &video,
        Resolution::P1080,
    )
    .unwrap();
    assert_eq!(report.frames, 13 + 2);
    assert!((report.seconds - 0.5).abs() < 1e-9);
    assert_round_trip(&tools, source.path(), Resolution::P1080);
    // A few 4K frames too: each one carries four times as much.
    assert_round_trip(&tools, source.path(), Resolution::P2160);
}

#[test]
fn data_ending_exactly_on_a_frame_boundary_round_trips() {
    let Some(tools) = tools() else { return };
    // tar adds a 512-byte header, pads to 512 and ends with 1024 zero bytes;
    // the body adds a 32-byte hash. Fourteen frames is the smallest count a
    // single file can fill exactly, because of the padding to 512.
    let size = 14 * FRAME_1080 - 512 - 1024 - 32;
    assert_eq!(size % 512, 0);
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("exact.bin"), noise(size, 7)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("exact.mkv");
    let report = encode(
        &tools,
        &[source.path().join("exact.bin")],
        &video,
        Resolution::P1080,
    )
    .unwrap();
    assert_eq!(report.frames, 14 + 2, "no extra empty frame");
    assert_round_trip(&tools, source.path(), Resolution::P1080);
}

#[test]
fn the_video_is_about_four_percent_larger_than_random_data() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("random.bin");
    fs::write(&file, noise(20 * FRAME_1080, 8)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let report = encode(
        &tools,
        &[file],
        &work.path().join("r.mkv"),
        Resolution::P1080,
    )
    .unwrap();
    let ratio = report.video_bytes as f64 / report.input_bytes as f64;
    assert!(
        (1.02..1.07).contains(&ratio),
        "video is {ratio:.3} times the data"
    );
}

#[test]
fn probe_reads_the_header() {
    let Some(tools) = tools() else { return };
    for resolution in BOTH {
        let source = tempfile::tempdir().unwrap();
        let file = source.path().join("x.bin");
        fs::write(&file, noise(FRAME_1080 * 3, 9)).unwrap();
        let work = tempfile::tempdir().unwrap();
        let video = work.path().join("x.mkv");
        let report = encode(&tools, &[file], &video, resolution).unwrap();

        let info = atv_core::probe(&video, &tools).unwrap();
        assert_eq!(info.resolution, resolution);
        assert_eq!(info.mode, Mode::Lossless);
        assert!(!info.encrypted && !info.compressed);
        assert!(!info.needs_password && !info.needs_key_file);
        assert_eq!(info.codec, "ffv1");
        assert_eq!(info.file_bytes, report.video_bytes);
        assert_eq!(
            info.frames,
            Some(report.frames),
            "length x rate gives the frame count"
        );
        let preview = info.preview.expect("a thumbnail of the first data frame");
        assert_eq!((preview.width, preview.height), (480, 270));
        assert!(preview.pixels.iter().any(|p| *p > 128));
        assert!(preview.pixels.iter().any(|p| *p < 128));
    }
}

#[test]
fn progress_events_tell_the_whole_story() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("p.bin");
    let size = 6 * FRAME_1080;
    fs::write(&file, noise(size, 10)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("p.mkv");

    let mut events: Vec<Event> = Vec::new();
    let options = encode_options(&tools, &[file], &video, Resolution::P1080);
    let report = atv_core::encode(
        &options,
        &mut |event| events.push(event),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(events.first().unwrap().stage, Stage::Preparing);
    let last = events.last().unwrap();
    assert_eq!(last.stage, Stage::WritingFrames);
    assert_eq!(last.bytes_done, size as u64);
    assert_eq!(last.bytes_total, size as u64);
    assert_eq!(last.frames_done, report.frames - 2);
    assert_eq!(
        last.frames_total,
        Some(report.frames - 2),
        "the estimate was exact"
    );
    assert!(events.iter().any(|event| event.preview.is_some()));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].bytes_done <= pair[1].bytes_done)
    );

    let mut events: Vec<Event> = Vec::new();
    let options = decode_options(&tools, &video, &work.path().join("out"), None);
    atv_core::decode(
        &options,
        &mut |event| events.push(event),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(events.first().unwrap().stage, Stage::ReadingFrames);
    let last = events.last().unwrap();
    assert_eq!(last.stage, Stage::CheckingIntegrity);
    assert_eq!(last.bytes_done, size as u64);
    assert_eq!(last.frames_done, report.frames - 2);
    assert!(events.iter().any(|event| event.preview.is_some()));
}

#[test]
fn cancelling_an_encode_leaves_no_partial_file() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("long.bin");
    fs::write(&file, noise(60 * FRAME_1080, 11)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("cancelled.mkv");

    let cancel = CancelToken::new();
    let options = encode_options(&tools, &[file], &video, Resolution::P1080);
    let mut saw_frames = 0;
    let result = atv_core::encode(
        &options,
        &mut |event| {
            saw_frames = event.frames_done;
            if event.frames_done >= 5 {
                cancel.cancel();
            }
        },
        &cancel,
    );
    assert!(matches!(result, Err(AtvError::Cancelled)), "{result:?}");
    assert!(
        saw_frames < 60,
        "it stopped early (after {saw_frames} frames)"
    );
    assert!(!video.exists(), "the partial video was deleted");
    assert_eq!(fs::read_dir(work.path()).unwrap().count(), 0);
}

#[test]
fn cancelling_a_decode_leaves_nothing_behind() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("long.bin");
    fs::write(&file, noise(30 * FRAME_1080, 12)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    encode(&tools, &[file], &video, Resolution::P1080).unwrap();

    let out = work.path().join("out");
    let cancel = CancelToken::new();
    let options = decode_options(&tools, &video, &out, None);
    let result = atv_core::decode(
        &options,
        &mut |event| {
            if event.frames_done >= 5 {
                cancel.cancel();
            }
        },
        &cancel,
    );
    assert!(matches!(result, Err(AtvError::Cancelled)), "{result:?}");
    assert!(!out.exists(), "no partly restored files, and no folder");
}

#[test]
fn a_failed_encode_leaves_no_partial_file() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let good = source.path().join("good.bin");
    fs::write(&good, noise(3 * FRAME_1080, 13)).unwrap();
    let missing = source.path().join("missing.bin");
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("failed.mkv");

    let result = encode(&tools, &[good, missing], &video, Resolution::P1080);
    assert!(
        matches!(result, Err(AtvError::InvalidInput { .. })),
        "{result:?}"
    );
    assert!(!video.exists());
}

/// Make an ordinary video with ffmpeg's built-in test picture.
fn make_ordinary_video(tools: &FfmpegTools, size: &str, output: &Path) {
    let status = Command::new(&tools.ffmpeg)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("testsrc=size={size}:rate=30"))
        .args(["-frames:v", "5", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(output)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not make the test video");
}

#[test]
fn an_ordinary_video_is_not_an_atv_video() {
    let Some(tools) = tools() else { return };
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("holiday.mp4");
    make_ordinary_video(&tools, "1920x1080", &video);

    assert!(matches!(
        atv_core::probe(&video, &tools),
        Err(AtvError::NotAtvVideo)
    ));
    let out = work.path().join("out");
    let result = decode(&tools, &video, &out);
    assert!(matches!(result, Err(AtvError::NotAtvVideo)), "{result:?}");
    assert!(!out.exists(), "the output folder is not even created");
}

#[test]
fn a_video_of_another_shape_is_refused_with_its_size() {
    let Some(tools) = tools() else { return };
    let work = tempfile::tempdir().unwrap();
    // Not 16:9, so it cannot be a resized ATV video.
    let video = work.path().join("square.mp4");
    make_ordinary_video(&tools, "1280x1024", &video);
    let result = atv_core::probe(&video, &tools);
    assert!(
        matches!(
            result,
            Err(AtvError::UnsupportedResolution {
                width: 1280,
                height: 1024
            })
        ),
        "{result:?}"
    );
    // 16:9 at another size could be a resized copy, but this one has no header.
    let small = work.path().join("small.mp4");
    make_ordinary_video(&tools, "1280x720", &small);
    let result = atv_core::probe(&small, &tools);
    assert!(matches!(result, Err(AtvError::NotAtvVideo)), "{result:?}");
}

#[test]
fn a_resized_lossless_video_fails_cleanly() {
    // One pixel per bit cannot survive resizing. The header frame (8 x 8
    // cells) still reads, so the failure is reported as damage, and nothing
    // is restored.
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("data.bin");
    fs::write(&file, noise(2 * FRAME_1080, 41)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    encode(&tools, &[file], &video, Resolution::P1080).unwrap();
    let small = work.path().join("small.mp4");
    let status = Command::new(&tools.ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&video)
        .args([
            "-vf",
            "scale=1280:720",
            "-c:v",
            "libx264",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&small)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let info = atv_core::probe(&small, &tools).unwrap();
    assert_eq!(info.resized_from, Some((1280, 720)));
    let out = work.path().join("out");
    let result = decode(&tools, &small, &out);
    assert!(
        matches!(result, Err(AtvError::Damaged { .. })),
        "{result:?}"
    );
    assert!(!out.exists());
}

#[test]
fn a_file_that_is_not_a_video_is_not_an_atv_video() {
    let Some(tools) = tools() else { return };
    let work = tempfile::tempdir().unwrap();
    let fake = work.path().join("fake.mkv");
    fs::write(&fake, noise(50_000, 14)).unwrap();
    assert!(matches!(
        atv_core::probe(&fake, &tools),
        Err(AtvError::NotAtvVideo)
    ));
    let result = decode(&tools, &fake, &work.path().join("out"));
    assert!(matches!(result, Err(AtvError::NotAtvVideo)), "{result:?}");

    let missing = work.path().join("nope.mkv");
    assert!(matches!(
        atv_core::probe(&missing, &tools),
        Err(AtvError::Io(_))
    ));
}

#[test]
fn a_video_cut_short_is_reported_as_cut_short() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("whole.bin");
    fs::write(&file, noise(20 * FRAME_1080, 15)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("whole.mkv");
    encode(&tools, &[file], &video, Resolution::P1080).unwrap();

    let bytes = fs::read(&video).unwrap();
    let cut = work.path().join("cut.mkv");
    fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let out = work.path().join("out");
    let result = decode(&tools, &cut, &out);
    assert!(matches!(result, Err(AtvError::Truncated)), "{result:?}");
    assert!(!out.exists(), "nothing half-restored is kept");
}

#[test]
fn a_flipped_byte_in_the_video_file_is_caught() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("whole.bin");
    fs::write(&file, noise(8 * FRAME_1080, 16)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("whole.mkv");
    encode(&tools, &[file], &video, Resolution::P1080).unwrap();

    let mut bytes = fs::read(&video).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x55;
    let damaged = work.path().join("damaged.mkv");
    fs::write(&damaged, &bytes).unwrap();
    let out = work.path().join("out");
    let result = decode(&tools, &damaged, &out);
    assert!(
        result.is_err(),
        "damage must never produce files: {result:?}"
    );
    assert!(!out.exists());
}

#[test]
fn the_wrong_file_extension_is_refused_before_anything_is_written() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("a.bin");
    fs::write(&file, b"abc").unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("out.mp4");
    let result = encode(&tools, &[file], &video, Resolution::P1080);
    assert!(
        matches!(result, Err(AtvError::InvalidInput { .. })),
        "{result:?}"
    );
    assert!(!video.exists());
}

#[test]
fn smallest_video_is_smaller_for_documents_and_restores_the_same() {
    // "Smallest video" (zstd 19 instead of 3), the owner's choice after the
    // compression measurements of 13 Nov 2026.
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let mut text = String::new();
    for i in 0..60_000u32 {
        let word = ["alpha", "frame", "header", "pixel", "repair", "stream"][(i % 6) as usize];
        text.push_str(&format!(
            "line {i}: {word} {} {}\n",
            i.wrapping_mul(2_654_435_761) % 997,
            i % 13
        ));
    }
    fs::write(source.path().join("notes.txt"), &text).unwrap();
    let inputs = items_in(source.path());

    let mut sizes = Vec::new();
    for smallest in [false, true] {
        let work = tempfile::tempdir().unwrap();
        let video = work.path().join("v.mkv");
        let mut options = encode_options(&tools, &inputs, &video, Resolution::P1080);
        options.compress = true;
        options.smallest = smallest;
        let report = atv_core::encode(&options, &mut |_| {}, &CancelToken::new()).unwrap();
        sizes.push(report.video_bytes);

        let restored = work.path().join("restored");
        decode(&tools, &video, &restored).unwrap();
        assert_eq!(
            snapshot(&restored),
            snapshot(source.path()),
            "smallest {smallest}"
        );
    }
    assert!(
        sizes[1] < sizes[0],
        "smallest {} vs normal {}",
        sizes[1],
        sizes[0]
    );
}
