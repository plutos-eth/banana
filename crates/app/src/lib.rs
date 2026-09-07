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
pub mod state;

pub use state::{AppError, AppState, Mode};

/// Where the store and the strategy live.
///
/// Relative to the working directory rather than to a per-user application folder, because
/// `history.db` is a gigabyte a day and the user should be able to see it, move it and
/// point a cron job at the same path (spec sec.10). `--data-dir` overrides it.
fn data_dir() -> std::path::PathBuf {
    std::env::args()
        .skip_while(|a| a != "--data-dir")
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("data"))
}

/// Whether this process was launched with `--live` (spec §3.2, PLAN.md C7).
///
/// A launch flag and never a button: a process started without it cannot be talked into
/// being one with it, which is what makes "real money moves only behind an explicit flag"
/// a property of the process rather than of a check somebody might skip.
fn live_flag() -> bool {
    std::env::args().any(|a| a == "--live")
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
        .manage(AppState::new(data_dir(), live_flag()))
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
            commands::arm,
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
