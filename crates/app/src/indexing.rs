//! Running an index from the window, without the index belonging to the window.
//!
//! Spec §4.2: closing or reloading the UI must not stop work that is under way. The task
//! spawned here owns the writer lock through an [`crate::state::IndexGuard`] and holds an
//! `AppHandle` rather than a window handle, so a closed window means events go nowhere —
//! not that the index stops. When it finishes, the lock is released whether it succeeded,
//! failed or panicked, because the guard releases on drop.

use std::path::PathBuf;
use std::time::Duration;

use banana_chain::gate::{Gate, GateConfig, default_endpoints, parse_endpoints};
use banana_chain::rpc::Client;
use banana_chain::transport::LiveTransport;
use banana_indexer::run::{IndexPlan, run as run_index};
use banana_store::History;
use serde::Serialize;
use tauri::Manager;

use crate::commands::emit;
use crate::state::AppState;

/// Streamed to the UI as `index-progress`. Mirrors the indexer's own event so the
/// progress bar shows the real four-phase weighting rather than a spinner.
#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub phase: String,
    pub units_done: u64,
    pub units_total: u64,
    pub percent_x10: u32,
    pub rows_written: u64,
    pub eta_secs: Option<u64>,
}

/// Sent once, as `index-done`, whatever the outcome.
#[derive(Debug, Clone, Serialize)]
pub struct DonePayload {
    pub ok: bool,
    /// Present only on failure, and shown verbatim: an index that stopped for a reason the
    /// user cannot see is indistinguishable from one that hung.
    pub error: Option<String>,
    pub elapsed_secs: u64,
    pub launches: u64,
    pub trades: u64,
}

pub fn spawn(app: tauri::AppHandle, data_dir: PathBuf, from: Option<u64>, to: Option<u64>) {
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let result = run(&app, data_dir, from, to).await;
        let (ok, error, launches, trades) = match result {
            Ok((l, t)) => (true, None, l, t),
            Err(e) => {
                tracing::error!(error = %e, "index failed");
                (false, Some(e.to_string()), 0, 0)
            }
        };
        emit(
            &app,
            "index-done",
            DonePayload {
                ok,
                error,
                elapsed_secs: started.elapsed().as_secs(),
                launches,
                trades,
            },
        );
    });
}

async fn run(
    app: &tauri::AppHandle,
    data_dir: PathBuf,
    from: Option<u64>,
    to: Option<u64>,
) -> anyhow::Result<(u64, u64)> {
    let state = app.state::<AppState>();
    // Taking the guard is what closes the read-only handle and claims the writer lock.
    // Held for the whole run; released on drop, including on the error paths below.
    let _guard = state.begin_index()?;

    let client = client()?;
    let mut history = History::open(data_dir.join("history.db"))?;

    let head = client
        .block_number(banana_chain::gate::Priority::Bulk)
        .await?;
    let plan = match (from, to) {
        (Some(f), Some(t)) => IndexPlan {
            from_block: f,
            to_block: t,
            ..IndexPlan::last_24h(head)
        },
        _ => IndexPlan::last_24h(head),
    };

    let app_for_sink = app.clone();
    let report = run_index(
        &client,
        &mut history,
        &plan,
        Box::new(move |e| {
            emit(
                &app_for_sink,
                "index-progress",
                ProgressPayload {
                    phase: e.phase_label.to_string(),
                    units_done: e.units_done,
                    units_total: e.units_total,
                    percent_x10: e.percent_x10,
                    rows_written: e.rows_written,
                    eta_secs: e.eta_secs,
                },
            );
        }),
    )
    .await?;

    Ok((report.launch_rows, report.trade_rows))
}

fn client() -> anyhow::Result<Client> {
    let endpoints = match std::env::var("RPC_URL") {
        Ok(raw) if !raw.trim().is_empty() => parse_endpoints(&raw),
        _ => default_endpoints(),
    };
    anyhow::ensure!(!endpoints.is_empty(), "RPC_URL parsed to no endpoints");
    let transport = LiveTransport::new(Duration::from_secs(20))?;
    Ok(Client::new(Gate::new(
        Box::new(transport),
        endpoints,
        GateConfig::default(),
    )))
}
