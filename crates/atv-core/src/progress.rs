//! Shared state for one job: progress counters, the event callback, the
//! cancel flag, and the first engine error raised from inside a stream.
//!
//! The pipeline is a chain of `Read`/`Write` adapters, which can only return
//! `io::Error`. When a layer needs to report an [`AtvError`] it parks it here
//! with [`Ctx::fail`] and returns a plain `io::Error`; the top of the job swaps
//! the real error back in with [`Ctx::resolve`]. This does not depend on how
//! other crates wrap or forward `io::Error`.

use std::cell::{Cell, RefCell};
use std::io;
use std::time::{Duration, Instant};

use crate::error::AtvError;
use crate::types::{CancelToken, Event, Preview, Stage};

/// Progress events are sent at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(33);
/// Preview images are made at most this often.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) struct Ctx<'a> {
    on_event: RefCell<&'a mut dyn FnMut(Event)>,
    cancel: &'a CancelToken,
    failure: RefCell<Option<AtvError>>,
    stage: Cell<Stage>,
    bytes_done: Cell<u64>,
    bytes_total: Cell<u64>,
    frames_done: Cell<u64>,
    frames_total: Cell<Option<u64>>,
    repaired_bytes: Cell<u64>,
    last_progress: Cell<Option<Instant>>,
    last_preview: Cell<Option<Instant>>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(
        on_event: &'a mut dyn FnMut(Event),
        cancel: &'a CancelToken,
        stage: Stage,
    ) -> Self {
        Ctx {
            on_event: RefCell::new(on_event),
            cancel,
            failure: RefCell::new(None),
            stage: Cell::new(stage),
            bytes_done: Cell::new(0),
            bytes_total: Cell::new(0),
            frames_done: Cell::new(0),
            frames_total: Cell::new(None),
            repaired_bytes: Cell::new(0),
            last_progress: Cell::new(None),
            last_preview: Cell::new(None),
        }
    }

    /// Park an engine error and get an `io::Error` to return through a stream.
    /// Only the first error is kept; it is the cause of whatever follows.
    pub(crate) fn fail(&self, error: AtvError) -> io::Error {
        let mut slot = self.failure.borrow_mut();
        if slot.is_none() {
            *slot = Some(error);
        }
        io::Error::other("the ATV pipeline stopped")
    }

    /// The parked error if there is one, otherwise `fallback`.
    pub(crate) fn resolve(&self, fallback: AtvError) -> AtvError {
        self.failure.borrow_mut().take().unwrap_or(fallback)
    }

    /// Returns an error (and parks `Cancelled`) once the job has been cancelled.
    pub(crate) fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            Err(self.fail(AtvError::Cancelled))
        } else {
            Ok(())
        }
    }

    /// Move to a new stage and tell the listener straight away.
    pub(crate) fn set_stage(&self, stage: Stage) {
        self.stage.set(stage);
        self.emit(None);
    }

    pub(crate) fn set_bytes_total(&self, total: u64) {
        self.bytes_total.set(total);
    }

    pub(crate) fn set_frames_total(&self, total: Option<u64>) {
        self.frames_total.set(total);
    }

    /// Data frames counted so far.
    pub(crate) fn frames_done(&self) -> u64 {
        self.frames_done.get()
    }

    /// Start counting frames from zero again (used when the verify pass begins).
    pub(crate) fn reset_frames(&self) {
        self.frames_done.set(0);
    }

    pub(crate) fn add_bytes(&self, count: u64) {
        self.bytes_done
            .set(self.bytes_done.get().saturating_add(count));
        self.emit_throttled();
    }

    pub(crate) fn add_repaired(&self, count: u64) {
        self.repaired_bytes
            .set(self.repaired_bytes.get().saturating_add(count));
    }

    /// Forget repairs counted so far (used when the verify pass begins).
    pub(crate) fn reset_repaired(&self) {
        self.repaired_bytes.set(0);
    }

    pub(crate) fn repaired_bytes(&self) -> u64 {
        self.repaired_bytes.get()
    }

    /// Count one data frame. `make_preview` is only called when a preview is due.
    pub(crate) fn frame_done(&self, make_preview: impl FnOnce() -> Preview) {
        self.frames_done
            .set(self.frames_done.get().saturating_add(1));
        let now = Instant::now();
        let due = self
            .last_preview
            .get()
            .is_none_or(|last| now.duration_since(last) >= PREVIEW_INTERVAL);
        if due {
            self.last_preview.set(Some(now));
            self.emit(Some(make_preview()));
        } else {
            self.emit_throttled();
        }
    }

    /// Send the current numbers now, ignoring the rate limit.
    pub(crate) fn emit_now(&self) {
        self.emit(None);
    }

    fn emit_throttled(&self) {
        let now = Instant::now();
        let due = self
            .last_progress
            .get()
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL);
        if due {
            self.emit(None);
        }
    }

    fn emit(&self, preview: Option<Preview>) {
        self.last_progress.set(Some(Instant::now()));
        let event = Event {
            stage: self.stage.get(),
            bytes_done: self.bytes_done.get(),
            bytes_total: self.bytes_total.get(),
            frames_done: self.frames_done.get(),
            frames_total: self.frames_total.get(),
            repaired_bytes: self.repaired_bytes.get(),
            preview,
        };
        (self.on_event.borrow_mut())(event);
    }
}
