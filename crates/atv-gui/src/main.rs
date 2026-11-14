//! ATV desktop app: a thin Tauri 2 front end over `atv-core` (spec section 9).
//! The page is plain HTML, CSS and JavaScript in `web/`, built from the
//! approved design (not published).
// Release builds on Windows are window apps, without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod commands;
mod files;
mod jobs;
mod messages;
mod platform;
mod store;
mod text;

use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::{Manager, WindowEvent};

use commands::Shared;

fn main() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Shared::load())
        .invoke_handler(tauri::generate_handler![
            commands::startup,
            commands::page_ready,
            commands::set_appearance,
            commands::check_ffmpeg,
            commands::choose_ffmpeg,
            commands::clear_ffmpeg_path,
            commands::save_settings,
            commands::finish_setup,
            commands::choose_output_folder,
            commands::pick_inputs,
            commands::describe_items,
            commands::encode_summary,
            commands::password_strength,
            commands::measure_packed,
            commands::suggest_output,
            commands::choose_output_file,
            commands::create_key_file,
            commands::choose_key_file,
            commands::start_encode,
            commands::cancel_job,
            commands::pick_video,
            commands::probe_video,
            commands::default_restore_folder,
            commands::choose_restore_folder,
            commands::start_decode,
            commands::history_remove,
            commands::history_clear,
            commands::reveal_history,
            commands::reveal,
            commands::open_ffmpeg_site,
            commands::open_author_page,
            commands::youtube_details,
            commands::open_youtube,
            commands::start_youtube_download,
            commands::tools_status,
            commands::install_tool,
            commands::open_tool_page,
        ])
        .setup(|app| {
            // Downloads from an earlier run are not needed any more.
            if let Some(places) = &app.state::<Shared>().locations {
                let folder = places.downloads.clone();
                std::thread::spawn(move || store::clear_downloads(&folder));
            }
            if let Some(window) = app.get_webview_window("main") {
                let (appearance, material) = app
                    .state::<Shared>()
                    .settings
                    .lock()
                    .map(|s| (s.appearance, s.material))
                    .unwrap_or_default();
                commands::apply_appearance(&window, appearance, material);
                // The page shows the window once it has drawn itself. If that
                // never happens, show it anyway rather than leave nothing on screen.
                let late = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(4));
                    let _ = late.show();
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing during a job: stop the job first, so it can remove its
            // partial output, then close (the job's end quits the app).
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<Shared>();
                if state.job_running() {
                    api.prevent_close();
                    state.quit_after_job.store(true, Ordering::SeqCst);
                    state.cancel_job();
                }
            }
        })
        .run(tauri::generate_context!());
    if let Err(error) = result {
        eprintln!("ATV could not start: {error}");
        std::process::exit(1);
    }
}
