//! `banana` — thin headless entrypoint for `index` and `doctor`.
//!
//! The desktop app is the primary surface (spec §10). This binary exists so a long index
//! can run from a terminal or a cron job, and so `doctor` can be scripted.
//!
//! **No command here requires a private key**, and this crate deliberately does not depend
//! on `banana-live`, which is what makes that guarantee mechanical rather than
//! aspirational (PLAN.md C6). `scripts/check-trust-boundary.ps1` enforces it in CI.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use banana_chain::gate::{Gate, GateConfig, default_endpoints, parse_endpoints};
use banana_chain::rpc::Client;
use banana_chain::transport::{LiveTransport, RecordingTransport, ReplayTransport, Transport};
use banana_chain::{addr, doctor};
use banana_core::strategy::StrategyConfig;
use banana_indexer::run::{IndexPlan, run as run_index};
use banana_indexer::verify;
use banana_store::{History, Lock};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "banana",
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

    /// Check the indexed data against itself and report threshold evidence.
    ///
    /// Needs no network: everything here is derived from the local store.
    Verify {
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
        /// Stop after this many curves, for a quick read on a large store.
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Run a saved strategy over the indexed window.
    ///
    /// Needs no network. Prints the funnel, the §5.5 guards and the §5.4 framing exactly
    /// as the Lab will show them, so the terminal and the UI cannot drift apart.
    Backtest {
        /// A `StrategyConfig` as JSON. Defaults to the built-in baseline of §7.1.
        #[arg(long, value_name = "FILE")]
        strategy: Option<PathBuf>,
        #[arg(long, default_value = "data")]
        data_dir: PathBuf,
        /// Print the whole result as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
        /// Write the default strategy to stdout and exit, as a starting point to edit.
        #[arg(long)]
        print_default: bool,
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
    ) -> Result<banana_chain::transport::HttpResponse, banana_chain::transport::TransportError>
    {
        self.0.post(url, body).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "banana=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match &cli.command {
        Command::Doctor { probe, curve } => {
            println!("banana doctor — chain {}", addr::CHAIN_ID);
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

            let head = client.block_number(banana_chain::Priority::Bulk).await?;
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

            // Owned by the closure, which the indexer keeps for the length of the run.
            let mut last_line = std::time::Instant::now();
            let mut reported_done: Option<&'static str> = None;
            let report = run_index(
                &client,
                &mut history,
                &plan,
                Box::new(move |e| {
                    // Throttled to twice a second, but a completed phase always prints its
                    // real totals. Without that the last line of a phase is a stale snapshot:
                    // the run that found this showed "launches 856501/856501 rows 6373" for a
                    // phase that had in fact written 24,984 rows, because the final chunks
                    // landed inside the throttle window.
                    let complete = e.units_total > 0 && e.units_done >= e.units_total;
                    let first_report_of_completion =
                        complete && reported_done != Some(e.phase_label);
                    if first_report_of_completion {
                        reported_done = Some(e.phase_label);
                    }
                    if first_report_of_completion
                        || last_line.elapsed() >= Duration::from_millis(500)
                    {
                        last_line = std::time::Instant::now();
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
                }),
            )
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

        Command::Verify { data_dir, limit } => {
            let db_path = data_dir.join("history.db");
            let history = History::open_read_only(&db_path)
                .with_context(|| format!("opening {}", db_path.display()))?;

            println!("== curve replay ==");
            println!();
            let r = verify::verify_all(&history, *limit)?;
            println!("  curves replayed      {}", r.curves_checked);
            println!("  buys checked         {}", r.replay.checked);
            println!("  reproduced exactly   {}", r.replay.exact);
            println!("  mismatched           {}", r.replay.mismatched);
            println!("  errored              {}", r.replay.errored);
            if !r.curves_with_mismatch.is_empty() {
                println!(
                    "  first mismatches:    {}",
                    r.curves_with_mismatch.join(", ")
                );
            }
            let pct_exact = if r.replay.checked > 0 {
                r.replay.exact as f64 * 100.0 / r.replay.checked as f64
            } else {
                100.0
            };
            println!();
            println!("  exact                {pct_exact:.1}% of buys");
            println!(
                "  verdict: {}",
                if r.is_exact() {
                    "EXACT - every buy reproduces its event".to_string()
                } else {
                    // Not a failure. A curve whose replay does not reproduce reality is
                    // refused a reconstructed entry rather than given a plausible wrong
                    // one, so the inexactness is bounded and visible instead of silent.
                    format!(
                        "{} curve(s) do not reproduce exactly; those are refused a                          reconstructed entry rather than given a wrong one",
                        r.curves_with_mismatch.len()
                    )
                }
            );

            let (buys, sells, taxed) = verify::side_counts(&history)?;
            println!();
            println!("== trades ==");
            println!();
            println!("  buys                 {buys}");
            println!("  sells                {sells}");
            println!(
                "  snipe-taxed buys     {taxed} ({:.1}% of buys)",
                if buys > 0 {
                    taxed as f64 * 100.0 / buys as f64
                } else {
                    0.0
                }
            );

            let d = verify::distributions(&history)?;
            println!();
            println!("== universe ==");
            println!();
            println!("  launches             {}", d.launches);
            println!(
                "  entry observed       {} ({:.1}%)",
                d.observed_entries,
                pct(d.observed_entries, d.launches)
            );
            println!(
                "  entry reconstructed  {} ({:.1}%)",
                d.reconstructed_entries,
                pct(d.reconstructed_entries, d.launches)
            );
            println!(
                "  undecodable launches {} ({:.1}%)",
                d.undecodable,
                pct(d.undecodable, d.launches)
            );
            println!(
                "  via a bundler        {} ({:.1}%)",
                d.bundled,
                pct(d.bundled, d.launches)
            );
            println!(
                "  migrated             {} ({:.2}%, 1 in {})",
                d.migrated,
                pct(d.migrated, d.launches),
                // "1 in N". No migrations means there is no such N, and 0 is the honest
                // stand-in here because the count beside it already says none.
                d.launches.checked_div(d.migrated).unwrap_or(0)
            );
            println!(
                "  no post-entry trade  {} ({:.1}%)",
                d.no_post_entry_trades,
                pct(d.no_post_entry_trades, d.launches)
            );
            println!(
                "  no entry at all      {} ({:.1}%)",
                d.no_entry,
                pct(d.no_entry, d.launches)
            );
            println!(
                "  farm twins > 0       {} ({:.1}%)",
                d.twins_nonzero,
                pct(d.twins_nonzero, d.launches)
            );

            println!();
            println!("== distributions, for deriving thresholds ==");
            println!();
            println!("                          p10       p25       p50       p75       p90");
            row("post-entry trades", d.post_entry_trades_p);
            row("lifespan (blocks)", d.lifespan_blocks_p);
            row("max multiple (bps)", d.max_multiple_p);
            row("5m multiple (bps)", d.mult_5m_p);
            row("deployer depth (blk)", d.depth_p);
            row("time to peak (s)", d.time_to_ath_p);
            row("trailing silence (blk)", d.quiet_blocks_p);
            println!();
            println!(
                "  never traded above entry   {} ({:.1}%)",
                d.never_above_entry,
                pct(d.never_above_entry, d.launches)
            );
            println!(
                "  below entry at 5 minutes   {} ({:.1}%)",
                d.below_entry_at_5m,
                pct(d.below_entry_at_5m, d.launches)
            );
            Ok(())
        }

        Command::Backtest {
            strategy,
            data_dir,
            json,
            print_default,
        } => {
            if *print_default {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&StrategyConfig::default())?
                );
                return Ok(());
            }

            let config = match strategy {
                Some(p) => {
                    let raw = std::fs::read_to_string(p)
                        .with_context(|| format!("reading {}", p.display()))?;
                    serde_json::from_str::<StrategyConfig>(&raw)
                        .with_context(|| format!("parsing {} as a strategy", p.display()))?
                }
                None => StrategyConfig::default(),
            };

            let db_path = data_dir.join("history.db");
            let history = History::open_read_only(&db_path)
                .with_context(|| format!("opening {}", db_path.display()))?;
            let result = banana_backtest::run(&history, &config)?;

            if *json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            print_backtest(&result);
            Ok(())
        }
    }
}

/// The terminal rendering of a backtest, kept beside the Lab's so they cannot drift.
///
/// Everything §5.5 makes mandatory is printed unconditionally: the funnel, the effective
/// universe, the cutoff, and the regime warning.
fn print_backtest(r: &banana_backtest::BacktestResult) {
    use banana_backtest::metrics::Results;

    println!();
    println!(
        "== window ==  blocks {}..{} ({:.1} h), maturity cutoff {} h",
        r.window.from_block,
        r.window.to_block,
        r.window.hours_x10 as f64 / 10.0,
        r.window.maturity_cutoff_hours
    );
    println!();
    println!("== funnel ==");
    println!();
    for s in &r.funnel.stages {
        // `of` matters where the funnel branches: `reached_target` and `migrated` both
        // narrow `priced` and neither narrows the other, so printing them as a chain would
        // read as a containment that is not there.
        let base = match &s.of {
            Some(of) if of != "all_launches" => format!("  (of {of})"),
            _ => String::new(),
        };
        println!(
            "  {:>9}  -{:<8}  {}{}",
            s.remaining, s.removed, s.label, base
        );
    }

    println!();
    match &r.results {
        Results::InsufficientSample {
            passed, required, ..
        } => {
            // Spec §5.5: not a warning beside a number. There is no number.
            println!("== result ==");
            println!();
            println!("  sample too small — {passed} tokens passed, {required} needed");
            println!("  no hit rate and no distribution is shown at this sample size.");
        }
        Results::Measured(m) => {
            println!("== result ==");
            println!();
            println!("  target      {}", m.target);
            println!(
                "  hit rate    {:.1}%  ({} of {})",
                m.hit_rate_bps as f64 / 100.0,
                m.hits,
                m.measured_over
            );
            println!("  migrations  {}", m.migrations);
            println!();
            println!("  == held for a fixed time, which is what a rule could have done ==");
            for h in [&m.hold_5m, &m.hold_30m] {
                println!();
                println!("  {}", h.assumption);
                println!(
                    "    p10 {:<8} p25 {:<8} p50 {:<8} p75 {:<8} p90 {:<8}",
                    mult(h.multiple.p10),
                    mult(h.multiple.p25),
                    mult(h.multiple.p50),
                    mult(h.multiple.p75),
                    mult(h.multiple.p90)
                );
                println!(
                    "    above entry {}   below entry {}   measured over {}",
                    h.above_entry, h.below_entry, h.measured_over
                );
            }
            println!();
            println!("  == {} ==", m.peak.label);
            println!(
                "    p10 {:<8} p25 {:<8} p50 {:<8} p75 {:<8} p90 {:<8}",
                mult(m.peak.multiple.p10),
                mult(m.peak.multiple.p25),
                mult(m.peak.multiple.p50),
                mult(m.peak.multiple.p75),
                mult(m.peak.multiple.p90)
            );
            println!(
                "    never traded above entry {} of {}",
                m.peak.never_above_entry, m.peak.measured_over
            );
        }
    }

    println!();
    println!("== regime ==");
    println!();
    for line in wrap(&r.regime_warning, 76) {
        println!("  {line}");
    }
    println!();
    println!("  light-half query {} ms", r.query_ms);
}

/// Basis points as a multiple, without floating point (spec §12).
fn mult(bps: u64) -> String {
    format!("{}.{:02}x", bps / 10_000, (bps % 10_000) / 100)
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

fn pct(n: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        n as f64 * 100.0 / total as f64
    }
}

fn row(label: &str, p: [u64; 5]) {
    println!(
        "  {label:<22}{:>9} {:>9} {:>9} {:>9} {:>9}",
        p[0], p[1], p[2], p[3], p[4]
    );
}
