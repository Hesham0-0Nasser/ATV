//! End-to-end tests for compression, encryption and verify-after-encode,
//! through the real ffmpeg. Each test is skipped, with a message, when ffmpeg
//! and ffprobe are not installed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use atv_core::{AtvError, CancelToken, Event, Protection, Resolution, Stage};
use common::{
    FRAME_1080, decode_options, decode_with, encode_options, items_in, noise, run_encode,
    sample_tree, snapshot, tools,
};

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

fn new_key_file(dir: &Path, name: &str) -> Protection {
    let path = dir.join(name);
    atv_core::generate_key_file(&path).unwrap();
    Protection {
        password: None,
        key_file: Some(path),
    }
}

#[test]
fn every_protection_and_compression_combination_round_trips() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    sample_tree(source.path());
    let inputs = items_in(source.path());
    let keys = tempfile::tempdir().unwrap();
    let key_only = new_key_file(keys.path(), "k.atvkey");
    let both = Protection {
        password: Some("p\u{00e4}ssw\u{00f6}rd with spaces".into()),
        key_file: key_only.key_file.clone(),
    };
    let protections = [
        ("none", None),
        ("password", Some(password("correct horse battery staple"))),
        ("key file", Some(key_only)),
        ("both", Some(both)),
    ];

    for compress in [false, true] {
        for (name, protection) in &protections {
            let work = tempfile::tempdir().unwrap();
            let video = work.path().join("v.mkv");
            let mut options = encode_options(&tools, &inputs, &video, Resolution::P1080);
            options.compress = compress;
            options.protection = protection.clone();
            options.verify = true;
            let report = run_encode(&options)
                .unwrap_or_else(|error| panic!("{name}, compress {compress}: {error}"));
            assert!(report.verified);
            assert_eq!(report.compressed, compress);
            assert_eq!(report.encrypted, protection.is_some());

            let info = atv_core::probe(&video, &tools).unwrap();
            assert_eq!(info.compressed, compress);
            assert_eq!(info.encrypted, protection.is_some());
            let expects_password = protection.as_ref().is_some_and(|p| p.password.is_some());
            let expects_key_file = protection.as_ref().is_some_and(|p| p.key_file.is_some());
            assert_eq!(info.needs_password, expects_password, "{name}");
            assert_eq!(info.needs_key_file, expects_key_file, "{name}");

            let restored = work.path().join("restored");
            let decoded = decode_with(&tools, &video, &restored, protection.as_ref())
                .unwrap_or_else(|error| panic!("{name}, compress {compress}: {error}"));
            assert_eq!(decoded.sha256, report.sha256);
            assert_eq!(
                snapshot(&restored),
                snapshot(source.path()),
                "{name}, {compress}"
            );
        }
    }
}

#[test]
fn an_encrypted_compressed_video_round_trips_at_4k() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    sample_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let protection = password("4k password");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P2160);
    options.compress = true;
    options.protection = Some(protection.clone());
    options.verify = true;
    run_encode(&options).unwrap();
    let restored = work.path().join("restored");
    decode_with(&tools, &video, &restored, Some(&protection)).unwrap();
    assert_eq!(snapshot(&restored), snapshot(source.path()));
}

#[test]
fn wrong_or_missing_secrets_are_refused_before_anything_is_written() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    sample_tree(source.path());
    let keys = tempfile::tempdir().unwrap();
    let right_key = new_key_file(keys.path(), "right.atvkey");
    let wrong_key = new_key_file(keys.path(), "wrong.atvkey");
    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("out");

    // Locked with a password.
    let video = work.path().join("password.mkv");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.protection = Some(password("right"));
    run_encode(&options).unwrap();
    let result = decode_with(&tools, &video, &out, Some(&password("wrong")));
    assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");
    let result = decode_with(&tools, &video, &out, None);
    assert!(
        matches!(
            result,
            Err(AtvError::KeyRequired {
                password: true,
                key_file: false
            })
        ),
        "{result:?}"
    );
    // A key file does not stand in for the password.
    let result = decode_with(&tools, &video, &out, Some(&right_key));
    assert!(
        matches!(result, Err(AtvError::KeyRequired { .. })),
        "{result:?}"
    );

    // Locked with a key file.
    let video = work.path().join("keyfile.mkv");
    options.output = video.clone();
    options.protection = Some(right_key.clone());
    run_encode(&options).unwrap();
    let result = decode_with(&tools, &video, &out, Some(&wrong_key));
    assert!(matches!(result, Err(AtvError::WrongKey)), "{result:?}");
    let result = decode_with(&tools, &video, &out, Some(&password("a password instead")));
    assert!(
        matches!(
            result,
            Err(AtvError::KeyRequired {
                password: false,
                key_file: true
            })
        ),
        "{result:?}"
    );
    assert!(!out.exists(), "no attempt created the output folder");

    // And the right one still works after all those refusals.
    decode_with(&tools, &video, &out, Some(&right_key)).unwrap();
    assert_eq!(snapshot(&out), snapshot(source.path()));
}

#[test]
fn encryption_without_a_secret_is_refused() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    sample_tree(source.path());
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    for empty in [Protection::default(), password("")] {
        options.protection = Some(empty);
        let result = run_encode(&options);
        assert!(
            matches!(result, Err(AtvError::InvalidInput { .. })),
            "{result:?}"
        );
        assert!(!video.exists());
    }
}

#[test]
fn an_encrypted_video_file_reveals_no_names_or_content() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("very-secret-name.txt");
    fs::write(&file, "TOP-SECRET-CONTENT ".repeat(40_000)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let contains = |video: &Path, needle: &[u8]| {
        let bytes = fs::read(video).unwrap();
        bytes.windows(needle.len()).any(|window| window == needle)
    };

    let video = work.path().join("secret.mkv");
    let mut options = encode_options(
        &tools,
        std::slice::from_ref(&file),
        &video,
        Resolution::P1080,
    );
    options.protection = Some(password("pw"));
    run_encode(&options).unwrap();
    assert!(!contains(&video, b"very-secret-name"));
    assert!(!contains(&video, b"TOP-SECRET"));

    // The info a stranger can read says only how the video is stored.
    let info = atv_core::probe(&video, &tools).unwrap();
    assert!(info.encrypted && info.needs_password && !info.needs_key_file);
}

#[test]
fn compression_shrinks_a_compressible_input() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("log.txt");
    fs::write(
        &file,
        "2026-11-12 12:00:00 INFO nothing happened\n".repeat(200_000),
    )
    .unwrap();
    let work = tempfile::tempdir().unwrap();

    let plain_video = work.path().join("plain.mkv");
    let plain = run_encode(&encode_options(
        &tools,
        std::slice::from_ref(&file),
        &plain_video,
        Resolution::P1080,
    ))
    .unwrap();

    let packed_video = work.path().join("packed.mkv");
    let mut options = encode_options(
        &tools,
        std::slice::from_ref(&file),
        &packed_video,
        Resolution::P1080,
    );
    options.compress = true;
    options.verify = true;
    let packed = run_encode(&options).unwrap();

    assert!(
        plain.frames > 30,
        "{} frames without compression",
        plain.frames
    );
    assert_eq!(
        packed.frames, 3,
        "8 MB of repeated text fits one frame once compressed"
    );
    assert_eq!(packed.sha256, plain.sha256);
    let restored = work.path().join("restored");
    decode_with(&tools, &packed_video, &restored, None).unwrap();
    assert_eq!(snapshot(&restored), snapshot(source.path()));
}

#[test]
fn verify_is_reported_as_its_own_stage_with_exact_frame_counts() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("v.bin");
    fs::write(&file, noise(8 * FRAME_1080, 31)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let mut options = encode_options(&tools, &[file], &video, Resolution::P1080);
    options.verify = true;
    options.protection = Some(password("pw"));

    let mut events: Vec<Event> = Vec::new();
    let report = atv_core::encode(
        &options,
        &mut |event| events.push(event),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(report.verified);
    let stages: Vec<Stage> = events.iter().map(|event| event.stage).collect();
    let mut order = stages.clone();
    order.dedup();
    assert_eq!(
        order,
        [Stage::Preparing, Stage::WritingFrames, Stage::Verifying]
    );

    let data_frames = report.frames - 2;
    let verifying: Vec<&Event> = events
        .iter()
        .filter(|event| event.stage == Stage::Verifying)
        .collect();
    assert_eq!(
        verifying.first().unwrap().frames_done,
        0,
        "the count starts again"
    );
    assert_eq!(verifying.last().unwrap().frames_done, data_frames);
    assert!(
        verifying
            .iter()
            .all(|event| event.frames_total == Some(data_frames))
    );
    assert!(verifying.iter().any(|event| event.preview.is_some()));
}

#[test]
fn cancelling_during_verify_removes_the_video() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("v.bin");
    fs::write(&file, noise(40 * FRAME_1080, 32)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let mut options = encode_options(&tools, &[file], &video, Resolution::P1080);
    options.verify = true;

    let cancel = CancelToken::new();
    let result = atv_core::encode(
        &options,
        &mut |event| {
            if event.stage == Stage::Verifying && event.frames_done >= 3 {
                cancel.cancel();
            }
        },
        &cancel,
    );
    assert!(matches!(result, Err(AtvError::Cancelled)), "{result:?}");
    assert!(
        !video.exists(),
        "a video that was never verified is not kept"
    );
}

#[test]
fn damage_to_an_encrypted_video_file_is_caught() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("v.bin");
    fs::write(&file, noise(8 * FRAME_1080, 33)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let protection = password("pw");
    let mut options = encode_options(&tools, &[file], &video, Resolution::P1080);
    options.protection = Some(protection.clone());
    run_encode(&options).unwrap();
    let bytes = fs::read(&video).unwrap();
    let out = work.path().join("out");

    // A flipped byte in the middle of the file.
    let mut flipped = bytes.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0x55;
    let damaged = work.path().join("flipped.mkv");
    fs::write(&damaged, &flipped).unwrap();
    let result = decode_with(&tools, &damaged, &out, Some(&protection));
    assert!(result.is_err(), "{result:?}");

    // The file cut off part-way.
    let cut = work.path().join("cut.mkv");
    fs::write(&cut, &bytes[..bytes.len() * 2 / 3]).unwrap();
    let result = decode_with(&tools, &cut, &out, Some(&protection));
    assert!(matches!(result, Err(AtvError::Truncated)), "{result:?}");
    assert!(!out.exists(), "damage never produces files");
}

#[test]
fn a_decode_reports_which_secrets_are_missing_through_the_options_too() {
    let Some(tools) = tools() else { return };
    let source = tempfile::tempdir().unwrap();
    sample_tree(source.path());
    let keys = tempfile::tempdir().unwrap();
    let both = Protection {
        password: Some("pw".into()),
        key_file: new_key_file(keys.path(), "k.atvkey").key_file,
    };
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mkv");
    let mut options = encode_options(&tools, &items_in(source.path()), &video, Resolution::P1080);
    options.protection = Some(both.clone());
    run_encode(&options).unwrap();

    let out = work.path().join("out");
    let decode = decode_options(&tools, &video, &out, Some(&password("pw")));
    let result = atv_core::decode(&decode, &mut |_| {}, &CancelToken::new());
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
    decode_with(&tools, &video, &out, Some(&both)).unwrap();
    assert_eq!(snapshot(&out), snapshot(source.path()));
}
