//! End-to-end tests for Robust mode through the real ffmpeg: H.264 video,
//! blocks of pixels, Reed-Solomon repair, and survival of a second,
//! lower-quality compression. Each test is skipped, with a message, when
//! ffmpeg and ffprobe are not installed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use atv_core::{AtvError, Cell, FfmpegTools, Mode, Protection, Resolution};
use common::{decode, decode_with, encode_options, items_in, noise, run_encode, snapshot, tools};

fn robust(cell: Cell) -> Mode {
    Mode::Robust { cell, parity: 32 }
}

/// A folder of small inputs: Robust mode holds little, so keep videos short.
fn small_tree(root: &Path) {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/data.bin"), noise(60_000, 41)).unwrap();
    fs::write(root.join("docs/readme.txt"), "robust mode\n".repeat(50)).unwrap();
    fs::write(root.join("tiny.txt"), b"x").unwrap();
}

/// Compress a video again, the way a hosting site would.
fn reencode(tools: &FfmpegTools, input: &Path, output: &Path, extra: &[&str]) {
    let status = Command::new(&tools.ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args([
            "-c:v", "libx264", "-preset", "medium", "-pix_fmt", "yuv420p",
        ])
        .args(extra)
        .arg(output)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not re-encode the video");
}

#[test]
fn robust_videos_round_trip_at_both_block_sizes_and_resolutions() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let inputs = items_in(source.path());
    for resolution in [Resolution::P1080, Resolution::P2160] {
        for cell in [Cell::C4, Cell::C8] {
            let work = tempfile::tempdir().unwrap();
            let video = work.path().join("v.mp4");
            let mut options = encode_options(&tools, &inputs, &video, resolution);
            options.mode = robust(cell);
            options.verify = true;
            let report = run_encode(&options)
                .unwrap_or_else(|error| panic!("{resolution:?} {cell:?}: {error}"));
            assert!(report.verified);
            assert_eq!(report.mode, robust(cell));

            let info = atv_core::probe(&video, &tools).unwrap();
            assert_eq!(info.resolution, resolution);
            assert_eq!(info.mode, robust(cell));
            assert_eq!(info.codec, "h264");
            assert_eq!(info.frames, Some(report.frames));

            let restored = work.path().join("restored");
            let decoded = decode(&tools, &video, &restored)
                .unwrap_or_else(|error| panic!("{resolution:?} {cell:?}: {error}"));
            assert_eq!(decoded.sha256, report.sha256);
            assert_eq!(
                snapshot(&restored),
                snapshot(source.path()),
                "{resolution:?} {cell:?}"
            );
        }
    }
}

#[test]
fn robust_videos_survive_being_compressed_again_at_crf_28() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let inputs = items_in(source.path());
    for cell in [Cell::C4, Cell::C8] {
        for parity in [32u8, 64] {
            let work = tempfile::tempdir().unwrap();
            let video = work.path().join("v.mp4");
            let mut options = encode_options(&tools, &inputs, &video, Resolution::P1080);
            options.mode = Mode::Robust { cell, parity };
            let report = run_encode(&options).unwrap();

            let squeezed = work.path().join("squeezed.mp4");
            reencode(&tools, &video, &squeezed, &["-crf", "28"]);
            // (The second file is not always smaller: 8 x 8 blocks line up
            // with H.264's own blocks and are already cheap to store.)
            let before = fs::metadata(&video).unwrap().len();
            let after = fs::metadata(&squeezed).unwrap().len();

            let restored = work.path().join("restored");
            let decoded = decode(&tools, &squeezed, &restored)
                .unwrap_or_else(|error| panic!("{cell:?} parity {parity}: {error}"));
            assert_eq!(decoded.sha256, report.sha256);
            assert_eq!(
                snapshot(&restored),
                snapshot(source.path()),
                "{cell:?} {parity}"
            );
            eprintln!(
                "{cell:?} parity {parity}: {before} -> {after} bytes after CRF 28, {} bytes repaired",
                decoded.repaired_bytes
            );
        }
    }
}

#[test]
fn an_encrypted_compressed_robust_video_survives_recompression() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mp4");
    let protection = Protection {
        password: Some("robust, encrypted and secret".into()),
        key_file: None,
    };
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.mode = robust(Cell::C8);
    options.compress = true;
    options.protection = Some(protection.clone());
    options.verify = true;
    run_encode(&options).unwrap();

    let squeezed = work.path().join("squeezed.mp4");
    reencode(&tools, &video, &squeezed, &["-crf", "28"]);
    let info = atv_core::probe(&squeezed, &tools).unwrap();
    assert!(info.encrypted && info.needs_password && info.compressed);
    assert_eq!(
        info.mode,
        robust(Cell::C8),
        "the header survives re-compression too"
    );

    let restored = work.path().join("restored");
    let wrong = Protection {
        password: Some("not it".into()),
        key_file: None,
    };
    let result = decode_with(&tools, &squeezed, &restored, Some(&wrong));
    assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");
    decode_with(&tools, &squeezed, &restored, Some(&protection)).unwrap();
    assert_eq!(snapshot(&restored), snapshot(source.path()));
}

#[test]
fn frames_repeated_by_a_site_are_skipped() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mp4");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.mode = robust(Cell::C4);
    let report = run_encode(&options).unwrap();

    // Converting 30 frames per second to 60 shows every frame twice.
    let doubled = work.path().join("doubled.mp4");
    reencode(&tools, &video, &doubled, &["-crf", "23", "-r", "60"]);
    let info = atv_core::probe(&doubled, &tools).unwrap();
    assert!(
        info.frames.unwrap() >= 2 * report.frames - 2,
        "the copy has about twice the frames: {:?} vs {}",
        info.frames,
        report.frames
    );
    let restored = work.path().join("restored");
    decode(&tools, &doubled, &restored).unwrap();
    assert_eq!(snapshot(&restored), snapshot(source.path()));
}

#[test]
fn frames_dropped_by_a_site_are_reported_as_damage() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mp4");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.mode = robust(Cell::C4);
    run_encode(&options).unwrap();

    // Converting 30 frames per second to 15 throws every other frame away.
    let halved = work.path().join("halved.mp4");
    reencode(&tools, &video, &halved, &["-crf", "23", "-r", "15"]);
    let restored = work.path().join("restored");
    let result = decode(&tools, &halved, &restored);
    assert!(
        matches!(result, Err(AtvError::Damaged { .. })),
        "{result:?}"
    );
    assert!(
        !restored.exists(),
        "nothing is restored from an incomplete video"
    );
}

#[test]
fn a_video_squeezed_far_too_hard_fails_cleanly() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mp4");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.mode = robust(Cell::C4);
    run_encode(&options).unwrap();

    // 200 kbit/s is a small fraction of the data rate: nothing can survive it.
    let crushed = work.path().join("crushed.mp4");
    reencode(
        &tools,
        &video,
        &crushed,
        &["-b:v", "200k", "-maxrate", "200k", "-bufsize", "400k"],
    );
    let restored = work.path().join("restored");
    let result = decode(&tools, &crushed, &restored);
    assert!(
        result.is_err(),
        "wrong output must never be produced: {result:?}"
    );
    assert!(!restored.exists());
}

#[test]
fn a_slower_frame_rate_makes_a_longer_video_with_the_same_frames() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let inputs = items_in(source.path());
    let work = tempfile::tempdir().unwrap();

    let normal = work.path().join("normal.mp4");
    let mut options = encode_options(&tools, &inputs, &normal, Resolution::P1080);
    options.mode = robust(Cell::C8);
    let at_30 = run_encode(&options).unwrap();
    assert_eq!(at_30.frame_rate, 30);

    let slow = work.path().join("slow.mp4");
    options.output = slow.clone();
    options.frame_rate = Some(10);
    let at_10 = run_encode(&options).unwrap();
    assert_eq!(at_10.frame_rate, 10);
    assert_eq!(at_10.frames, at_30.frames);
    assert!((at_10.seconds - 3.0 * at_30.seconds).abs() < 1e-6);
    let info = atv_core::probe(&slow, &tools).unwrap();
    assert!(
        (info.seconds.unwrap() - at_10.seconds).abs() < 0.2,
        "{info:?}"
    );

    let restored = work.path().join("restored");
    decode(&tools, &slow, &restored).unwrap();
    assert_eq!(snapshot(&restored), snapshot(source.path()));

    // Lossless has one frame rate; asking for another is refused.
    let lossless = work.path().join("l.mkv");
    let mut options = encode_options(&tools, &inputs, &lossless, Resolution::P1080);
    options.frame_rate = Some(10);
    let result = run_encode(&options);
    assert!(
        matches!(result, Err(AtvError::InvalidInput { .. })),
        "{result:?}"
    );
    options.mode = robust(Cell::C8);
    options.output = work.path().join("r.mp4");
    options.frame_rate = Some(0);
    assert!(matches!(
        run_encode(&options),
        Err(AtvError::InvalidInput { .. })
    ));
}

#[test]
fn robust_videos_must_be_mp4_files() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.mode = robust(Cell::C8);
    let result = run_encode(&options);
    assert!(
        matches!(result, Err(AtvError::InvalidInput { .. })),
        "{result:?}"
    );
    assert!(!video.exists());
}

#[test]
fn damage_to_a_copy_is_measured() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mp4");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.mode = robust(Cell::C4);
    let report = run_encode(&options).unwrap();
    let data_frames = report.frames - 2;

    // The video against itself: nothing wrong.
    let same = atv_core::measure_damage(&video, &video, &tools).unwrap();
    assert_eq!(same.original_frames, data_frames);
    assert_eq!(
        (same.compared, same.missing, same.beyond_repair),
        (data_frames, 0, 0)
    );
    assert_eq!(same.mean_ber, 0.0);

    // A milder second compression: a few wrong bits, nothing past repair.
    let squeezed = work.path().join("squeezed.mp4");
    reencode(&tools, &video, &squeezed, &["-crf", "28"]);
    let mild = atv_core::measure_damage(&video, &squeezed, &tools).unwrap();
    assert_eq!(mild.missing, 0);
    assert_eq!(mild.beyond_repair, 0);
    assert!(mild.mean_ber < 0.001, "{mild:?}");

    // Every other frame dropped: the losses are counted.
    let halved = work.path().join("halved.mp4");
    reencode(&tools, &video, &halved, &["-crf", "23", "-r", "15"]);
    let dropped = atv_core::measure_damage(&video, &halved, &tools).unwrap();
    assert!(dropped.missing > 0, "{dropped:?}");

    // A copy shrunk to 720p is measured scaled back.
    let small = work.path().join("small.mp4");
    reencode(&tools, &video, &small, &["-vf", "scale=1280:720"]);
    let shrunk = atv_core::measure_damage(&video, &small, &tools).unwrap();
    assert_eq!(shrunk.copy_size, (1280, 720));
    assert_eq!((shrunk.missing, shrunk.beyond_repair), (0, 0), "{shrunk:?}");

    // Another shape cannot be a copy of it.
    let square = work.path().join("square.mp4");
    reencode(&tools, &video, &square, &["-vf", "scale=1280:1024"]);
    let result = atv_core::measure_damage(&video, &square, &tools);
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
}

#[test]
fn a_4k_video_shrunk_to_exactly_1080p_is_restored() {
    // Found when comparing ATV with YtDisk: a 4K video at exactly 1920 x 1080
    // (YouTube serves that copy while it still makes the 4K one) has an ATV
    // size, so it used to be read as a 1080p video and refused. The other
    // way round, a 1080p video enlarged to 4K, is covered too.
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    for (resolution, copy_size, cell) in [
        (Resolution::P2160, "1920:1080", Cell::C8),
        (Resolution::P1080, "3840:2160", Cell::C8),
    ] {
        let work = tempfile::tempdir().unwrap();
        let video = work.path().join("v.mp4");
        let mut options = encode_options(&tools, &items_in(source.path()), &video, resolution);
        options.mode = robust(cell);
        let report = run_encode(&options).unwrap();

        let copy = work.path().join("copy.mp4");
        let filter = format!("scale={copy_size}");
        reencode(&tools, &video, &copy, &["-vf", &filter, "-crf", "23"]);

        let info = atv_core::probe(&copy, &tools).unwrap();
        assert_eq!(info.resolution, resolution, "{copy_size}");
        assert!(info.resized_from.is_some(), "{copy_size}");

        let restored = work.path().join("restored");
        let decoded = decode_with(&tools, &copy, &restored, None)
            .unwrap_or_else(|error| panic!("{copy_size}: {error}"));
        assert_eq!(decoded.sha256, report.sha256);
        assert_eq!(snapshot(&restored), snapshot(source.path()), "{copy_size}");
    }
}

#[test]
fn copies_shrunk_to_720p_by_a_site_are_restored() {
    // What YouTube did in the real-site test (spec 8.4): 1080p and 4K
    // uploads came back as 720p H.264.
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    small_tree(source.path());
    for (resolution, cell) in [
        (Resolution::P1080, Cell::C8),
        (Resolution::P1080, Cell::C4),
        (Resolution::P2160, Cell::C8),
    ] {
        let work = tempfile::tempdir().unwrap();
        let video = work.path().join("v.mp4");
        let mut options = encode_options(&tools, &items_in(source.path()), &video, resolution);
        options.mode = robust(cell);
        options.protection = Some(Protection {
            password: Some("shrunk to 720p but still secret".into()),
            key_file: None,
        });
        let report = run_encode(&options).unwrap();

        let shrunk = work.path().join("youtube.mp4");
        reencode(
            &tools,
            &video,
            &shrunk,
            &["-vf", "scale=1280:720", "-crf", "23"],
        );

        let info = atv_core::probe(&shrunk, &tools).unwrap();
        assert_eq!(
            info.resized_from,
            Some((1280, 720)),
            "{resolution:?} {cell:?}"
        );
        assert_eq!(
            info.resolution, resolution,
            "the original size is found from the header"
        );
        assert_eq!(info.mode, robust(cell));
        assert!(info.encrypted && info.needs_password);

        let restored = work.path().join("restored");
        let protection = Protection {
            password: Some("shrunk to 720p but still secret".into()),
            key_file: None,
        };
        let decoded = decode_with(&tools, &shrunk, &restored, Some(&protection))
            .unwrap_or_else(|error| panic!("{resolution:?} {cell:?}: {error}"));
        assert_eq!(decoded.sha256, report.sha256);
        assert_eq!(
            snapshot(&restored),
            snapshot(source.path()),
            "{resolution:?} {cell:?}"
        );
    }
}
