//! `quarrel-app` — Tauri backend: commands, events, state and the engine supervisor.
//!
//! The engine does not live in the window (spec §4.2). It runs as a supervised
//! background task with its own lifecycle; the UI is a view over it. Closing or
//! reloading the window must not stop trading, and a UI panic must not take positions
//! with it. UI→engine traffic goes through Tauri commands, engine→UI through events.

#![forbid(unsafe_code)]

use tauri::Manager;

pub mod api;
pub mod commands;
pub mod indexing;
pub mod sniper;
pub mod state;

pub use state::{AppError, AppState, Mode};

/// Where the store, the strategy and the key live.
///
/// Four places, in order, and the order is what makes both a developer checkout and an
/// installed application work without either knowing about the other:
///
/// 1. `--data-dir <path>`, which wins outright.
/// 2. `./data`, **if it already exists**. A checkout has one, so `cargo run` from the
///    repository, or a cron job started there, uses the store you indexed there. It has
///    to be "if it exists": launched from a desktop icon the working directory is
///    wherever the icon points, and a bare relative path found an empty store and
///    reported no launches.
/// 3. `<exe dir>/data`, **if it already exists**. This was the fallback before there was
///    a per-OS one, so an existing portable install keeps its store instead of silently
///    starting empty beside it.
/// 4. The platform's own application-data directory, created if missing.
///
/// The last one is not a nicety. On macOS an installed binary lives inside
/// `Quarrel.app/Contents/MacOS`, so writing beside the executable puts a 1 GB database
/// inside the bundle — which breaks code signing, is thrown away by the next update, and
/// on Linux is simply not writable when the binary sits in `/usr/bin`.
fn data_dir() -> std::path::PathBuf {
    if let Some(explicit) = std::env::args()
        .skip_while(|a| a != "--data-dir")
        .nth(1)
        .map(std::path::PathBuf::from)
    {
        return explicit;
    }
    let cwd = std::path::PathBuf::from("data");
    if cwd.is_dir() {
        return cwd;
    }
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("data")))
        .filter(|d| d.is_dir())
    {
        return beside;
    }
    platform_data_dir()
}

/// The conventional place for an application's own files on this platform.
///
/// Resolved from the environment rather than through a crate: it is three rules, they do
/// not change, and a dependency that reads the same variables is not more correct for
/// being someone else's.
fn platform_data_dir() -> std::path::PathBuf {
    let home = |var: &str| std::env::var_os(var).map(std::path::PathBuf::from);

    #[cfg(target_os = "windows")]
    let base = home("APPDATA").or_else(|| home("LOCALAPPDATA"));

    #[cfg(target_os = "macos")]
    let base = home("HOME").map(|h| h.join("Library").join("Application Support"));

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = home("XDG_DATA_HOME").or_else(|| home("HOME").map(|h| h.join(".local/share")));

    // A machine with neither a home directory nor the variables that name one is not one
    // this application can be installed on, so the working directory is the honest last
    // resort rather than a panic on startup.
    base.unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("quarrel")
}

/// Build and run the desktop application.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "quarrel_app=info,quarrel_indexer=info".into()),
        )
        .init();

    let app = tauri::Builder::default()
        .manage(AppState::new(data_dir()))
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_strategy,
            commands::save_strategy,
            commands::get_feed,
            commands::get_launch,
            commands::run_backtest,
            commands::get_pass_count,
            commands::get_positions,
            commands::get_index_status,
            commands::start_index,
            commands::open_explorer,
            commands::choose_mode,
            commands::start_engine,
            commands::stop_engine,
            commands::save_key,
            commands::clear_key,
        ])
        .build(tauri::generate_context!())
        .expect("the desktop shell failed to start");

    // Spec §4.2: closing the window must not stop work that is under way. Tauri exits the
    // process when the last window closes, which would abandon an index mid-phase, so the
    // exit is deferred until the work finishes and then taken automatically. The user
    // closed the window and gets no window back; what they do not get is a half-written
    // store or a process they have to find in Task Manager.
    //
    // Phase 6's engine is long-running rather than finite, so it will need a tray icon to
    // stay reachable. That is a phase-6 problem; this is the phase-5 half of the same rule.
    app.run(|handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = &event {
            let state = handle.state::<AppState>();
            if state.is_indexing() {
                tracing::info!("window closed while indexing; finishing the run first");
                api.prevent_exit();
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    while handle.state::<AppState>().is_indexing() {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    tracing::info!("index finished; exiting");
                    handle.exit(0);
                });
            }
        }
    });
}
