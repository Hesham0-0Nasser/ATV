//! Turning the engine's progress events into what the running screen shows.
//!
//! Jobs run on a worker thread (see `commands.rs`). The engine already limits
//! events to about 30 a second and previews to about 10 a second, so every
//! event is turned into a [`View`] and sent to the page as it comes.

use std::time::Instant;

use atv_core::{Event, Preview, Stage};
use base64::Engine;
use serde::Serialize;

use crate::text;

/// The numbers of an engine event, without its preview.
#[derive(Debug, Clone, Copy)]
pub struct Update {
    pub stage: Stage,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub frames_done: u64,
    pub frames_total: Option<u64>,
    pub repaired_bytes: u64,
}

impl From<&Event> for Update {
    fn from(event: &Event) -> Self {
        Update {
            stage: event.stage,
            bytes_done: event.bytes_done,
            bytes_total: event.bytes_total,
            frames_done: event.frames_done,
            frames_total: event.frames_total,
            repaired_bytes: event.repaired_bytes,
        }
    }
}

/// A preview picture for the page: one grey byte per pixel, in base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewData {
    pub width: u32,
    pub height: u32,
    pub pixels: String,
}

impl From<&Preview> for PreviewData {
    fn from(preview: &Preview) -> Self {
        PreviewData {
            width: preview.width,
            height: preview.height,
            pixels: base64::engine::general_purpose::STANDARD.encode(&preview.pixels),
        }
    }
}

/// One row of the stage list. `state`: 0 waiting, 1 current, 2 done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageView {
    pub label: &'static str,
    pub state: i32,
}

/// What the running screen shows, worked out from the latest update.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    pub progress: f32,
    pub percent: String,
    pub frame_text: String,
    pub stages: Vec<StageView>,
    pub speed: String,
    pub elapsed: String,
    pub remaining: String,
    pub repaired: String,
}

/// Which stages a job has, from its options. Steps that are switched off are not shown.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Encode {
        compress: bool,
        encrypt: bool,
        verify: bool,
    },
    Decode {
        robust: bool,
        encrypted: bool,
        compressed: bool,
    },
}

/// Keeps the times a job needs to report speed and time left.
pub struct Tracker {
    kind: Kind,
    started: Instant,
    stage: Option<Stage>,
    stage_started: Instant,
}

impl Tracker {
    pub fn new(kind: Kind) -> Self {
        let now = Instant::now();
        Tracker {
            kind,
            started: now,
            stage: None,
            stage_started: now,
        }
    }

    pub fn view(&mut self, update: &Update) -> View {
        let now = Instant::now();
        if self.stage != Some(update.stage) {
            self.stage = Some(update.stage);
            self.stage_started = now;
        }
        let elapsed = now.duration_since(self.started).as_secs_f64();
        let in_stage = now.duration_since(self.stage_started).as_secs_f64();
        view(self.kind, update, elapsed, in_stage)
    }
}

/// Share of the bar given to writing when a verify pass follows it.
const WRITE_SHARE: f64 = 0.6;

fn fraction(done: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// The pure arithmetic behind [`Tracker::view`], separate so it can be tested.
pub fn view(kind: Kind, update: &Update, elapsed: f64, in_stage: f64) -> View {
    let frames_total = update.frames_total.unwrap_or(0);
    let frame_part = fraction(update.frames_done, frames_total);

    let (progress, bytes_per_second, frame_text) = match kind {
        Kind::Encode { verify, .. } => {
            let write_share = if verify { WRITE_SHARE } else { 1.0 };
            match update.stage {
                Stage::Preparing => (0.0, 0.0, String::new()),
                Stage::WritingFrames => {
                    let part = if update.bytes_total > 0 {
                        fraction(update.bytes_done, update.bytes_total)
                    } else {
                        frame_part
                    };
                    (
                        part * write_share,
                        update.bytes_done as f64 / in_stage.max(0.001),
                        frame_line("Frame", update.frames_done, update.frames_total),
                    )
                }
                Stage::Verifying => (
                    write_share + (1.0 - write_share) * frame_part,
                    update.bytes_total as f64 * frame_part / in_stage.max(0.001),
                    frame_line("Checking frame", update.frames_done, update.frames_total),
                ),
                _ => (0.0, 0.0, String::new()),
            }
        }
        Kind::Decode { .. } => match update.stage {
            // The frame count is an estimate, so never claim to be done early.
            Stage::ReadingFrames => (
                frame_part.min(0.99),
                update.bytes_done as f64 / elapsed.max(0.001),
                frame_line("Frame", update.frames_done, update.frames_total),
            ),
            Stage::CheckingIntegrity => (
                1.0,
                update.bytes_done as f64 / elapsed.max(0.001),
                frame_line("Frame", update.frames_done, Some(update.frames_done)),
            ),
            _ => (0.0, 0.0, String::new()),
        },
    };

    let remaining = if progress > 0.02 && progress < 1.0 {
        text::clock(elapsed * (1.0 - progress) / progress)
    } else if progress >= 1.0 {
        text::clock(0.0)
    } else {
        "—".to_string()
    };
    let repaired = match kind {
        Kind::Decode { robust: true, .. } if update.repaired_bytes > 0 => {
            format!(
                "{} repaired so far",
                text::plural(update.repaired_bytes, "damaged byte")
            )
        }
        _ => String::new(),
    };

    View {
        progress: progress as f32,
        percent: text::percent(progress as f32),
        frame_text,
        stages: stages(kind, update.stage),
        speed: if bytes_per_second > 0.0 {
            text::speed(bytes_per_second)
        } else {
            "—".to_string()
        },
        elapsed: text::clock(elapsed),
        remaining,
        repaired,
    }
}

fn frame_line(label: &str, done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total >= done => format!(
            "{label} {} of {}",
            text::group_digits(done),
            text::group_digits(total)
        ),
        _ => format!("{label} {}", text::group_digits(done)),
    }
}

/// The stage list. The engine streams, so compressing, encrypting and writing
/// frames happen together and are shown as current together (spec 3.1).
pub fn stages(kind: Kind, stage: Stage) -> Vec<StageView> {
    // Each row: label, and the engine stage during which it is current.
    let mut rows: Vec<(&'static str, Stage)> = Vec::new();
    match kind {
        Kind::Encode {
            compress,
            encrypt,
            verify,
        } => {
            rows.push(("Preparing", Stage::Preparing));
            if compress {
                rows.push(("Compressing", Stage::WritingFrames));
            }
            if encrypt {
                rows.push(("Encrypting", Stage::WritingFrames));
            }
            rows.push(("Writing frames", Stage::WritingFrames));
            if verify {
                rows.push(("Verifying", Stage::Verifying));
            }
        }
        Kind::Decode {
            robust,
            encrypted,
            compressed,
        } => {
            rows.push(("Reading frames", Stage::ReadingFrames));
            if robust {
                rows.push(("Correcting errors", Stage::ReadingFrames));
            }
            if encrypted {
                rows.push(("Decrypting", Stage::ReadingFrames));
            }
            if compressed {
                rows.push(("Decompressing", Stage::ReadingFrames));
            }
            rows.push(("Checking integrity", Stage::CheckingIntegrity));
        }
    }
    let order = |stage: Stage| match stage {
        Stage::Preparing => 0,
        Stage::WritingFrames => 1,
        Stage::Verifying => 2,
        Stage::ReadingFrames => 0,
        Stage::CheckingIntegrity => 1,
    };
    let now = order(stage);
    rows.into_iter()
        .map(|(label, at)| StageView {
            label,
            state: match order(at).cmp(&now) {
                std::cmp::Ordering::Less => 2,
                std::cmp::Ordering::Equal => 1,
                std::cmp::Ordering::Greater => 0,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(stage: Stage, bytes: (u64, u64), frames: (u64, Option<u64>)) -> Update {
        Update {
            stage,
            bytes_done: bytes.0,
            bytes_total: bytes.1,
            frames_done: frames.0,
            frames_total: frames.1,
            repaired_bytes: 0,
        }
    }

    const ENCODE_ALL: Kind = Kind::Encode {
        compress: true,
        encrypt: true,
        verify: true,
    };

    fn labels_and_states(rows: &[StageView]) -> Vec<(&str, i32)> {
        rows.iter().map(|row| (row.label, row.state)).collect()
    }

    #[test]
    fn encode_stages_skip_what_is_off_and_group_the_streaming_steps() {
        let plain = Kind::Encode {
            compress: false,
            encrypt: false,
            verify: false,
        };
        assert_eq!(
            labels_and_states(&stages(plain, Stage::Preparing)),
            [("Preparing", 1), ("Writing frames", 0)]
        );
        assert_eq!(
            labels_and_states(&stages(ENCODE_ALL, Stage::WritingFrames)),
            [
                ("Preparing", 2),
                ("Compressing", 1),
                ("Encrypting", 1),
                ("Writing frames", 1),
                ("Verifying", 0)
            ]
        );
        assert_eq!(
            labels_and_states(&stages(ENCODE_ALL, Stage::Verifying)),
            [
                ("Preparing", 2),
                ("Compressing", 2),
                ("Encrypting", 2),
                ("Writing frames", 2),
                ("Verifying", 1)
            ]
        );
    }

    #[test]
    fn decode_stages_follow_the_video() {
        let robust_locked = Kind::Decode {
            robust: true,
            encrypted: true,
            compressed: false,
        };
        assert_eq!(
            labels_and_states(&stages(robust_locked, Stage::ReadingFrames)),
            [
                ("Reading frames", 1),
                ("Correcting errors", 1),
                ("Decrypting", 1),
                ("Checking integrity", 0)
            ]
        );
        let plain = Kind::Decode {
            robust: false,
            encrypted: false,
            compressed: true,
        };
        assert_eq!(
            labels_and_states(&stages(plain, Stage::CheckingIntegrity)),
            [
                ("Reading frames", 2),
                ("Decompressing", 2),
                ("Checking integrity", 1)
            ]
        );
    }

    #[test]
    fn encode_progress_splits_the_bar_between_writing_and_verifying() {
        let half_written = update(Stage::WritingFrames, (50, 100), (10, Some(20)));
        let view = view(ENCODE_ALL, &half_written, 10.0, 8.0);
        assert!((view.progress - 0.3).abs() < 1e-6, "{}", view.progress);
        assert_eq!(view.percent, "30%");
        assert_eq!(view.frame_text, "Frame 10 of 20");
        assert_eq!(view.speed, "6 B/s");

        let half_checked = update(Stage::Verifying, (100, 100), (10, Some(20)));
        let checked = super::view(ENCODE_ALL, &half_checked, 20.0, 5.0);
        assert!((checked.progress - 0.8).abs() < 1e-6);
        assert_eq!(checked.frame_text, "Checking frame 10 of 20");
        assert_eq!(checked.remaining, "0:05", "20 s for 80% leaves 5 s");

        let no_verify = Kind::Encode {
            compress: false,
            encrypt: false,
            verify: false,
        };
        let full = super::view(no_verify, &half_written, 10.0, 10.0);
        assert!((full.progress - 0.5).abs() < 1e-6);
    }

    #[test]
    fn decode_progress_never_reaches_100_before_the_check() {
        let kind = Kind::Decode {
            robust: true,
            encrypted: false,
            compressed: false,
        };
        let mut over = update(Stage::ReadingFrames, (0, 0), (120, Some(100)));
        over.repaired_bytes = 312;
        let reading = view(kind, &over, 4.0, 4.0);
        assert!(reading.progress <= 0.99);
        assert_eq!(
            reading.frame_text, "Frame 120",
            "an estimate that was too low is not shown"
        );
        assert_eq!(reading.repaired, "312 damaged bytes repaired so far");

        let checking = view(
            kind,
            &update(Stage::CheckingIntegrity, (5, 0), (100, Some(100))),
            5.0,
            0.1,
        );
        assert_eq!(checking.percent, "100%");
    }

    #[test]
    fn nothing_divides_by_zero() {
        let start = view(
            ENCODE_ALL,
            &update(Stage::Preparing, (0, 0), (0, None)),
            0.0,
            0.0,
        );
        assert_eq!(start.percent, "0%");
        assert_eq!(start.speed, "—");
        assert_eq!(start.remaining, "—");
        let empty_input = view(
            ENCODE_ALL,
            &update(Stage::WritingFrames, (0, 0), (0, Some(1))),
            0.0,
            0.0,
        );
        assert_eq!(empty_input.percent, "0%");
    }

    #[test]
    fn previews_travel_as_base64_grey_bytes() {
        let preview = Preview {
            width: 2,
            height: 1,
            pixels: vec![0, 255],
        };
        let data = PreviewData::from(&preview);
        assert_eq!((data.width, data.height), (2, 1));
        assert_eq!(data.pixels, "AP8=");
    }
}
