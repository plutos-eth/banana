//! `quarrel` — thin headless entrypoint for `index` and `doctor`.
//!
//! The desktop app is the primary surface (spec §10). This binary exists so a long index
//! can run from a terminal or a cron job, and so `doctor` can be scripted.
//!
//! **No command here requires a private key**, and this crate deliberately does not depend
//! on `quarrel-live`, which is what makes that guarantee mechanical rather than
//! aspirational (PLAN.md C6). `scripts/check-trust-boundary.ps1` enforces it in CI.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use quarrel_chain::gate::{Gate, GateConfig, default_endpoints, parse_endpoints};
use quarrel_chain::rpc::Client;
use quarrel_chain::transport::{LiveTransport, RecordingTransport, ReplayTransport, Transport};
use quarrel_chain::{addr, doctor};
use quarrel_indexer::run::{IndexPlan, run as run_index};
use quarrel_store::{History, Lock};

#[derive(Parser, Debug)]
#[command(
    name = "quarrel",
    version,
    about = "Local sniper terminal and strategy backtester for pons v2.",
    long_about = "Every command here runs with no private key. Real money moves only in the \
                  desktop app, launched with --live."
)]
struct Cli {
    /// Comma-separated RPC endpoints, preferred first. Append #nologs to one that refuses
    /// eth_getLogs. Falls back to RPC_URL, then to the built-in public pair.
    #[arg(long, global = true)]
    rpc_url: Option<String>,

    /// Record every RPC exchange to this directory, for offline replay.
    ///
    /// A full 24-hour index is ~830 MB and 20-30 minutes, which is not something to re-run
    /// on every iteration. A recording of a small window stands in for the chain and makes
    /// the indexer and backtest iterate in seconds (PLAN.md D12).
    #[arg(long, global = true, value_name = "DIR")]
    record: Option<PathBuf>,

    /// Serve every RPC call from a recording instead of the network.
    ///
    /// A cache miss is a hard error, never a fallthrough to the chain: a replay that
    /// quietly reached the network would give offline runs a hidden dependency on whatever
    /// the chain looked like that day.
    #[arg(long, global = true, value_name = "DIR", conflicts_with = "record")]
    replay: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check that the chain still looks the way this build assumes.
    Doctor {
        /// Talk to the chain. Without this only offline checks run.
        #[arg(long)]
        probe: bool,
        /// Additionally probe one curve address, to confirm the curve ABI still matches.
        #[arg(long)]
        curve: Option<String>,
    },

    /// Index on-chain launch history into the local store.
    ///
    /// Resumable: an interrupted run picks up from its last checkpoint rather than
    /// restarting. Re-running a covered range writes zero rows.
    Index {
        /// First block. Defaults to 24 hours before the head.
        #[arg(long)]
        from: Option<u64>,
        /// Last block. Defaults to the current head.
        #[arg(long)]
        to: Option<u64>,
        /// Append only blocks since the last index, instead of a fresh window.
        ///
        /// Note that at a daily cadence this covers ~856,500 blocks -- the same as a first
        /// run. It is only cheap if run more often.
        #[arg(long, conflicts_with_all = ["from", "to"])]
        update: bool,
        /// Skip the launch-calldata phase.
        ///
        /// Much faster, but socials and exempt-wallet counts stay Unknown, and the Lab
        /// will refuse any filter that reads them rather than treating missing as absent.
        #[arg(long)]
        skip_calldata: bool,
        /// Blocks between sampled timestamp anchors.
        #[arg(long, default_value_t = 500)]
        anchor_every: u64,
        /// Concurrent transaction fetches in the calldata phase.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Where the databases live.
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
    },
}

/// Returns the client and, when recording, the transport to flush afterwards.
fn build_client(cli: &Cli) -> Result<(Client, Option<std::sync::Arc<RecordingTransport>>)> {
    let endpoints = match cli
        .rpc_url
        .clone()
        .or_else(|| std::env::var("RPC_URL").ok())
    {
        Some(raw) if !raw.trim().is_empty() => {
            let eps = parse_endpoints(&raw);
            anyhow::ensure!(!eps.is_empty(), "RPC_URL parsed to no endpoints");
            eps
        }
        _ => default_endpoints(),
    };

    let mut recorder = None;
    let transport: Box<dyn Transport> = if let Some(dir) = &cli.replay {
        let t = ReplayTransport::load(dir)
            .with_context(|| format!("loading the recording at {}", dir.display()))?;
        eprintln!(
            "replaying {} recorded responses from {}",
            t.len(),
            dir.display()
        );
        Box::new(t)
    } else {
        let live =
            LiveTransport::new(Duration::from_secs(20)).context("building the HTTP transport")?;
        match &cli.record {
            Some(dir) => {
                let rec = std::sync::Arc::new(RecordingTransport::new(Box::new(live), dir));
                recorder = Some(rec.clone());
                Box::new(SharedRecorder(rec))
            }
            None => Box::new(live),
        }
    };

    let gate = Gate::new(transport, endpoints, GateConfig::default());
    Ok((Client::new(gate), recorder))
}

/// Lets the CLI keep a handle on the recorder so it can flush after the run, while the
/// gate owns its own boxed transport.
#[derive(Debug)]
struct SharedRecorder(std::sync::Arc<RecordingTransport>);

#[async_trait::async_trait]
impl Transport for SharedRecorder {
    async fn post(
        &self,
        url: &str,
        body: &str,
    ) -> Result<quarrel_chain::transport::HttpResponse, quarrel_chain::transport::TransportError>
    {
        self.0.post(url, body).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "quarrel=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match &cli.command {
        Command::Doctor { probe, curve } => {
            println!("quarrel doctor — chain {}", addr::CHAIN_ID);
            println!();

            let mut report = doctor::offline();
            if *probe || cli.replay.is_some() {
                let (client, recorder) = build_client(&cli)?;
                let live = doctor::probe(&client).await;
                report.checks.extend(live.checks);

                if let Some(c) = curve {
                    let parsed = c.parse().context("--curve is not an address")?;
                    let cr = doctor::probe_curve(&client, parsed).await;
                    report.checks.extend(cr.checks);
                }

                if let Some(rec) = recorder {
                    let n = rec.flush().context("writing the recording")?;
                    eprintln!("recorded {n} responses");
                }
            } else {
                println!("  (offline checks only; pass --probe to verify against the chain)\n");
            }

            print!("{}", report.render());

            if !report.ok() {
                anyhow::bail!("{} check(s) failed", report.failures());
            }
            Ok(())
        }

        Command::Index {
            from,
            to,
            update,
            skip_calldata,
            anchor_every,
            concurrency,
            data_dir,
        } => {
            let (client, recorder) = build_client(&cli)?;
            let db_path = data_dir.join("history.db");

            // Per-file, not per-directory: this is what lets the index run from cron while
            // the desktop app is open and trading (PLAN.md C3).
            let _lock = Lock::acquire(&db_path).context("taking the history writer lock")?;
            let mut history = History::open(&db_path).context("opening history.db")?;

            let head = client.block_number(quarrel_chain::Priority::Bulk).await?;
            let mut plan = IndexPlan::last_24h(head);
            plan.skip_calldata = *skip_calldata;
            plan.anchor_every = *anchor_every;
            plan.calldata_concurrency = *concurrency;

            if *update {
                // Resume from the furthest block any phase reached.
                let last = ["launches", "trades", "anchors"]
                    .iter()
                    .filter_map(|p| history.phase_state(p).ok().flatten())
                    .map(|s| s.last_block)
                    .max();
                if let Some(last) = last {
                    plan.from_block = last;
                }
                plan.to_block = head;
            } else {
                if let Some(f) = from {
                    plan.from_block = *f;
                }
                if let Some(t) = to {
                    plan.to_block = *t;
                }
            }

            anyhow::ensure!(
                plan.from_block <= plan.to_block,
                "empty range: {} > {}",
                plan.from_block,
                plan.to_block
            );

            // Say the span up front. A daily --update is the same size as a first run, and
            // the README says so, but seeing the number beats reading the note.
            println!(
                "indexing blocks {}..{} ({} blocks, ~{:.1} h at 100.87 ms){}",
                plan.from_block,
                plan.to_block,
                plan.blocks(),
                plan.blocks() as f64 * 0.10087 / 3600.0,
                if plan.skip_calldata {
                    ", calldata skipped"
                } else {
                    ""
                }
            );
            println!();

            let mut last_line = std::time::Instant::now();
            let report = run_index(&client, &mut history, &plan, |p| {
                if last_line.elapsed() >= Duration::from_millis(500) {
                    last_line = std::time::Instant::now();
                    let e = p.event();
                    println!(
                        "  {:>5.1}%  {:<16} {:>9}/{:<9}  rows {:<9} {}",
                        e.percent_x10 as f64 / 10.0,
                        e.phase_label,
                        e.units_done,
                        e.units_total,
                        e.rows_written,
                        e.eta_secs
                            .map(|s| format!("eta {}m{:02}s", s / 60, s % 60))
                            .unwrap_or_default()
                    );
                }
            })
            .await?;

            if let Some(rec) = recorder {
                let n = rec.flush().context("writing the recording")?;
                eprintln!("recorded {n} responses");
            }

            println!();
            println!(
                "done in {}m{:02}s",
                report.elapsed_secs / 60,
                report.elapsed_secs % 60
            );
            println!("  launches      {}", history.launch_count()?);
            println!("  trades        {}", history.trade_count()?);
            println!("  enrichment    {}", report.enrichment_rows);
            println!("  anchors       {}", report.anchor_rows);
            println!("  outcomes      {}", report.outcomes_written);
            println!();
            println!(
                "  entry rule:   {} observed, {} reconstructed",
                report.observed_entries, report.reconstructed_entries
            );
            println!("  undecodable:  {} launches", report.undecodable_launches);
            if let Ok(md) = std::fs::metadata(&db_path) {
                println!("  history.db    {:.1} MB", md.len() as f64 / 1_048_576.0);
            }
            Ok(())
        }
    }
}
