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
    }
}
