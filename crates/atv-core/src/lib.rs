//! ATV engine: turns any file or folder into a black-and-white pixel video
//! and back.
//!
//! This crate does all the real work and knows nothing about windows or
//! terminals, so any front end can drive it. The design is in
//! `docs/project-spec.md`.
//!
//! - [`find_ffmpeg`] locates the ffmpeg and ffprobe programs.
//! - [`encode`] writes a video, [`decode`] restores the files from one.
//! - [`probe`] reads a video's header; [`estimate`] predicts a video's size.
//! - [`generate_key_file`] makes a key file for encryption.
//! - [`measure_damage`] compares a copy of a video with its original (for testing sites).
//! - [`find_ytdlp`], [`youtube_details`] and [`download_youtube`] fetch a video from YouTube.
//! - [`latest_release`] and [`install_tool`] keep ffmpeg, yt-dlp and Deno up to date.
//!
//! Jobs report progress through a callback taking [`Event`] and can be stopped
//! from another thread with a [`CancelToken`]. Nothing here reads a whole input
//! into memory.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod archive;
mod crypto;
mod decode;
mod encode;
mod error;
mod ffmpeg;
mod format;
mod frames;
mod lab;
mod ldpc;
mod progress;
mod robust;
mod stream;
#[cfg(test)]
mod testutil;
mod tools;
mod types;
mod youtube;

pub use crypto::{
    MIN_PASSWORD_SCORE, PasswordStrength, Protection, SecretString, generate_key_file,
    password_strength,
};
pub use decode::{decode, probe};
pub use encode::{encode, estimate, estimate_packed, measure_packed};
pub use error::AtvError;
pub use ffmpeg::{FfmpegTools, find_ffmpeg};
pub use lab::{DamageReport, measure_damage};
pub use tools::{
    Release, Tool, install as install_tool, is_newer, latest_release, managed_program,
    program_file, program_version,
};
pub use types::{
    CancelToken, Cell, DecodeOptions, DecodeReport, EncodeOptions, EncodeReport, Estimate, Event,
    FPS, Mode, Preview, Resolution, RestoredFile, Stage, VideoCodec, VideoInfo,
};
pub use youtube::{
    DownloadProgress, OnlineVideo, YouTubeLink, YtDlp, download as download_youtube,
    find_js_runtime, find_ytdlp, video_details as youtube_details,
};
