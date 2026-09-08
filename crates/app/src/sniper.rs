//! Running the engine from the window, without the engine belonging to the window.
//!
//! Same rule as `indexing.rs` and for the same reason (spec §4.2): the task holds an
//! `AppHandle`, not a window, so closing or reloading the UI means events go nowhere — not
//! that a position stops being watched. The `p` shortcut starts and stops it; nothing else
//! does, and it cannot be started twice.
//!
//! # Why the journal lock is taken here and not at startup
//!
//! `live.db`'s writer lock belongs to whoever is trading. Taking it when the engine starts
//! rather than when the application opens means a second window can still be used to look
//! at a backtest while the first one trades, and a crash releases it with the process.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use banana_chain::gate::{Gate, GateConfig, default_endpoints, parse_endpoints};
use banana_chain::rpc::Client;
use banana_chain::transport::LiveTransport;
use banana_live::{Engine, EngineConfig, Event, Mode, Session};
use banana_store::{History, Journal, Lock};
use tauri::Manager;
use tokio::sync::mpsc;

use crate::commands::emit;
use crate::state::{AppError, AppState, Result};

/// What the application holds while the engine runs.
///
/// Dropping it stops the engine and releases the journal's writer lock.
#[derive(Debug)]
pub struct Running {
    stop: mpsc::Sender<()>,
    pub session_id: i64,
    /// Held for as long as the engine is: this is what stops two of them trading at once.
    _lock: Lock,
}

/// The engine's handle, or nothing.
#[derive(Debug, Default)]
pub struct Handle(Mutex<Option<Running>>);

impl Handle {
    pub fn is_running(&self) -> bool {
        self.0.lock().expect("engine lock").is_some()
    }

    pub fn session_id(&self) -> Option<i64> {
        self.0
            .lock()
            .expect("engine lock")
            .as_ref()
            .map(|r| r.session_id)
    }

    fn set(&self, running: Running) {
        *self.0.lock().expect("engine lock") = Some(running);
    }

    /// Ask the engine to stop. It finishes the sweep it is in and writes its end.
    pub fn stop(&self) -> bool {
        match self.0.lock().expect("engine lock").take() {
            Some(r) => {
                // A full channel means a stop is already queued, which is the same outcome.
                let _ = r.stop.try_send(());
                true
            }
            None => false,
        }
    }

    fn clear(&self) {
        *self.0.lock().expect("engine lock") = None;
    }
}

/// Start the engine for this session's mode.
pub async fn start(app: tauri::AppHandle, data_dir: PathBuf) -> Result<i64> {
    let state = app.state::<AppState>();
    let Some(mode) = state.mode() else {
        return Err(AppError::Refused(
            "choose TEST or LIVE before starting the engine".into(),
        ));
    };
    if state.engine().is_running() {
        return Err(AppError::Refused("the engine is already running".into()));
    }

    // Taken before anything else: two engines trading the same wallet from the same
    // directory would each believe they owned the session budget.
    let lock = Lock::acquire(data_dir.join("live.db")).map_err(banana_store::StoreError::from)?;
    let journal = Journal::open(data_dir.join("live.db"))?;

    let strategy = state.strategy();
    let session = match mode {
        Mode::Test => Session::test(strategy.live_guards.clone()),
        Mode::Live => Session::live(strategy.live_guards.clone(), &data_dir)
            .map_err(|e| AppError::Refused(e.to_string()))?,
    };

    // Read-only, and only if an index exists. The engine reads deployer history from it
    // and never writes, so an index can run from a terminal at the same time.
    let history = History::open_read_only(data_dir.join("history.db")).ok();

    let config = EngineConfig {
        // In TEST this is the saved wallet if there is one, so a rehearsal is priced
        // against a real balance. In LIVE the engine overrides it with the signing wallet.
        wallet: state.wallet(),
        ..EngineConfig::default()
    };

    let (events, mut rx) = mpsc::channel::<Event>(256);
    let (stop_tx, stop_rx) = mpsc::channel::<()>(1);

    let engine = Engine::start(
        client()?,
        session,
        strategy,
        journal,
        history,
        config,
        events,
    )
    .await
    .map_err(|e| AppError::Refused(e.to_string()))?;
    let session_id = engine.session_id();

    state.engine().set(Running {
        stop: stop_tx,
        session_id,
        _lock: lock,
    });

    // The pump. Separate from the engine so a slow window cannot stall a trade: the
    // channel is bounded, and if the UI is gone the sends simply go nowhere.
    let pump = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(e) = rx.recv().await {
            emit(&pump, "engine", &e);
        }
    });

    let done = app.clone();
    tauri::async_runtime::spawn(async move {
        engine.run(stop_rx).await;
        // Whatever happened — stopped, or the watcher died — the handle must not go on
        // claiming to be running, or `p` would never start it again.
        done.state::<AppState>().engine().clear();
        emit(&done, "engine-stopped", ());
    });

    Ok(session_id)
}

fn client() -> Result<Client> {
    let endpoints = match std::env::var("RPC_URL") {
        Ok(raw) if !raw.trim().is_empty() => parse_endpoints(&raw),
        _ => default_endpoints(),
    };
    if endpoints.is_empty() {
        return Err(AppError::Refused("RPC_URL parsed to no endpoints".into()));
    }
    let transport = LiveTransport::new(Duration::from_secs(20))
        .map_err(|e| AppError::Refused(e.to_string()))?;
    Ok(Client::new(Gate::new(
        Box::new(transport),
        endpoints,
        GateConfig::default(),
    )))
}
