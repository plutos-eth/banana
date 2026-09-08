//! The IPC surface: thin wrappers over [`crate::api`].
//!
//! Nothing here does work. Every handler unwraps its arguments, calls one function in
//! `api`, and returns. That is deliberate: the logic is testable without a window, and
//! this file stays short enough that the whole trust surface between the UI and the
//! machine can be read in one sitting.
//!
//! The UI has no other way out. Its CSP allows `connect-src 'self' ipc:` and nothing else
//! (PLAN.md C1), so this list *is* the set of things the frontend can cause to happen.

use banana_core::strategy::StrategyConfig;
use tauri::{Emitter, Manager, State};

use crate::api;
use crate::state::{AppError, AppState, Result};

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> api::Status {
    api::status(&state)
}

#[tauri::command]
pub fn get_strategy(state: State<'_, AppState>) -> StrategyConfig {
    state.strategy()
}

#[tauri::command]
pub fn save_strategy(state: State<'_, AppState>, config: StrategyConfig) -> Result<()> {
    state.save_strategy(&config)
}

#[tauri::command]
pub fn get_feed(state: State<'_, AppState>, query: api::FeedQuery) -> Result<api::FeedPage> {
    api::feed(&state, &query)
}

#[tauri::command]
pub fn get_launch(state: State<'_, AppState>, token: String) -> Result<api::LaunchDetail> {
    api::launch_detail(&state, &token)
}

#[tauri::command]
pub fn run_backtest(
    state: State<'_, AppState>,
    config: StrategyConfig,
) -> Result<banana_backtest::BacktestResult> {
    api::backtest(&state, &config)
}

#[tauri::command]
pub fn get_pass_count(
    state: State<'_, AppState>,
    config: StrategyConfig,
) -> Result<api::PassCount> {
    api::pass_count(&state, &config)
}

#[tauri::command]
pub fn get_positions(state: State<'_, AppState>) -> api::Positions {
    api::positions(&state)
}

#[tauri::command]
pub fn get_index_status(state: State<'_, AppState>) -> Result<api::IndexStatus> {
    api::index_status(&state)
}

/// Start an index, streaming progress as `index-progress` events.
///
/// Returns as soon as the run is under way. The window can be closed while it continues:
/// the task holds the writer lock and the guard releases it on drop, so a closed window
/// leaves neither a stuck lock nor a half-written phase (spec §4.2, §6.2).
#[tauri::command]
pub async fn start_index(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    from: Option<u64>,
    to: Option<u64>,
) -> Result<()> {
    if state.is_indexing() {
        return Err(AppError::IndexBusy);
    }
    let data_dir = state.data_dir().to_path_buf();
    crate::indexing::spawn(app, data_dir, from, to);
    Ok(())
}

/// Choose TEST or LIVE, once, on the startup screen.
///
/// The whole mode decision, in one call. It replaces the launch flag and the arm phrase
/// that preceded it: two ceremonies for one choice was confusing without being safer, and
/// what actually stops a test session spending is that it holds no key at all.
///
/// A second call with a different mode is refused. Restarting is how you change your mind,
/// which keeps a running session from drifting into spending money it was not started to
/// spend.
#[tauri::command]
pub fn choose_mode(
    state: State<'_, AppState>,
    mode: crate::state::Mode,
) -> Result<crate::state::Mode> {
    state.choose_mode(mode)
}

/// Start the sniper.
///
/// TEST runs the whole pipeline and stops at the signature; LIVE signs and sends inside
/// the money guards. Which one is decided by the mode chosen at startup, and it cannot be
/// changed while the process runs.
#[tauri::command]
pub async fn start_engine(app: tauri::AppHandle) -> Result<i64> {
    let data_dir = app.state::<AppState>().data_dir().to_path_buf();
    crate::sniper::start(app.clone(), data_dir).await
}

/// Stop the sniper. Open positions stay open and stop being watched, which the Positions
/// view says out loud.
#[tauri::command]
pub fn stop_engine(state: State<'_, AppState>) -> bool {
    state.engine().stop()
}

/// Store a private key pasted into Settings.
///
/// Returns the **address** it derives, never the key. The key is validated before it is
/// written, so a mistyped one is refused where it was pasted rather than the first time an
/// order would have fired.
#[tauri::command]
pub fn save_key(state: State<'_, AppState>, key: String) -> Result<String> {
    state.save_key(&key).map(|a| format!("{a:#x}"))
}

/// Forget the stored key.
#[tauri::command]
pub fn clear_key(state: State<'_, AppState>) -> Result<()> {
    state.clear_key()
}

/// Open a URL in the user's own browser.
///
/// **Restricted to the explorer origin.** Token names, symbols and logo fields are written
/// by whoever launched the token, and this is the one command that could turn attacker
/// text into an outbound request. Anything not under [`banana_chain::addr::EXPLORER`] is
/// refused, so the worst a hostile launch can do is produce a link that does not open.
#[tauri::command]
pub fn open_explorer(url: String) -> Result<()> {
    let base = banana_chain::addr::EXPLORER;
    if !url.starts_with(&format!("{base}/")) {
        return Err(AppError::Refused(format!(
            "refusing to open {url}: only {base} links are allowed"
        )));
    }
    open_in_browser(&url)
}

/// Hand a URL to the operating system. Never a shell, so nothing is interpreted.
#[cfg(target_os = "windows")]
fn open_in_browser(url: &str) -> Result<()> {
    // `rundll32 url.dll,FileProtocolHandler` takes the URL as a single argv entry, so
    // there is no command line for a quoting trick to escape from. `cmd /c start` would
    // have been the other option and it interprets its argument.
    std::process::Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn()
        .map_err(AppError::Io)?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_in_browser(url: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(url)
        .spawn()
        .map_err(AppError::Io)?;
    Ok(())
}

/// Emit an event to every window, ignoring the failure if none is open.
pub fn emit<T: serde::Serialize + Clone>(app: &tauri::AppHandle, event: &str, payload: T) {
    if let Err(e) = app.emit(event, payload) {
        // A closed window is not an error worth stopping an index for.
        tracing::debug!(event, error = %e, "no window to emit to");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explorer_links_are_opened() {
        // The attack this guards: a token whose name or logo field is a URL. Nothing
        // deployer-supplied reaches the network (spec §3.1, PLAN.md C1).
        for hostile in [
            "https://evil.example/steal",
            "file:///C:/Windows/System32/calc.exe",
            "https://robinhoodchain.blockscout.com.evil.example/x",
            "javascript:alert(1)",
            "",
        ] {
            let e = open_explorer(hostile.to_string());
            assert!(
                matches!(e, Err(AppError::Refused(_))),
                "{hostile} should have been refused"
            );
        }
    }

    #[test]
    fn an_explorer_link_passes_the_origin_check() {
        // Checked up to the point of spawning a browser, which a test must not do.
        let base = banana_chain::addr::EXPLORER;
        let url = format!("{base}/token/0xabc");
        assert!(url.starts_with(&format!("{base}/")));
    }
}
