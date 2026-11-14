//! Smart delivery's settings against local imitations of the real sites
//! (spec 8.6). No uploading: each imitation copies what the real copies
//! showed (size, codec, frame structure, quantiser) and was tuned until it
//! reproduced their wrong-bit rates. The tests use the *harsher* variant of
//! each imitation, so passing leaves a margin over the real site.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use atv_core::{Cell, FfmpegTools, Mode, Resolution, VideoCodec};
use common::{decode, encode_options, items_in, noise, run_encode, snapshot, tools};

/// WhatsApp's phone app at "Standard" quality, measured on real copies:
/// shrunk to 854 x 480, H.264 with most macroblocks at QP 51 (the maximum),
/// a full frame every 300, no B-frames. The real phone shrinks with a
/// bilinear filter; the harsher fast-bilinear one used here gave about 1.8x
/// the real damage.
const WHATSAPP_HARSH: &[&str] = &[
    "-vf",
    "scale=854:480:flags=fast_bilinear",
    "-c:v",
    "libx264",
    "-preset",
    "ultrafast",
    "-qp",
    "51",
    "-x264-params",
    "keyint=300:bframes=0",
    "-pix_fmt",
    "yuv420p",
];

/// YouTube's 720p file, measured: H.264 at QP 44-45, a full frame every 180,
/// B-frames. QP 43 reproduced the real 4K copies; QP 45 is the harsher one.
const YOUTUBE_720_HARSH: &[&str] = &[
    "-vf",
    "scale=1280:720:flags=bicubic",
    "-c:v",
    "libx264",
    "-preset",
    "slow",
    "-qp",
    "45",
    "-x264-params",
    "keyint=180:bframes=1",
    "-pix_fmt",
    "yuv420p",
];

/// YouTube's full-size copies at about half the bitrate YouTube really gave
/// ATV's 4K 4 x 4 test video (VP9 4K 36 Mbit/s, H.264 1080p 48 Mbit/s): the
/// free encoders cannot match YouTube's efficiency, so matching its bitrate
/// at half is the harsh, margin-giving imitation (spec 8.6).
const YOUTUBE_4K_HARSH: &[&str] = &[
    "-c:v",
    "libvpx-vp9",
    "-b:v",
    "20M",
    "-maxrate",
    "22M",
    "-bufsize",
    "40M",
    "-deadline",
    "good",
    "-cpu-used",
    "4",
    "-row-mt",
    "1",
    "-pix_fmt",
    "yuv420p",
];
const YOUTUBE_1080_HARSH: &[&str] = &[
    "-vf",
    "scale=1920:1080:flags=bicubic",
    "-c:v",
    "libx264",
    "-preset",
    "medium",
    "-b:v",
    "24M",
    "-maxrate",
    "24M",
    "-bufsize",
    "96M",
    "-pix_fmt",
    "yuv420p",
];

fn has_encoder(tools: &FfmpegTools, name: &str) -> bool {
    let out = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(name)
}

fn imitate(tools: &FfmpegTools, input: &Path, output: &Path, args: &[&str]) {
    let status = Command::new(&tools.ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args(args)
        .arg(output)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not make the imitation");
}

/// Encode random data with a Smart-delivery setting, pass it through an
/// imitation, and require a byte-identical restore.
fn survives(
    tools: &FfmpegTools,
    resolution: Resolution,
    mode: Mode,
    frame_rate: Option<u32>,
    codec: VideoCodec,
    site: &[&str],
    label: &str,
) {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("data.bin"), noise(300_000, 21)).unwrap();
    let work = tempfile::tempdir().unwrap();
    let video = work.path().join("v.mp4");
    let mut options = encode_options(tools, &items_in(source.path()), &video, resolution);
    options.mode = mode;
    options.frame_rate = frame_rate;
    options.video_codec = codec;
    run_encode(&options).unwrap();

    let copy = work.path().join("copy.mp4");
    imitate(tools, &video, &copy, site);
    let restored = work.path().join("restored");
    decode(tools, &copy, &restored).unwrap_or_else(|error| panic!("{label}: {error}"));
    assert_eq!(snapshot(&restored), snapshot(source.path()), "{label}");
}

#[test]
fn the_whatsapp_setting_survives_whatsapp() {
    let Some(tools) = tools() else { return };
    let mode = Mode::Robust {
        cell: Cell::C8,
        parity: 64,
    };
    survives(
        &tools,
        Resolution::P1080,
        mode,
        Some(10),
        VideoCodec::H264,
        WHATSAPP_HARSH,
        "WhatsApp",
    );
}

/// Format 3's standard repair (LDPC) survives YouTube's 720p file, so the
/// YouTube destination need not wait for the HD copy (spec 5.4b, 8.8).
#[test]
fn the_youtube_setting_survives_youtubes_720p_file() {
    let Some(tools) = tools() else { return };
    let mode = Mode::Robust {
        cell: Cell::C4,
        parity: 32, // the YouTube destination: standard repair
    };
    survives(
        &tools,
        Resolution::P2160,
        mode,
        None,
        VideoCodec::H264,
        YOUTUBE_720_HARSH,
        "YouTube 720p, H.264",
    );
    if has_encoder(&tools, "libaom-av1") {
        survives(
            &tools,
            Resolution::P2160,
            mode,
            None,
            VideoCodec::Av1,
            YOUTUBE_720_HARSH,
            "YouTube 720p, AV1",
        );
    }
}

#[test]
fn the_youtube_setting_survives_youtubes_full_size_copies() {
    let Some(tools) = tools() else { return };
    let mode = Mode::Robust {
        cell: Cell::C4,
        parity: 32, // the YouTube destination: standard repair
    };
    let codecs: Vec<VideoCodec> = if has_encoder(&tools, "libaom-av1") {
        vec![VideoCodec::H264, VideoCodec::Av1]
    } else {
        vec![VideoCodec::H264]
    };
    for codec in codecs {
        if has_encoder(&tools, "libvpx-vp9") {
            survives(
                &tools,
                Resolution::P2160,
                mode,
                None,
                codec,
                YOUTUBE_4K_HARSH,
                &format!("YouTube 4K VP9, {codec:?}"),
            );
        }
        survives(
            &tools,
            Resolution::P2160,
            mode,
            None,
            codec,
            YOUTUBE_1080_HARSH,
            &format!("YouTube 1080p, {codec:?}"),
        );
    }
}
