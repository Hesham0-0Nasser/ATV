//! Everything the page can ask the Rust side to do (spec 9.7: "Tauri commands
//! call the same core functions and forward Events to the web view").
//!
//! Commands that open a file dialog or run ffmpeg are `async`, so Tauri runs
//! them off the main thread; the dialogs' blocking calls must never run there.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;

use atv_core::{
    AtvError, CancelToken, DecodeOptions, DecodeReport, DownloadProgress, EncodeOptions,
    EncodeReport, FfmpegTools, Mode, Protection, Release, SecretString, Tool, YouTubeLink, YtDlp,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use crate::files::{self, Item};
use crate::jobs::{Kind, PreviewData, Tracker, Update, View};
use crate::messages::{self, ErrorView};
use crate::platform::{self, Look};
use crate::store::{self, Appearance, Direction, Entry, Locations, Material, Outcome, Settings};
use crate::text;

/// One tool's newest official release, `None` where ATV does not install
/// it, or why the check failed.
type LatestCheck = Result<Option<Release>, String>;

/// Everything the app keeps between commands.
pub struct Shared {
    pub locations: Option<Locations>,
    pub settings: Mutex<Settings>,
    pub history: Mutex<Vec<Entry>>,
    pub tools: Mutex<Option<FfmpegTools>>,
    /// yt-dlp, once found.
    pub ytdlp: Mutex<Option<YtDlp>>,
    /// The newest official release of each tool, from the last check.
    pub latest: Mutex<Vec<(Tool, LatestCheck)>>,
    /// The running job's cancel button. One job at a time (spec 9.5).
    pub job: Mutex<Option<CancelToken>>,
    /// Set when the window was closed during a job: quit once it has cleaned up.
    pub quit_after_job: AtomicBool,
    /// The background measurement of the Encode items' compressed size, if
    /// one is running. A new one, or starting an encode, stops it.
    pub measure: Mutex<Option<CancelToken>>,
}

impl Shared {
    pub fn load() -> Self {
        let locations = Locations::for_this_user();
        let settings = locations
            .as_ref()
            .map(|places| store::load_settings(&places.settings))
            .unwrap_or_default();
        let history = locations
            .as_ref()
            .map(|places| store::load_history(&places.history))
            .unwrap_or_default();
        Shared {
            locations,
            settings: Mutex::new(settings),
            history: Mutex::new(history),
            tools: Mutex::new(None),
            ytdlp: Mutex::new(None),
            latest: Mutex::new(Vec::new()),
            job: Mutex::new(None),
            quit_after_job: AtomicBool::new(false),
            measure: Mutex::new(None),
        }
    }

    fn save_settings(&self) {
        if let Some(places) = &self.locations {
            // Failing to save is not worth interrupting the person for.
            let _ = store::save_settings(&places.settings, &lock(&self.settings));
        }
    }

    fn save_history(&self) {
        if let Some(places) = &self.locations {
            let _ = store::save_history(&places.history, &lock(&self.history));
        }
    }

    pub fn job_running(&self) -> bool {
        lock(&self.job).is_some()
    }

    pub fn cancel_job(&self) {
        if let Some(cancel) = lock(&self.job).as_ref() {
            cancel.cancel();
        }
    }
}

/// A lock that still works after a panic elsewhere: the data is plain values.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run blocking work off the async runtime's own threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

// ------------------------------------------------------------------ start-up

#[derive(Serialize)]
pub struct SettingsView {
    appearance: Appearance,
    material: Material,
    resolution: String,
    mode: String,
    cell: u32,
    parity: u32,
    compress: bool,
    smallest: bool,
    destination: String,
    verify: bool,
    output_folder: String,
    output_folder_friendly: String,
    ffmpeg_path_saved: bool,
    setup_done: bool,
}

fn settings_view(settings: &Settings) -> SettingsView {
    let folder = settings.output_folder();
    SettingsView {
        appearance: settings.appearance,
        material: settings.material,
        resolution: settings.resolution.clone(),
        mode: settings.mode.clone(),
        cell: settings.cell,
        parity: settings.parity,
        compress: settings.compress,
        smallest: settings.smallest,
        destination: settings.destination.clone(),
        verify: settings.verify,
        output_folder: path_text(&folder),
        output_folder_friendly: files::friendly_folder(&folder),
        ffmpeg_path_saved: settings.ffmpeg_path.is_some(),
        setup_done: settings.setup_done,
    }
}

#[derive(Serialize)]
pub struct Startup {
    version: &'static str,
    os: &'static str,
    reveal_label: &'static str,
    install_hint: &'static str,
    install_command: &'static str,
    look: Look,
    settings: SettingsView,
    history: Vec<HistoryRow>,
}

#[tauri::command]
pub fn startup(state: State<'_, Shared>) -> Startup {
    let (install_hint, install_command) = platform::install_hint();
    Startup {
        version: env!("CARGO_PKG_VERSION"),
        os: platform::os_name(),
        reveal_label: platform::reveal_label(),
        install_hint,
        install_command,
        look: platform::look(),
        settings: settings_view(&lock(&state.settings)),
        history: history_rows(&lock(&state.history)),
    }
}

/// Show the window once the page has drawn itself, so it never flashes blank.
#[tauri::command]
pub fn page_ready(window: WebviewWindow) {
    let _ = window.show();
    let _ = window.set_focus();
}

// --------------------------------------------------------------- appearance

/// Follow the system, or force light or dark, and choose the window's
/// material. The window's own theme and the Windows material are changed to
/// match, so the glass agrees with the page.
#[tauri::command]
pub fn set_appearance(window: WebviewWindow, appearance: Appearance, material: Material) {
    apply_appearance(&window, appearance, material);
}

pub fn apply_appearance(window: &WebviewWindow, appearance: Appearance, material: Material) {
    let theme = match appearance {
        Appearance::System => None,
        Appearance::Light => Some(tauri::Theme::Light),
        Appearance::Dark => Some(tauri::Theme::Dark),
    };
    let _ = window.set_theme(theme);
    if cfg!(windows) && platform::look().transparency {
        use tauri::utils::config::WindowEffectsConfig;
        use tauri::window::Effect;
        // Acrylic follows the window's theme set above; Mica has its own
        // light and dark kinds. Acrylic stutters on builds before 22621, so
        // those get Mica instead. Solid has no material: the page is opaque.
        let mica = match appearance {
            Appearance::System => Effect::Mica,
            Appearance::Light => Effect::MicaLight,
            Appearance::Dark => Effect::MicaDark,
        };
        let effect = match material {
            Material::Acrylic if platform::acrylic_supported() => Some(Effect::Acrylic),
            Material::Acrylic | Material::Mica => Some(mica),
            Material::Solid => None,
        };
        let _ = window.set_effects(effect.map(|effect| WindowEffectsConfig {
            effects: vec![effect],
            state: None,
            radius: None,
            color: None,
            interactive: false,
        }));
    }
}

// ------------------------------------------------------------------- ffmpeg

#[derive(Serialize)]
pub struct FfmpegStatus {
    found: bool,
    version: String,
    path: String,
    problem: String,
    saved: bool,
    /// This ffmpeg can write AV1 ("Smaller upload").
    av1: bool,
}

fn ffmpeg_status(app: &AppHandle, problem: Option<String>) -> FfmpegStatus {
    let state = app.state::<Shared>();
    let saved = lock(&state.settings).ffmpeg_path.clone();
    let tools = lock(&state.tools).clone();
    match tools {
        Some(tools) => FfmpegStatus {
            found: true,
            version: tools.version.clone(),
            path: path_text(&tools.ffmpeg),
            problem: problem.unwrap_or_default(),
            saved: saved.is_some(),
            av1: tools.av1,
        },
        None => FfmpegStatus {
            found: false,
            version: String::new(),
            path: saved.as_deref().map(path_text).unwrap_or_default(),
            problem: problem.unwrap_or_default(),
            saved: saved.is_some(),
            av1: false,
        },
    }
}

/// Look for ffmpeg: the path saved in Settings, then the usual places.
#[tauri::command]
pub async fn check_ffmpeg(app: AppHandle) -> Result<FfmpegStatus, String> {
    blocking(move || {
        let state = app.state::<Shared>();
        let start = ffmpeg_start(&state);
        let result = atv_core::find_ffmpeg(start.as_deref());
        let problem = result.as_ref().err().map(messages::ffmpeg_problem);
        *lock(&state.tools) = result.ok();
        ffmpeg_status(&app, problem)
    })
    .await
}

/// Choose ffmpeg by hand. It is kept only if that exact program passes the checks.
#[tauri::command]
pub async fn choose_ffmpeg(app: AppHandle) -> Result<FfmpegStatus, String> {
    blocking(move || {
        let mut dialog = app.dialog().file().set_title("Choose the ffmpeg program");
        if cfg!(windows) {
            dialog = dialog.add_filter("ffmpeg", &["exe"]);
        }
        let Some(chosen) = dialog.blocking_pick_file().and_then(|file| file.into_path().ok()) else {
            return ffmpeg_status(&app, None);
        };
        let state = app.state::<Shared>();
        match atv_core::find_ffmpeg(Some(&chosen)) {
            Ok(tools) if tools.ffmpeg == chosen => {
                lock(&state.settings).ffmpeg_path = Some(chosen);
                state.save_settings();
                *lock(&state.tools) = Some(tools);
                ffmpeg_status(&app, None)
            }
            Ok(_) => ffmpeg_status(
                &app,
                Some(format!(
                    "{} is not a working ffmpeg: it needs ffprobe beside it and the ffv1 and libx264 encoders.",
                    chosen.display()
                )),
            ),
            Err(error) => ffmpeg_status(&app, Some(messages::ffmpeg_problem(&error))),
        }
    })
    .await
}

#[tauri::command]
pub async fn clear_ffmpeg_path(app: AppHandle) -> Result<FfmpegStatus, String> {
    {
        let state = app.state::<Shared>();
        lock(&state.settings).ffmpeg_path = None;
        state.save_settings();
    }
    check_ffmpeg(app).await
}

// ----------------------------------------------------------------- settings

#[derive(Deserialize)]
pub struct SettingsInput {
    appearance: Appearance,
    material: Material,
    resolution: String,
    mode: String,
    cell: u32,
    parity: u32,
    compress: bool,
    smallest: bool,
    destination: String,
    verify: bool,
}

#[tauri::command]
pub fn save_settings(state: State<'_, Shared>, input: SettingsInput) -> SettingsView {
    let view = {
        let mut settings = lock(&state.settings);
        settings.appearance = input.appearance;
        settings.material = input.material;
        settings.resolution = if input.resolution == "4k" {
            "4k"
        } else {
            "1080p"
        }
        .to_string();
        settings.mode = if input.mode == "robust" {
            "robust"
        } else {
            "lossless"
        }
        .to_string();
        settings.cell = if input.cell == 4 { 4 } else { 8 };
        settings.parity = if input.parity == 32 { 32 } else { 64 };
        settings.compress = input.compress;
        settings.smallest = input.smallest;
        settings.destination = match input.destination.as_str() {
            "whatsapp" | "youtube" | "custom" => input.destination.clone(),
            _ => "keep".to_string(),
        };
        settings.verify = input.verify;
        settings_view(&settings)
    };
    state.save_settings();
    view
}

/// The setup wizard was finished or skipped: do not show it on the next start.
#[tauri::command]
pub fn finish_setup(state: State<'_, Shared>) -> SettingsView {
    let view = {
        let mut settings = lock(&state.settings);
        settings.setup_done = true;
        settings_view(&settings)
    };
    state.save_settings();
    view
}

#[tauri::command]
pub async fn choose_output_folder(app: AppHandle) -> Result<Option<SettingsView>, String> {
    blocking(move || {
        let state = app.state::<Shared>();
        let start = lock(&state.settings).output_folder();
        let folder = app
            .dialog()
            .file()
            .set_title("Choose where new videos and restored files go")
            .set_directory(start)
            .blocking_pick_folder()
            .and_then(|folder| folder.into_path().ok())?;
        let view = {
            let mut settings = lock(&state.settings);
            settings.output_folder = Some(folder);
            settings_view(&settings)
        };
        state.save_settings();
        Some(view)
    })
    .await
}

// ------------------------------------------------------------------- encode

#[tauri::command]
pub async fn pick_inputs(app: AppHandle, folder: bool) -> Result<Vec<String>, String> {
    blocking(move || {
        let dialog = app.dialog().file();
        let picked: Vec<PathBuf> = if folder {
            dialog
                .set_title("Choose a folder to put in the video")
                .blocking_pick_folders()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|path| path.into_path().ok())
                .collect()
        } else {
            dialog
                .set_title("Choose files to put in the video")
                .blocking_pick_files()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|path| path.into_path().ok())
                .collect()
        };
        picked.iter().map(|path| path_text(path)).collect()
    })
    .await
}

/// Name, kind and size of each input. Walking a large folder can take a
/// while, so this never runs on the main thread.
#[tauri::command]
pub async fn describe_items(paths: Vec<String>) -> Result<Vec<Item>, String> {
    blocking(move || {
        paths
            .iter()
            .map(|path| files::describe(Path::new(path)))
            .collect()
    })
    .await
}

#[derive(Serialize)]
pub struct Summary {
    length: String,
    size: String,
    frames: String,
}

/// Compress the Encode items once without keeping anything, to learn how
/// big the video will really be (spec 4.1). Runs in the background; a newer
/// measurement or an encode stops it, and then this fails with "cancelled".
#[tauri::command]
pub async fn measure_packed(state: State<'_, Shared>, paths: Vec<String>) -> Result<u64, String> {
    let cancel = CancelToken::new();
    if let Some(previous) = lock(&state.measure).replace(cancel.clone()) {
        previous.cancel();
    }
    let inputs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    blocking(move || atv_core::measure_packed(&inputs, &cancel))
        .await?
        .map_err(|error| error.to_string())
}

/// The live summary strip under the options.
// Each argument is one value the page sends by name; a struct would only
// rename them.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn encode_summary(
    bytes: u64,
    resolution: String,
    mode: String,
    cell: u32,
    parity: u32,
    compress: bool,
    frame_rate: Option<u32>,
    av1: Option<bool>,
    packed: Option<u64>,
    smallest: Option<bool>,
) -> Summary {
    let mode = messages::mode_from(&mode, cell, parity);
    let resolution = messages::resolution_from(&resolution);
    // Once the compressed size is measured (spec 4.1), the estimate is of the
    // real data; until then it is an upper limit.
    let packed = packed.filter(|_| compress);
    let mut estimate = match packed {
        Some(packed) => atv_core::estimate_packed(packed, resolution, &mode),
        None => atv_core::estimate(bytes, resolution, &mode),
    };
    // The estimate is for 30 frames a second; Smart delivery may pick fewer.
    if let Some(fps) = frame_rate.filter(|fps| (1..30).contains(fps)) {
        estimate.seconds = estimate.frames as f64 / f64::from(fps);
    }
    // AV1 with screen tools measured at 42-56% of H.264 for ATV's blocks (spec 7.1).
    if av1.unwrap_or(false) && matches!(mode, Mode::Robust { .. }) {
        estimate.video_bytes /= 2;
    }
    Summary {
        length: if estimate.seconds < 1.0 {
            "< 1 s".to_string()
        } else {
            format!(
                "≈ {}",
                text::about(estimate.seconds).trim_start_matches("about ")
            )
        },
        // "Smallest video" compresses harder than the measurement did.
        size: if compress && (packed.is_none() || smallest.unwrap_or(false)) {
            format!("≤ {}", text::bytes(estimate.video_bytes))
        } else {
            format!("≈ {}", text::bytes(estimate.video_bytes))
        },
        frames: text::group_digits(estimate.frames),
    }
}

#[derive(Serialize)]
pub struct StrengthView {
    /// 0 to 4 (zxcvbn's score).
    score: u8,
    /// Strong enough to be the video's only secret (spec 6.4).
    enough: bool,
    /// Why it is weak, in plain words, when that is known.
    warning: Option<String>,
}

/// How hard the password being typed is to guess (spec 6.4). The password is
/// only judged here, then wiped; it is never kept or logged.
#[tauri::command]
pub fn password_strength(password: String) -> StrengthView {
    let password = zeroize::Zeroizing::new(password);
    let strength = atv_core::password_strength(&password);
    StrengthView {
        score: strength.score,
        enough: strength.enough,
        warning: strength.warning,
    }
}

#[derive(Serialize)]
pub struct OutputView {
    path: String,
    name: String,
    folder: String,
}

fn output_view(path: &Path) -> OutputView {
    OutputView {
        path: path_text(path),
        name: files::display_name(path),
        folder: path
            .parent()
            .map(files::friendly_folder)
            .unwrap_or_default(),
    }
}

/// Where a new video goes: a free name in the output folder from Settings,
/// taken from what is being encoded. A name picked with "Change…" keeps its
/// folder and name and only follows the mode's extension.
#[tauri::command]
pub fn suggest_output(
    state: State<'_, Shared>,
    inputs: Vec<String>,
    mode: String,
    chosen: Option<String>,
) -> OutputView {
    let extension = messages::mode_from(&mode, 8, 64).extension();
    if let Some(chosen) = chosen {
        return output_view(&PathBuf::from(chosen).with_extension(extension));
    }
    let inputs: Vec<PathBuf> = inputs.into_iter().map(PathBuf::from).collect();
    let folder = lock(&state.settings).output_folder();
    output_view(&files::free_path(
        &folder,
        &files::video_stem(&inputs),
        extension,
    ))
}

#[tauri::command]
pub async fn choose_output_file(
    app: AppHandle,
    current: String,
    mode: String,
) -> Result<Option<OutputView>, String> {
    blocking(move || {
        let current = PathBuf::from(current);
        let extension = messages::mode_from(&mode, 8, 64).extension();
        let mut dialog = app
            .dialog()
            .file()
            .set_title("Save the video as")
            .add_filter("Video", &[extension]);
        if let Some(folder) = current.parent() {
            dialog = dialog.set_directory(folder);
        }
        dialog = dialog.set_file_name(files::display_name(&current));
        dialog
            .blocking_save_file()
            .and_then(|path| path.into_path().ok())
            .map(|path| output_view(&path.with_extension(extension)))
    })
    .await
}

#[derive(Serialize)]
pub struct KeyView {
    path: String,
    name: String,
}

#[tauri::command]
pub async fn create_key_file(app: AppHandle) -> Result<Option<KeyView>, String> {
    let picked = blocking(move || {
        let start = directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| lock(&app.state::<Shared>().settings).output_folder());
        app.dialog()
            .file()
            .set_title("Save the new key file")
            .set_directory(start)
            .set_file_name("ATV key.atvkey")
            .add_filter("ATV key file", &["atvkey"])
            .blocking_save_file()
            .and_then(|path| path.into_path().ok())
    })
    .await?;
    let Some(path) = picked else { return Ok(None) };
    let path = if path.extension().is_none() {
        path.with_extension("atvkey")
    } else {
        path
    };
    atv_core::generate_key_file(&path).map_err(|error| messages::capitalise(&error.to_string()))?;
    Ok(Some(KeyView {
        name: files::display_name(&path),
        path: path_text(&path),
    }))
}

#[tauri::command]
pub async fn choose_key_file(app: AppHandle) -> Result<Option<KeyView>, String> {
    let picked = blocking(move || {
        app.dialog()
            .file()
            .set_title("Choose a key file")
            .add_filter("ATV key file", &["atvkey"])
            .add_filter("All files", &["*"])
            .blocking_pick_file()
            .and_then(|path| path.into_path().ok())
    })
    .await?;
    let Some(path) = picked else { return Ok(None) };
    if !files::looks_like_key_file(&path) {
        return Err(format!(
            "{} is not an ATV key file.",
            files::display_name(&path)
        ));
    }
    Ok(Some(KeyView {
        name: files::display_name(&path),
        path: path_text(&path),
    }))
}

#[derive(Deserialize)]
pub struct EncodeRequest {
    inputs: Vec<String>,
    output: String,
    resolution: String,
    mode: String,
    cell: u32,
    parity: u32,
    compress: bool,
    smallest: bool,
    /// Frames per second for Robust videos (Smart delivery); `None` is 30.
    frame_rate: Option<u32>,
    /// Robust videos as AV1 ("Smaller upload"), else H.264.
    #[serde(default)]
    av1: bool,
    verify: bool,
    encrypt: bool,
    password: Option<String>,
    key_file: Option<String>,
    /// Total size of the inputs as measured on the page, for History.
    input_bytes: u64,
}

#[tauri::command]
pub fn start_encode(
    app: AppHandle,
    state: State<'_, Shared>,
    request: EncodeRequest,
) -> Result<(), ErrorView> {
    // The encode reads the same files; it should not compete with a measurement.
    if let Some(measuring) = lock(&state.measure).take() {
        measuring.cancel();
    }
    let tools = lock(&state.tools).clone().ok_or_else(|| {
        messages::explain(&AtvError::FfmpegNotFound {
            detail: "ffmpeg was not found".to_string(),
        })
    })?;
    let mode = messages::mode_from(&request.mode, request.cell, request.parity);
    let resolution = messages::resolution_from(&request.resolution);
    let protection = request.encrypt.then(|| Protection {
        password: request
            .password
            .filter(|password| !password.is_empty())
            .map(SecretString::new),
        key_file: request.key_file.map(PathBuf::from),
    });
    let output = PathBuf::from(&request.output);
    let options = EncodeOptions {
        inputs: request.inputs.iter().map(PathBuf::from).collect(),
        output: output.clone(),
        resolution,
        mode,
        compress: request.compress,
        smallest: request.compress && request.smallest,
        protection,
        verify: request.verify,
        keep_unverified: false,
        frame_rate: matches!(mode, Mode::Robust { .. })
            .then_some(request.frame_rate)
            .flatten(),
        video_codec: if request.av1 {
            atv_core::VideoCodec::Av1
        } else {
            atv_core::VideoCodec::H264
        },
        tools,
    };
    let entry = Entry {
        direction: Direction::Encode,
        name: files::display_name(&output),
        date: chrono::Utc::now().timestamp(),
        resolution: resolution.label().to_string(),
        mode: messages::mode_short(&mode),
        encrypted: request.encrypt,
        size: request.input_bytes,
        outcome: Outcome::Failed,
        path: Some(output),
    };
    let kind = Kind::Encode {
        compress: request.compress,
        encrypt: request.encrypt,
        verify: request.verify,
    };
    let cancel = claim_job(&state)?;
    thread::spawn(move || {
        let mut tracker = Tracker::new(kind);
        let result = atv_core::encode(
            &options,
            &mut |event| send_update(&app, &mut tracker, &event),
            &cancel,
        );
        let finished = encode_finished(result, entry);
        job_ended(&app, finished);
    });
    Ok(())
}

/// Take the one job slot, or refuse if a job is already running.
fn claim_job(state: &Shared) -> Result<CancelToken, ErrorView> {
    let mut slot = lock(&state.job);
    if slot.is_some() {
        return Err(messages::explain(&AtvError::InvalidInput {
            message: "another job is already running".to_string(),
        }));
    }
    let cancel = CancelToken::new();
    *slot = Some(cancel.clone());
    Ok(cancel)
}

#[derive(Clone, Serialize)]
struct JobUpdate {
    #[serde(flatten)]
    view: View,
    preview: Option<PreviewData>,
}

fn send_update(app: &AppHandle, tracker: &mut Tracker, event: &atv_core::Event) {
    let view = tracker.view(&Update::from(event));
    let _ = app.emit(
        "job-update",
        JobUpdate {
            view,
            preview: event.preview.as_ref().map(PreviewData::from),
        },
    );
}

#[derive(Clone, Serialize)]
pub struct RestoredItem {
    name: String,
    icon: &'static str,
    kind: String,
    size: String,
}

/// How a job ended, as the page needs it.
#[derive(Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Finished {
    EncodeDone {
        name: String,
        path: String,
        folder: String,
        length: String,
        size: String,
        details: String,
        verified: bool,
    },
    DecodeDone {
        items: Vec<RestoredItem>,
        more: String,
        summary: String,
        repaired: String,
        reveal: String,
    },
    Cancelled {
        encoding: bool,
    },
    Failed {
        encoding: bool,
        error: ErrorView,
    },
}

fn encode_finished(
    result: Result<EncodeReport, AtvError>,
    mut entry: Entry,
) -> (Finished, Option<Entry>) {
    match result {
        Ok(report) => {
            entry.outcome = Outcome::Done;
            entry.size = report.input_bytes;
            let finished = Finished::EncodeDone {
                name: files::display_name(&report.output),
                path: path_text(&report.output),
                folder: report
                    .output
                    .parent()
                    .map(files::friendly_folder)
                    .unwrap_or_default(),
                length: text::clock(report.seconds),
                size: text::bytes(report.video_bytes),
                details: format!(
                    "{} · {} · {}",
                    report.resolution.label(),
                    messages::mode_short(&report.mode),
                    if report.encrypted {
                        "Encrypted"
                    } else {
                        "Not encrypted"
                    }
                ),
                verified: report.verified,
            };
            (finished, Some(entry))
        }
        Err(AtvError::Cancelled) => {
            entry.outcome = Outcome::Cancelled;
            entry.path = None;
            (Finished::Cancelled { encoding: true }, Some(entry))
        }
        Err(error) => {
            entry.outcome = Outcome::Failed;
            entry.path = None;
            (
                Finished::Failed {
                    encoding: true,
                    error: messages::explain(&error),
                },
                Some(entry),
            )
        }
    }
}

/// Record the job in History, free the job slot, and tell the page.
fn job_ended(app: &AppHandle, (finished, entry): (Finished, Option<Entry>)) {
    let state = app.state::<Shared>();
    if let Some(entry) = entry {
        store::push_entry(&mut lock(&state.history), entry);
        state.save_history();
    }
    *lock(&state.job) = None;
    let _ = app.emit("job-finished", finished);
    let _ = app.emit("history-changed", history_rows(&lock(&state.history)));
    if state.quit_after_job.load(Ordering::SeqCst) {
        app.exit(0);
    }
}

#[tauri::command]
pub fn cancel_job(state: State<'_, Shared>) {
    state.cancel_job();
}

// ------------------------------------------------------------------- decode

#[tauri::command]
pub async fn pick_video(app: AppHandle) -> Result<Option<String>, String> {
    blocking(move || {
        app.dialog()
            .file()
            .set_title("Choose an ATV video")
            .add_filter("ATV video", &["mkv", "mp4"])
            .add_filter("All files", &["*"])
            .blocking_pick_file()
            .and_then(|path| path.into_path().ok())
            .map(|path| path_text(&path))
    })
    .await
}

#[derive(Serialize)]
pub struct VideoView {
    path: String,
    name: String,
    resolution: String,
    mode: String,
    tag: String,
    robust: bool,
    length: String,
    frames: String,
    /// Data frames, for working out how much of a damaged video could be read.
    data_frames: Option<u64>,
    size: String,
    encrypted: bool,
    compressed: bool,
    needs_password: bool,
    needs_key_file: bool,
    preview: Option<PreviewData>,
    /// Set when a site resized the video: `1280 × 720`.
    resized: Option<String>,
    /// Robust repair bytes per codeword (0 for Lossless).
    parity: u8,
    /// Height of the file itself, which differs from the ATV size when a
    /// site resized it.
    copy_height: u32,
    /// ATV format version: 3 for Robust videos made since 14 Nov 2026.
    version: u8,
}

/// Read a video's header (and a thumbnail) to show what it is.
#[tauri::command]
pub async fn probe_video(app: AppHandle, path: String) -> Result<VideoView, ErrorView> {
    let tools = lock(&app.state::<Shared>().tools).clone().ok_or_else(|| {
        messages::explain(&AtvError::FfmpegNotFound {
            detail: "ffmpeg was not found".to_string(),
        })
    })?;
    let video = PathBuf::from(&path);
    let result = blocking(move || atv_core::probe(&video, &tools))
        .await
        .map_err(|message| messages::explain(&AtvError::InvalidInput { message }))?;
    let info = result.map_err(|error| messages::explain(&error))?;
    let data_frames = info.frames.map(|frames| frames.saturating_sub(2));
    Ok(VideoView {
        name: files::display_name(Path::new(&path)),
        path,
        resolution: info.resolution.label().to_string(),
        mode: messages::mode_label(&info.mode),
        tag: format!(
            "{} · {}",
            info.resolution.label(),
            messages::mode_short(&info.mode)
        ),
        robust: matches!(info.mode, Mode::Robust { .. }),
        length: info
            .seconds
            .map(text::clock)
            .unwrap_or_else(|| "—".to_string()),
        frames: info
            .frames
            .map(text::group_digits)
            .unwrap_or_else(|| "—".to_string()),
        data_frames,
        size: text::bytes(info.file_bytes),
        encrypted: info.encrypted,
        compressed: info.compressed,
        needs_password: info.needs_password,
        needs_key_file: info.needs_key_file,
        preview: info.preview.as_ref().map(PreviewData::from),
        resized: info
            .resized_from
            .map(|(width, height)| format!("{width} × {height}")),
        parity: match info.mode {
            Mode::Robust { parity, .. } => parity,
            Mode::Lossless => 0,
        },
        copy_height: info
            .resized_from
            .map(|(_, height)| height)
            .unwrap_or_else(|| info.resolution.height()),
        version: info.format_version,
    })
}

#[derive(Serialize)]
pub struct FolderView {
    path: String,
    friendly: String,
}

#[tauri::command]
pub fn default_restore_folder(state: State<'_, Shared>) -> FolderView {
    let folder = lock(&state.settings).output_folder();
    FolderView {
        friendly: files::friendly_folder(&folder),
        path: path_text(&folder),
    }
}

#[tauri::command]
pub async fn choose_restore_folder(
    app: AppHandle,
    current: String,
) -> Result<Option<FolderView>, String> {
    blocking(move || {
        app.dialog()
            .file()
            .set_title("Choose where to restore the files")
            .set_directory(current)
            .blocking_pick_folder()
            .and_then(|path| path.into_path().ok())
            .map(|path| FolderView {
                friendly: files::friendly_folder(&path),
                path: path_text(&path),
            })
    })
    .await
}

#[derive(Deserialize)]
pub struct DecodeRequest {
    video: String,
    output_dir: String,
    password: Option<String>,
    key_file: Option<String>,
    /// From the probe, for History and the stage list.
    resolution: String,
    mode: String,
    robust: bool,
    encrypted: bool,
    compressed: bool,
    data_frames: Option<u64>,
}

#[tauri::command]
pub fn start_decode(
    app: AppHandle,
    state: State<'_, Shared>,
    request: DecodeRequest,
) -> Result<(), ErrorView> {
    let tools = lock(&state.tools).clone().ok_or_else(|| {
        messages::explain(&AtvError::FfmpegNotFound {
            detail: "ffmpeg was not found".to_string(),
        })
    })?;
    let protection = request.encrypted.then(|| Protection {
        password: request
            .password
            .filter(|password| !password.is_empty())
            .map(SecretString::new),
        key_file: request.key_file.map(PathBuf::from),
    });
    let video = PathBuf::from(&request.video);
    let options = DecodeOptions {
        video: video.clone(),
        output_dir: PathBuf::from(&request.output_dir),
        protection,
        tools,
    };
    let entry = Entry {
        direction: Direction::Decode,
        name: files::display_name(&video),
        date: chrono::Utc::now().timestamp(),
        resolution: request.resolution,
        mode: request.mode,
        encrypted: request.encrypted,
        size: 0,
        outcome: Outcome::Failed,
        path: None,
    };
    let kind = Kind::Decode {
        robust: request.robust,
        encrypted: request.encrypted,
        compressed: request.compressed,
    };
    let data_frames = request.data_frames;
    let robust = request.robust;
    let cancel = claim_job(&state)?;
    thread::spawn(move || {
        let mut tracker = Tracker::new(kind);
        let result = atv_core::decode(
            &options,
            &mut |event| send_update(&app, &mut tracker, &event),
            &cancel,
        );
        let finished = decode_finished(result, entry, data_frames, robust);
        job_ended(&app, finished);
    });
    Ok(())
}

/// At most this many restored items are listed; the rest are counted.
const LIST_LIMIT: usize = 200;

fn decode_finished(
    result: Result<DecodeReport, AtvError>,
    mut entry: Entry,
    data_frames: Option<u64>,
    robust: bool,
) -> (Finished, Option<Entry>) {
    match result {
        Ok(report) => {
            entry.outcome = Outcome::Done;
            entry.size = report.total_bytes;
            entry.path = report
                .items
                .first()
                .cloned()
                .or_else(|| Some(report.output_dir.clone()));
            let items: Vec<RestoredItem> = report
                .items
                .iter()
                .take(LIST_LIMIT)
                .map(|item| restored_item(item, &report))
                .collect();
            let more = report.items.len().saturating_sub(LIST_LIMIT);
            let finished = Finished::DecodeDone {
                items,
                more: if more > 0 {
                    format!("and {} more", text::plural(more as u64, "item"))
                } else {
                    String::new()
                },
                summary: format!(
                    "{} · {} · {}",
                    text::plural(report.items.len() as u64, "item"),
                    text::bytes(report.total_bytes),
                    files::friendly_folder(&report.output_dir)
                ),
                repaired: match (robust, report.repaired_bytes) {
                    (false, _) => String::new(),
                    (true, 0) => "No damage was found".to_string(),
                    (true, count) => format!("{} repaired", text::plural(count, "damaged byte")),
                },
                reveal: entry.path.as_deref().map(path_text).unwrap_or_default(),
            };
            (finished, Some(entry))
        }
        Err(AtvError::Cancelled) => {
            entry.outcome = Outcome::Cancelled;
            (Finished::Cancelled { encoding: false }, Some(entry))
        }
        Err(error) => {
            let mut view = messages::explain(&error);
            if let (AtvError::Damaged { frame }, Some(total)) = (&error, data_frames)
                && total > 0
            {
                view.recoverable = Some((*frame as f32 / total as f32).clamp(0.0, 1.0));
            }
            // A wrong or missing password is a retry, not a job for History.
            let record = !matches!(error, AtvError::WrongKey | AtvError::KeyRequired { .. });
            entry.outcome = Outcome::Failed;
            (
                Finished::Failed {
                    encoding: false,
                    error: view,
                },
                record.then_some(entry),
            )
        }
    }
}

/// A top-level restored item with its size, adding up the files inside a folder.
fn restored_item(item: &Path, report: &DecodeReport) -> RestoredItem {
    let name = files::display_name(item);
    let inside: Vec<&atv_core::RestoredFile> = report
        .files
        .iter()
        .filter(|file| {
            file.path.components().next().map(|part| part.as_os_str())
                == Some(std::ffi::OsStr::new(&name))
        })
        .collect();
    let bytes: u64 = inside.iter().map(|file| file.size).sum();
    if item.is_dir() {
        RestoredItem {
            name,
            icon: "folder",
            kind: format!("Folder · {}", text::plural(inside.len() as u64, "file")),
            size: text::bytes(bytes),
        }
    } else {
        let described = files::describe(item);
        RestoredItem {
            name,
            icon: described.icon,
            kind: described.kind,
            size: text::bytes(bytes),
        }
    }
}

// ------------------------------------------------------------------ history

#[derive(Clone, Serialize)]
pub struct HistoryRow {
    encode: bool,
    name: String,
    direction: &'static str,
    date: String,
    format: String,
    encrypted: bool,
    size: String,
    result: &'static str,
    can_show: bool,
}

pub fn history_rows(entries: &[Entry]) -> Vec<HistoryRow> {
    entries
        .iter()
        .map(|entry| HistoryRow {
            encode: entry.direction == Direction::Encode,
            name: entry.name.clone(),
            direction: match entry.direction {
                Direction::Encode => "Encoded",
                Direction::Decode => "Decoded",
            },
            date: date(entry.date),
            format: format!("{} · {}", entry.resolution, entry.mode),
            encrypted: entry.encrypted,
            size: if entry.size > 0 {
                text::bytes(entry.size)
            } else {
                "—".to_string()
            },
            result: match entry.outcome {
                Outcome::Done => "done",
                Outcome::Failed => "failed",
                Outcome::Cancelled => "cancelled",
            },
            can_show: entry.path.as_ref().is_some_and(|path| path.exists()),
        })
        .collect()
}

/// `Today, 14:32`, `Yesterday, 21:08`, or `9 Nov 2026`, in local time.
fn date(seconds: i64) -> String {
    use chrono::{DateTime, Local};
    let Some(when) = DateTime::from_timestamp(seconds, 0).map(|utc| utc.with_timezone(&Local))
    else {
        return String::new();
    };
    let today = Local::now().date_naive();
    let day = when.date_naive();
    if day == today {
        format!("Today, {}", when.format("%H:%M"))
    } else if today.pred_opt() == Some(day) {
        format!("Yesterday, {}", when.format("%H:%M"))
    } else {
        when.format("%-d %b %Y").to_string()
    }
}

#[tauri::command]
pub fn history_remove(state: State<'_, Shared>, index: usize) -> Vec<HistoryRow> {
    {
        let mut history = lock(&state.history);
        if index < history.len() {
            history.remove(index);
        }
    }
    state.save_history();
    history_rows(&lock(&state.history))
}

#[tauri::command]
pub fn history_clear(state: State<'_, Shared>) -> Vec<HistoryRow> {
    lock(&state.history).clear();
    state.save_history();
    Vec::new()
}

#[tauri::command]
pub fn reveal_history(state: State<'_, Shared>, index: usize) {
    let path = lock(&state.history)
        .get(index)
        .and_then(|entry| entry.path.clone());
    if let Some(path) = path {
        platform::reveal(&path);
    }
}

// ------------------------------------------------------------------ YouTube
//
// Owner's request after the YouTube test: paste a link on the Decode tab,
// see the video as it is on YouTube, and let ATV download the best copy and
// open it (spec 9.4 and 13).

/// The checked link, or the error the page shows under the box.
fn youtube_link(text: &str) -> Result<YouTubeLink, ErrorView> {
    YouTubeLink::parse(text).ok_or_else(|| ErrorView {
        code: "link",
        title: "This isn’t a YouTube video link".to_string(),
        text: "Paste the link from the video’s Share button, such as https://youtu.be/… or https://www.youtube.com/watch?v=…".to_string(),
        detail: String::new(),
        recoverable: None,
    })
}

/// yt-dlp, found once and then remembered.
fn ytdlp(app: &AppHandle) -> Result<YtDlp, AtvError> {
    let state = app.state::<Shared>();
    if let Some(found) = lock(&state.ytdlp).clone() {
        return Ok(found);
    }
    let folder = tools_folder(&state);
    let found = atv_core::find_ytdlp(None, Some(&folder))?;
    *lock(&state.ytdlp) = Some(found.clone());
    Ok(found)
}

fn ytdlp_error(error: &AtvError) -> ErrorView {
    let mut view = messages::explain(error);
    if matches!(error, AtvError::DownloaderNotFound { .. }) {
        let (where_, command) = platform::ytdlp_install_hint();
        view.text = format!("{} {where_}: {command}", view.text);
    }
    view
}

#[derive(Serialize)]
pub struct OnlineView {
    id: String,
    url: String,
    /// The player page for the preview, from YouTube's privacy-enhanced site.
    embed: String,
    /// A 16:9-friendly thumbnail that every video has.
    thumbnail: String,
    title: String,
    channel: String,
    length: String,
    /// "4K", "1080p", "720p"… or "Unknown".
    quality: String,
    /// The best copy is smaller than 1080p: YouTube may still be making HD.
    small: bool,
}

/// Ask YouTube about a pasted link, for the preview.
#[tauri::command]
pub async fn youtube_details(app: AppHandle, link: String) -> Result<OnlineView, ErrorView> {
    let link = youtube_link(&link)?;
    let result = blocking(move || {
        let ytdlp = ytdlp(&app)?;
        atv_core::youtube_details(&ytdlp, &link)
    })
    .await
    .map_err(|message| messages::explain(&AtvError::InvalidInput { message }))?;
    let video = result.map_err(|error| ytdlp_error(&error))?;
    let id = video.link.id.clone();
    Ok(OnlineView {
        url: video.link.url(),
        embed: format!(
            "https://www.youtube-nocookie.com/embed/{id}?autoplay=1&rel=0&modestbranding=1"
        ),
        thumbnail: format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"),
        id,
        title: video.title,
        channel: video.channel,
        length: video
            .seconds
            .map(text::clock)
            .unwrap_or_else(|| "—".to_string()),
        quality: video
            .best_height
            .map(quality_label)
            .unwrap_or_else(|| "Unknown".to_string()),
        small: video.best_height.is_some_and(|height| height < 1080),
    })
}

fn quality_label(height: u32) -> String {
    match height {
        2160.. => "4K".to_string(),
        1440..=2159 => "1440p".to_string(),
        _ => format!("{height}p"),
    }
}

/// Open the video's YouTube page in the browser.
#[tauri::command]
pub fn open_youtube(link: String) -> Result<(), ErrorView> {
    platform::open_url(&youtube_link(&link)?.url());
    Ok(())
}

#[derive(Clone, Serialize)]
struct DownloadUpdate {
    /// 0 to 1, when the size is known.
    fraction: Option<f64>,
    text: String,
}

#[derive(Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Downloaded {
    Done { path: String },
    Cancelled,
    Failed { error: ErrorView },
}

/// Download the best copy of a YouTube video, then tell the page where it is.
/// It uses the one job slot, so the Cancel button and closing the window
/// work as they do for encoding and decoding.
#[tauri::command]
pub fn start_youtube_download(
    app: AppHandle,
    state: State<'_, Shared>,
    link: String,
) -> Result<(), ErrorView> {
    let link = youtube_link(&link)?;
    let folder = state
        .locations
        .as_ref()
        .map(|places| places.downloads.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("ATV downloads"));
    let cancel = claim_job(&state)?;
    thread::spawn(move || {
        let result = ytdlp(&app).and_then(|ytdlp| {
            store::clear_downloads(&folder);
            let mut last_sent = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut on_progress = |progress: DownloadProgress| {
                // A few updates a second are plenty for a progress bar.
                if last_sent.elapsed().as_millis() < 150 {
                    return;
                }
                last_sent = std::time::Instant::now();
                let _ = app.emit("download-update", download_update(&progress));
            };
            atv_core::download_youtube(&ytdlp, &link, &folder, &mut on_progress, &cancel)
        });
        let downloaded = match result {
            Ok(path) => Downloaded::Done {
                path: path_text(&path),
            },
            Err(AtvError::Cancelled) => Downloaded::Cancelled,
            Err(error) => Downloaded::Failed {
                error: ytdlp_error(&error),
            },
        };
        let state = app.state::<Shared>();
        *lock(&state.job) = None;
        let _ = app.emit("download-finished", downloaded);
        if state.quit_after_job.load(Ordering::SeqCst) {
            app.exit(0);
        }
    });
    Ok(())
}

fn download_update(progress: &DownloadProgress) -> DownloadUpdate {
    let done = text::bytes(progress.downloaded);
    let mut line = match progress.total {
        Some(total) if total > 0 => format!("{done} of {}", text::bytes(total)),
        _ => format!("{done} downloaded"),
    };
    if let Some(speed) = progress.speed.filter(|speed| *speed > 0.0) {
        line.push_str(&format!(" · {}", text::speed(speed)));
    }
    DownloadUpdate {
        fraction: progress
            .total
            .filter(|total| *total > 0)
            .map(|total| (progress.downloaded as f64 / total as f64).clamp(0.0, 1.0)),
        text: line,
    }
}

// -------------------------------------------------------------------- tools
//
// Owner's request: ATV checks that ffmpeg, yt-dlp and a JavaScript runtime
// are present and current, and installs them from their official releases
// (spec 9.5b). The real work is in atv-core's `tools` module.

/// Where ATV keeps its own copies of the tools.
fn tools_folder(state: &Shared) -> PathBuf {
    state
        .locations
        .as_ref()
        .map(|places| places.tools.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("ATV tools"))
}

/// Where to start looking for ffmpeg: the one chosen in Settings, else ATV's
/// own copy, else `None` (search the usual places).
fn ffmpeg_start(state: &Shared) -> Option<PathBuf> {
    lock(&state.settings)
        .ffmpeg_path
        .clone()
        .or_else(|| atv_core::managed_program(&tools_folder(state), Tool::Ffmpeg))
}

fn tool_key(tool: Tool) -> &'static str {
    match tool {
        Tool::Ffmpeg => "ffmpeg",
        Tool::YtDlp => "ytdlp",
        Tool::Deno => "deno",
    }
}

fn tool_from_key(key: &str) -> Option<Tool> {
    Tool::ALL.into_iter().find(|tool| tool_key(*tool) == key)
}

/// The first line a program prints for `--version`, for Node.js and Bun.
fn other_runtime_version(program: &Path) -> Option<String> {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    (output.status.success() && !line.is_empty()).then(|| line.trim_start_matches('v').to_string())
}

#[derive(Clone, Serialize)]
pub struct ToolRow {
    key: &'static str,
    name: String,
    /// What ATV uses it for.
    purpose: &'static str,
    /// The installed version, short: `8.1`, `2026.10.26`.
    version: String,
    /// "ATV's copy", "On this PC" or "".
    source: String,
    path: String,
    /// The newest official version, once checked.
    latest: Option<String>,
    /// `ok`, `update`, `missing`, `installed` (not compared yet) or `unknown`.
    state: &'static str,
    /// A short line under the row: what to do, or why not.
    note: String,
    /// `install`, `update` or `None`.
    action: Option<&'static str>,
}

#[derive(Clone, Serialize)]
pub struct ToolsView {
    rows: Vec<ToolRow>,
    /// "Checked for updates today at 10:42", or "Not checked yet".
    checked: String,
    /// How many installed tools have a newer official version.
    updates: usize,
}

/// The short form of a version text: `8.1-full_build-www.gyan.dev` gives `8.1`.
fn short_version(text: &str) -> String {
    text.split(['-', ' ']).next().unwrap_or(text).to_string()
}

fn tool_row(
    tool: Tool,
    installed: Option<(String, PathBuf)>,
    other_runtime: Option<String>,
    latest: Option<&Result<Option<Release>, String>>,
    remembered: Option<String>,
    folder: &Path,
) -> ToolRow {
    let purpose = match tool {
        Tool::Ffmpeg => "Writes and reads the videos. Required.",
        Tool::YtDlp => "Downloads videos from YouTube links on the Decode screen.",
        Tool::Deno => "Lets yt-dlp unlock YouTube downloads. Deno, Node.js or Bun all work.",
    };
    let mut row = ToolRow {
        key: tool_key(tool),
        name: tool.name().to_string(),
        purpose,
        version: String::new(),
        source: String::new(),
        path: String::new(),
        latest: None,
        state: "missing",
        note: String::new(),
        action: None,
    };
    // The newest version: from this session's check, or remembered from the
    // last one (checks run at most once a day).
    let newest = match latest {
        Some(Ok(Some(release))) => Some(release.version.clone()),
        _ => remembered,
    };
    row.latest = newest.clone();
    // yt-dlp is happy with Node.js or Bun: then there is nothing to manage.
    if let Some(runtime) = other_runtime {
        row.name = "JavaScript runtime".to_string();
        row.version = runtime;
        row.source = "On this PC".to_string();
        row.state = "ok";
        row.note = "yt-dlp uses this. ATV only installs Deno when none is found.".to_string();
        row.latest = None;
        return row;
    }
    match installed {
        Some((version, path)) => {
            row.version = short_version(&version);
            row.source = if path.starts_with(folder) {
                "ATV's copy".to_string()
            } else {
                "On this PC".to_string()
            };
            row.path = path_text(&path);
            row.state = "installed";
            match latest {
                Some(Ok(None)) => row.note = install_note(tool),
                Some(Err(problem)) => {
                    row.note = format!("Could not check for updates: {problem}");
                }
                _ => {}
            }
            if let Some(newest) = &newest {
                match atv_core::is_newer(newest, &version) {
                    Some(true) => {
                        row.state = "update";
                        row.action = Some("update");
                        row.note = format!(
                            "Version {newest} is out. ATV installs it as its own copy; {} is not changed.",
                            if row.source == "ATV's copy" {
                                "nothing else"
                            } else {
                                "your copy"
                            }
                        );
                    }
                    Some(false) => row.state = "ok",
                    None => {
                        row.state = "unknown";
                        row.note = format!(
                            "This build has no release number to compare. The newest release is {newest}."
                        );
                        row.action = Some("update");
                    }
                }
            }
        }
        None => {
            row.state = "missing";
            if tool.installable_here() {
                row.action = Some("install");
                row.note = format!(
                    "Not found. Install downloads it from {}.",
                    source_name(tool)
                );
            } else {
                row.note = install_note(tool);
            }
        }
    }
    row
}

fn source_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Ffmpeg => "gyan.dev (the Windows build ffmpeg.org links to)",
        Tool::YtDlp => "yt-dlp’s GitHub releases",
        Tool::Deno => "Deno’s GitHub releases",
    }
}

/// For a tool ATV does not install on this system: how to install it.
fn install_note(tool: Tool) -> String {
    match tool {
        Tool::Ffmpeg => {
            let (place, command) = platform::install_hint();
            format!("ATV does not install ffmpeg here. {place}: {command}")
        }
        Tool::YtDlp => {
            let (place, command) = platform::ytdlp_install_hint();
            format!("ATV does not install yt-dlp here. {place}: {command}")
        }
        Tool::Deno => "ATV does not install Deno on this system. See deno.com.".to_string(),
    }
}

/// Look at what is installed, and with `online`, ask each official release
/// channel for its newest version (all three at once).
fn tools_view(app: &AppHandle, online: bool) -> ToolsView {
    let state = app.state::<Shared>();
    let folder = tools_folder(&state);
    if online {
        let checks: Vec<_> = Tool::ALL
            .into_iter()
            .map(|tool| {
                (
                    tool,
                    thread::spawn(move || {
                        atv_core::latest_release(tool).map_err(|error| error.to_string())
                    }),
                )
            })
            .collect();
        let results: Vec<_> = checks
            .into_iter()
            .map(|(tool, handle)| {
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err("the check stopped unexpectedly".to_string()));
                (tool, result)
            })
            .collect();
        {
            let mut settings = lock(&state.settings);
            for (tool, result) in &results {
                if let Ok(Some(release)) = result {
                    settings
                        .latest_tools
                        .insert(tool_key(*tool).to_string(), release.version.clone());
                }
            }
            settings.last_tool_check = Some(chrono::Local::now().timestamp());
        }
        *lock(&state.latest) = results;
        state.save_settings();
    }

    // What is installed now.
    let ffmpeg = lock(&state.tools)
        .clone()
        .map(|tools| (tools.version.clone(), tools.ffmpeg.clone()));
    let ytdlp = ytdlp(app).ok();
    let runtime = ytdlp
        .as_ref()
        .and_then(|found| found.js_runtime.clone())
        .or_else(|| atv_core::find_js_runtime(Some(&folder)));
    let (deno, other_runtime) = match runtime.as_deref().and_then(|text| text.split_once(':')) {
        Some(("deno", path)) => {
            let path = PathBuf::from(path);
            (
                atv_core::program_version(Tool::Deno, &path).map(|version| (version, path)),
                None,
            )
        }
        Some((name, path)) => {
            let label = if name == "node" { "Node.js" } else { "Bun" };
            let version = other_runtime_version(Path::new(path)).unwrap_or_default();
            (None, Some(format!("{label} {version}").trim().to_string()))
        }
        None => (None, None),
    };
    let ytdlp = ytdlp.map(|found| (found.version.clone(), found.path.clone()));

    let latest = lock(&state.latest).clone();
    let latest_of = |tool: Tool| {
        latest
            .iter()
            .find(|(t, _)| *t == tool)
            .map(|(_, result)| result)
    };
    let remembered = lock(&state.settings).latest_tools.clone();
    let remembered_of = |tool: Tool| remembered.get(tool_key(tool)).cloned();
    let rows = vec![
        tool_row(
            Tool::Ffmpeg,
            ffmpeg,
            None,
            latest_of(Tool::Ffmpeg),
            remembered_of(Tool::Ffmpeg),
            &folder,
        ),
        tool_row(
            Tool::YtDlp,
            ytdlp,
            None,
            latest_of(Tool::YtDlp),
            remembered_of(Tool::YtDlp),
            &folder,
        ),
        tool_row(
            Tool::Deno,
            deno,
            other_runtime,
            latest_of(Tool::Deno),
            remembered_of(Tool::Deno),
            &folder,
        ),
    ];
    let updates = rows.iter().filter(|row| row.state == "update").count();
    let checked = match lock(&state.settings).last_tool_check {
        Some(seconds) => chrono::DateTime::from_timestamp(seconds, 0)
            .map(|time| time.with_timezone(&chrono::Local))
            .map(|time| {
                let today = chrono::Local::now().date_naive();
                if time.date_naive() == today {
                    format!("Checked today at {}", time.format("%H:%M"))
                } else {
                    format!("Checked {}", time.format("%-d %b at %H:%M"))
                }
            })
            .unwrap_or_default(),
        None => "Not checked yet".to_string(),
    };
    ToolsView {
        rows,
        checked,
        updates,
    }
}

/// The tools' state. `check` is `"no"` (what is installed only), `"auto"`
/// (also ask online if the last check was more than a day ago) or `"now"`.
#[tauri::command]
pub async fn tools_status(app: AppHandle, check: String) -> Result<ToolsView, String> {
    blocking(move || {
        let online = match check.as_str() {
            "now" => true,
            "auto" => {
                let last = lock(&app.state::<Shared>().settings).last_tool_check;
                let now = chrono::Local::now().timestamp();
                last.is_none_or(|last| now - last >= 24 * 60 * 60 || last > now)
            }
            _ => false,
        };
        tools_view(&app, online)
    })
    .await
}

#[derive(Clone, Serialize)]
struct ToolProgress {
    key: &'static str,
    fraction: Option<f64>,
    text: String,
}

#[derive(Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum ToolFinished {
    Done {
        key: &'static str,
        name: String,
        version: String,
    },
    Cancelled {
        key: &'static str,
    },
    Failed {
        key: &'static str,
        error: ErrorView,
    },
}

/// Install or update one tool from its official release. Uses the job slot,
/// so nothing that runs the tool can start meanwhile.
#[tauri::command]
pub fn install_tool(
    app: AppHandle,
    state: State<'_, Shared>,
    key: String,
) -> Result<(), ErrorView> {
    let tool = tool_from_key(&key).ok_or_else(|| {
        messages::explain(&AtvError::InvalidInput {
            message: format!("there is no tool called {key}"),
        })
    })?;
    let cancel = claim_job(&state)?;
    let folder = tools_folder(&state);
    let known = lock(&state.latest)
        .iter()
        .find(|(t, _)| *t == tool)
        .and_then(|(_, result)| result.clone().ok().flatten());
    thread::spawn(move || {
        let key = tool_key(tool);
        let result = known
            .map(Ok)
            .unwrap_or_else(|| {
                atv_core::latest_release(tool).and_then(|release| {
                    release.ok_or_else(|| AtvError::ToolInstallFailed {
                        detail: install_note(tool),
                    })
                })
            })
            .and_then(|release| {
                let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
                let mut on_progress = |done: u64, total: Option<u64>| {
                    if last.elapsed().as_millis() < 150 {
                        return;
                    }
                    last = std::time::Instant::now();
                    let text = match total {
                        Some(total) => format!("{} of {}", text::bytes(done), text::bytes(total)),
                        None => format!("{} downloaded", text::bytes(done)),
                    };
                    let _ = app.emit(
                        "tool-progress",
                        ToolProgress {
                            key,
                            fraction: total
                                .filter(|total| *total > 0)
                                .map(|total| (done as f64 / total as f64).clamp(0.0, 1.0)),
                            text,
                        },
                    );
                };
                atv_core::install_tool(&release, &folder, &mut on_progress, &cancel)
                    .map(|path| (release, path))
            });
        let state = app.state::<Shared>();
        let finished = match result {
            Ok((release, _path)) => {
                match tool {
                    Tool::Ffmpeg => {
                        // A fresh ATV copy replaces any ffmpeg chosen by hand.
                        lock(&state.settings).ffmpeg_path = None;
                        state.save_settings();
                        let start = ffmpeg_start(&state);
                        *lock(&state.tools) = atv_core::find_ffmpeg(start.as_deref()).ok();
                    }
                    Tool::YtDlp | Tool::Deno => *lock(&state.ytdlp) = None,
                }
                ToolFinished::Done {
                    key,
                    name: tool.name().to_string(),
                    version: release.version,
                }
            }
            Err(AtvError::Cancelled) => ToolFinished::Cancelled { key },
            Err(error) => ToolFinished::Failed {
                key,
                error: messages::explain(&error),
            },
        };
        *lock(&state.job) = None;
        let _ = app.emit("tool-finished", finished);
        if state.quit_after_job.load(Ordering::SeqCst) {
            app.exit(0);
        }
    });
    Ok(())
}

/// Open a tool's official download page.
#[tauri::command]
pub fn open_tool_page(key: String) {
    if let Some(tool) = tool_from_key(&key) {
        platform::open_url(tool.official_page());
    }
}

#[tauri::command]
pub fn open_ffmpeg_site() {
    platform::open_ffmpeg_site();
}

/// Open the author's GitHub profile (the credit in About and the wizard).
#[tauri::command]
pub fn open_author_page() {
    platform::open_author_page();
}

#[tauri::command]
pub fn reveal(path: String) {
    platform::reveal(Path::new(&path));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_use_the_designs_signs() {
        let summary = encode_summary(
            1 << 30,
            "1080p".into(),
            "lossless".into(),
            8,
            64,
            false,
            None,
            None,
            None,
            None,
        );
        assert_eq!(summary.length, "≈ 2 min");
        assert!(summary.size.starts_with("≈ 1.1"), "{}", summary.size);
        assert_eq!(summary.frames, "4,145");
        let compressed = encode_summary(
            1 << 30,
            "1080p".into(),
            "lossless".into(),
            8,
            64,
            true,
            None,
            None,
            None,
            None,
        );
        assert!(compressed.size.starts_with("≤ "));
        // Smart delivery's 10 frames a second: same frames, three times as long.
        let at_30 = encode_summary(
            50_000_000,
            "1080p".into(),
            "robust".into(),
            8,
            64,
            false,
            None,
            None,
            None,
            None,
        );
        let at_10 = encode_summary(
            50_000_000,
            "1080p".into(),
            "robust".into(),
            8,
            64,
            false,
            Some(10),
            None,
            None,
            None,
        );
        assert_eq!(at_30.frames, at_10.frames);
        assert_ne!(at_30.length, at_10.length);

        // Once the compressed size is measured: "≈" and fewer frames, unless
        // "Smallest video" may still shrink it further.
        let measured = |smallest| {
            encode_summary(
                1 << 30,
                "1080p".into(),
                "lossless".into(),
                8,
                64,
                true,
                None,
                None,
                Some(100_000_000),
                Some(smallest),
            )
        };
        assert!(
            measured(false).size.starts_with("≈ 104"),
            "{}",
            measured(false).size
        );
        assert_eq!(measured(false).frames, "388");
        assert!(measured(true).size.starts_with("≤ "));
        // Compression off: a measurement is ignored.
        let off = encode_summary(
            1 << 30,
            "1080p".into(),
            "lossless".into(),
            8,
            64,
            false,
            None,
            None,
            Some(100_000_000),
            None,
        );
        assert_eq!(off.frames, "4,145");
    }

    #[test]
    fn dates_read_like_the_design() {
        let now = chrono::Utc::now().timestamp();
        assert!(date(now).starts_with("Today, "));
        assert!(date(now - 86_400).starts_with("Yesterday, "));
        assert!(date(1_700_000_000).contains("2023"));
        assert_eq!(date(i64::MAX), "");
    }

    #[test]
    fn history_rows_say_what_happened() {
        let entry = Entry {
            direction: Direction::Decode,
            name: "Family Backup.mp4".into(),
            date: 1_700_000_000,
            resolution: "4K".into(),
            mode: "Robust 4 × 4".into(),
            encrypted: true,
            size: 0,
            outcome: Outcome::Failed,
            path: None,
        };
        let row = &history_rows(&[entry])[0];
        assert_eq!(row.direction, "Decoded");
        assert_eq!(row.format, "4K · Robust 4 × 4");
        assert_eq!(row.size, "—");
        assert_eq!(row.result, "failed");
        assert!(!row.can_show);
    }
}
