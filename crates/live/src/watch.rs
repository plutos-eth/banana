//! Detect — seeing a launch while it can still be sniped.
//!
//! The first step of the live pipeline, and the only one that has to keep up with the
//! chain rather than with the user.
//!
//! # Polling, and what it actually costs
//!
//! There is no WebSocket transport in this workspace; every request goes through the gate
//! as HTTP JSON-RPC. So this polls `eth_getLogs` on the factory at [`Priority::Hot`].
//!
//! The plan assumed a 200 ms poll. It is not reachable: the gate spaces `eth_getLogs` at
//! **400 ms**, because that is what the only endpoint serving logs tolerates (measured
//! 2026-09-07 — 7 of 8 calls succeeded at 500 ms spacing, and short bursts are refused
//! immediately). Polling faster than the gate's spacing does not poll faster, it queues.
//! So the interval here is 450 ms and the honest latency budget is:
//!
//! | stage | cost |
//! |---|---|
//! | waiting for the next poll | 0–450 ms, mean ~225 ms |
//! | `eth_blockNumber` + `eth_getLogs` | ~100–300 ms |
//! | **launch visible** | **~300–750 ms after the block** |
//!
//! Against a 3,000 ms decay window that is 10–25% of the runway. It matters only to a
//! strategy that wants to buy inside the first second, which means paying most of a 9,900
//! bps opening tax; at the default `max_tax_bps` of 300 the buy is due at ~2,900 ms and
//! there are roughly two seconds of slack. A strategy that really does want the first
//! second needs a WebSocket subscription, and the honest thing is to measure that before
//! building it — [`Health::behind_blocks`] and [`Health::latency_ms`] are reported for
//! exactly that.
//!
//! # A restart does not replay yesterday
//!
//! A launch is snipeable for about three seconds. Resuming from a checkpoint written an
//! hour ago would hand the executor a queue of launches whose windows shut long ago, at
//! sniper speed, which is worse than seeing nothing. So coverage starts at
//! `head - lookback_blocks` — three seconds of chain, so a launch that is *still* live is
//! caught — and within a run the range is contiguous. When the loop falls so far behind
//! that catching up is meaningless it says so with a [`Gap`] rather than quietly stepping
//! over the blocks: a feed that lost coverage must not look like a feed where nothing
//! happened.
//!
//! # Reorgs
//!
//! Not handled, deliberately. This is an Arbitrum-stack chain whose sequencer gives
//! immediate soft finality, and a launch that was un-mined would fail at the buy anyway.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use banana_chain::addr;
use banana_chain::gate::{Priority, RpcError};
use banana_chain::launch_log;
use banana_chain::rpc::{Client, LogFilter};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// One decay window, in blocks, at the measured mean block time.
const DECAY_WINDOW_BLOCKS: u64 = 3_000 / addr::BLOCK_MS;

/// How the watcher behaves. The defaults are the measured ones; see the module docs.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Which factory to watch. A field rather than a constant so a test can point it
    /// somewhere else.
    pub factory: Address,
    /// Time between sweeps. Below the gate's `logs_spacing` this buys nothing.
    pub poll_interval: Duration,
    /// How far back the first sweep of a session reaches. One decay window: enough that a
    /// launch which is still snipeable is caught, and not one block more.
    pub lookback_blocks: u64,
    /// Ceiling on one `eth_getLogs` range, so catching up happens over several sweeps
    /// rather than in one request the endpoint may refuse.
    pub max_blocks_per_poll: u64,
    /// Beyond this backlog, coverage is declared lost rather than replayed.
    pub max_catchup_blocks: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            factory: addr::PONS_FACTORY,
            // Just over the gate's 400 ms logs spacing: close enough that the endpoint is
            // what paces us, far enough not to spin inside the gate.
            poll_interval: Duration::from_millis(450),
            lookback_blocks: DECAY_WINDOW_BLOCKS,
            max_blocks_per_poll: 5_000,
            // ~10 minutes. A backlog larger than this holds nothing tradeable, so scanning
            // it would spend the hot budget to produce stale rows.
            max_catchup_blocks: 6_000,
        }
    }
}

/// A launch, seen live.
///
/// Carries the head at the moment it was pulled off the wire, which makes its age knowable
/// downstream without a second call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub token: Address,
    pub curve: Address,
    pub deployer: Address,
    pub pair_token: Address,
    pub launch_config_id: u64,
    pub graduation_threshold: U256,
    pub block: u64,
    pub tx_hash: B256,
    pub log_index: u64,
    /// The chain head when this sighting was taken.
    pub head_at_sight: u64,
    /// This launch's place among the launches its transaction produced, in log order.
    ///
    /// A bundler can create several tokens in one call, and the calldata decoder needs to
    /// know which frame belongs to which. Computed here because a sweep covers whole
    /// blocks, so a transaction's launches are always in one sweep and never split across
    /// two.
    pub ordinal: usize,
    pub total: usize,
}

impl Launch {
    /// How far behind the head this launch already was when we saw it.
    pub fn behind_blocks(&self) -> u64 {
        self.head_at_sight.saturating_sub(self.block)
    }

    /// Age at the moment of sighting, in milliseconds.
    ///
    /// An **estimate**, and a lower bound on the real age: block distance times the
    /// measured mean block time, with nothing added for the trip home. Block timestamps
    /// would not do better — they have one-second granularity here, so ten blocks share a
    /// value.
    pub fn age_estimate_ms(&self) -> u64 {
        self.behind_blocks().saturating_mul(addr::BLOCK_MS)
    }

    /// Whether the opening tax window can still plausibly be open.
    ///
    /// What to do about a `false` is the caller's decision; this only answers.
    pub fn still_in_the_window(&self, decay_ms: u64) -> bool {
        self.age_estimate_ms() < decay_ms
    }

    /// Native ETH, as the factory encodes it.
    pub fn is_native_pair(&self) -> bool {
        self.pair_token.is_zero()
    }
}

/// Blocks that were never scanned, and the launches in them that were never seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub from: u64,
    pub to: u64,
}

impl Gap {
    pub fn blocks(&self) -> u64 {
        self.to.saturating_sub(self.from) + 1
    }

    /// Said plainly, because a user reading a quiet feed deserves to know it was quiet for
    /// a reason about us rather than about the chain.
    pub fn describe(&self) -> String {
        format!(
            "lost coverage of blocks {}-{} ({} blocks, about {}s). Launches in that range \
             were never seen",
            self.from,
            self.to,
            self.blocks(),
            self.blocks().saturating_mul(addr::BLOCK_MS) / 1_000
        )
    }
}

/// The result of one sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sweep {
    /// Head at the start of the sweep.
    pub head: u64,
    pub from: u64,
    pub to: u64,
    pub launches: Vec<Launch>,
    pub gap: Option<Gap>,
    pub elapsed_ms: u64,
}

impl Sweep {
    /// Blocks between what was scanned and the head. Nonzero means still catching up.
    pub fn behind(&self) -> u64 {
        self.head.saturating_sub(self.to)
    }
}

/// A pulse, so a quiet feed can be told apart from a broken one (spec §8 view 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub head: u64,
    pub scanned_to: u64,
    pub behind_blocks: u64,
    /// Wall time for the sweep: both calls plus whatever the gate made us wait.
    pub latency_ms: u64,
    pub launches: u32,
}

/// What the watcher tells the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Sighting {
    Launch(Launch),
    Health(Health),
    Gap(Gap),
    /// A sweep failed. Reported rather than swallowed: an endpoint that has started
    /// refusing looks exactly like an afternoon when nobody launched anything.
    Trouble {
        detail: String,
        consecutive: u32,
    },
}

/// Polls the factory for launches.
#[derive(Debug)]
pub struct Watcher {
    client: Client,
    config: WatchConfig,
    /// First block of the next sweep. `None` before the first one has run.
    next_from: Option<u64>,
}

impl Watcher {
    pub fn new(client: Client, config: WatchConfig) -> Self {
        debug_assert!(
            config.max_catchup_blocks > config.lookback_blocks,
            "a catch-up ceiling below the lookback would declare a gap on every restart"
        );
        Self {
            client,
            config,
            next_from: None,
        }
    }

    pub fn config(&self) -> &WatchConfig {
        &self.config
    }

    /// Where the next sweep will start, once one has run.
    pub fn next_from(&self) -> Option<u64> {
        self.next_from
    }

    /// One poll: find the head, scan what has not been scanned, decode.
    ///
    /// On failure `next_from` is left alone, so a retry re-covers the same blocks rather
    /// than stepping over them. Seeing a launch twice is harmless — the engine keys on the
    /// token — and missing one is not.
    pub async fn sweep(&mut self) -> Result<Sweep, RpcError> {
        let started = Instant::now();
        let head = self.client.block_number(Priority::Hot).await?;

        let mut from = self
            .next_from
            .unwrap_or_else(|| head.saturating_sub(self.config.lookback_blocks));

        // The head can sit still between sweeps — at 101 ms blocks and a 450 ms poll it
        // usually has not, but a stalled sequencer or a paused clock in a test will do it.
        if from > head {
            return Ok(Sweep {
                head,
                from,
                to: from.saturating_sub(1),
                launches: Vec::new(),
                gap: None,
                elapsed_ms: elapsed_ms(started),
            });
        }

        let mut gap = None;
        if head.saturating_sub(from) >= self.config.max_catchup_blocks {
            let resume = head.saturating_sub(self.config.lookback_blocks);
            gap = Some(Gap {
                from,
                to: resume.saturating_sub(1),
            });
            from = resume;
        }

        let to = head.min(from.saturating_add(self.config.max_blocks_per_poll - 1));

        let filter = LogFilter::new(from, to)
            .address(self.config.factory)
            .topics([launch_log::topic0()]);
        let logs = self.client.get_logs(&filter, Priority::Hot).await?;

        let mut launches: Vec<Launch> = logs
            .iter()
            .filter_map(launch_log::decode)
            .map(|d| Launch {
                token: d.token,
                curve: d.curve,
                deployer: d.deployer,
                pair_token: d.pair_token,
                launch_config_id: d.launch_config_id,
                graduation_threshold: d.graduation_threshold,
                block: d.block,
                tx_hash: d.tx_hash,
                log_index: d.log_index,
                head_at_sight: head,
                ordinal: 0,
                total: 1,
            })
            .collect();
        for (i, (ordinal, total)) in crate::enrich::positions(&launches).into_iter().enumerate() {
            launches[i].ordinal = ordinal;
            launches[i].total = total;
        }

        // Advance only after a successful decode pass.
        self.next_from = Some(to.saturating_add(1));

        Ok(Sweep {
            head,
            from,
            to,
            launches,
            gap,
            elapsed_ms: elapsed_ms(started),
        })
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// Poll until the receiver goes away.
///
/// Stopping is dropping the receiver: there is no flag to get wrong, and a consumer that
/// has gone means there is nobody to tell about a launch anyway.
///
/// While behind the head it polls again with no pause. That is not a busy loop — the gate
/// still spaces `eth_getLogs` — it just declines to add a second delay on top.
pub async fn run(mut watcher: Watcher, sink: mpsc::Sender<Sighting>) {
    let interval = watcher.config.poll_interval;
    let mut consecutive_failures = 0u32;

    loop {
        let started = Instant::now();
        let catching_up = match watcher.sweep().await {
            Ok(sweep) => {
                consecutive_failures = 0;
                if let Some(gap) = sweep.gap {
                    tracing::warn!(from = gap.from, to = gap.to, "{}", gap.describe());
                    if sink.send(Sighting::Gap(gap)).await.is_err() {
                        return;
                    }
                }
                let health = Health {
                    head: sweep.head,
                    scanned_to: sweep.to,
                    behind_blocks: sweep.behind(),
                    latency_ms: sweep.elapsed_ms,
                    launches: sweep.launches.len() as u32,
                };
                // Launches first: the pulse can wait, a launch cannot.
                for l in sweep.launches {
                    tracing::info!(
                        token = %l.token,
                        block = l.block,
                        age_ms = l.age_estimate_ms(),
                        "launch seen"
                    );
                    if sink.send(Sighting::Launch(l)).await.is_err() {
                        return;
                    }
                }
                if sink.send(Sighting::Health(health)).await.is_err() {
                    return;
                }
                health.behind_blocks > 0
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(error = %e, consecutive = consecutive_failures, "sweep failed");
                let trouble = Sighting::Trouble {
                    detail: e.to_string(),
                    consecutive: consecutive_failures,
                };
                if sink.send(trouble).await.is_err() {
                    return;
                }
                false
            }
        };

        if catching_up {
            continue;
        }
        let spent = started.elapsed();
        if spent < interval {
            tokio::time::sleep(interval - spent).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use async_trait::async_trait;
    use banana_chain::gate::{Endpoint, Gate, GateConfig};
    use banana_chain::transport::{HttpResponse, Transport, TransportError};
    use std::sync::Mutex;

    /// Answers by method from a script, and records what was asked.
    #[derive(Debug)]
    struct Scripted {
        heads: Mutex<Vec<u64>>,
        logs: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Transport for Scripted {
        async fn post(&self, _url: &str, body: &str) -> Result<HttpResponse, TransportError> {
            let reply = if body.contains("eth_blockNumber") {
                let mut h = self.heads.lock().unwrap();
                let v = if h.len() > 1 { h.remove(0) } else { h[0] };
                format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"0x{v:x}\"}}")
            } else if body.contains("eth_getLogs") {
                let mut l = self.logs.lock().unwrap();
                if l.is_empty() { no_logs() } else { l.remove(0) }
            } else {
                panic!("unscripted method in {body}");
            };
            Ok(HttpResponse {
                status: 200,
                body: reply,
            })
        }
    }

    fn no_logs() -> String {
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":[]}".to_owned()
    }

    /// A `TokenLaunched` in the shape the endpoint returns it.
    fn launch_log_json(block: u64, token: &str) -> String {
        let topic0 = launch_log::topic0().to_string();
        let pair = "0".repeat(64);
        let config_id = format!("{:064x}", 1u64);
        let threshold = format!("{:064x}", 4_000_000_000_000_000_000u64);
        let tx = "ab".repeat(32);
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":[{{\
             \"address\":\"0x7ed598bcef8bd9edd8c97a195c6d13f40801ec7e\",\
             \"topics\":[\"{topic0}\",\
             \"0x000000000000000000000000{token}\",\
             \"0x000000000000000000000000cccccccccccccccccccccccccccccccccccccccc\",\
             \"0x000000000000000000000000dddddddddddddddddddddddddddddddddddddddd\"],\
             \"data\":\"0x{pair}{config_id}{threshold}\",\
             \"blockNumber\":\"0x{block:x}\",\
             \"transactionHash\":\"0x{tx}\",\
             \"transactionIndex\":\"0x1\",\
             \"logIndex\":\"0x2\"}}]}}"
        )
    }

    fn watcher_with(heads: Vec<u64>, logs: Vec<String>, config: WatchConfig) -> Watcher {
        let scripted = Scripted {
            heads: Mutex::new(heads),
            logs: Mutex::new(logs),
        };
        let gate = Gate::new(
            Box::new(scripted),
            vec![Endpoint::new("http://test", "test", true)],
            GateConfig {
                spacing: Duration::ZERO,
                logs_spacing: Duration::ZERO,
                ..Default::default()
            },
        );
        Watcher::new(Client::new(gate), config)
    }

    /// The first sweep of a session starts one decay window back: not at genesis, and not
    /// from a checkpoint, because a launch older than its window is not a trade.
    #[tokio::test]
    async fn a_fresh_session_looks_back_exactly_one_decay_window() {
        let cfg = WatchConfig::default();
        let mut w = watcher_with(vec![900_000], vec![no_logs()], cfg.clone());
        let s = w.sweep().await.unwrap();
        assert_eq!(s.from, 900_000 - cfg.lookback_blocks);
        assert_eq!(s.to, 900_000);
        assert_eq!(w.next_from(), Some(900_001));
    }

    /// Contiguity within a run: the next sweep starts exactly where the last one stopped.
    #[tokio::test]
    async fn sweeps_are_contiguous() {
        let mut w = watcher_with(
            vec![900_000, 900_004],
            vec![no_logs(), no_logs()],
            WatchConfig::default(),
        );
        w.sweep().await.unwrap();
        let s = w.sweep().await.unwrap();
        assert_eq!(s.from, 900_001, "no block is scanned twice or skipped");
        assert_eq!(s.to, 900_004);
    }

    /// A head that has not moved must not scan a backwards range.
    #[tokio::test]
    async fn a_stalled_head_scans_nothing() {
        let mut w = watcher_with(
            vec![900_000, 900_000],
            vec![no_logs()],
            WatchConfig::default(),
        );
        w.sweep().await.unwrap();
        let s = w.sweep().await.unwrap();
        assert!(s.launches.is_empty());
        assert_eq!(
            w.next_from(),
            Some(900_001),
            "the checkpoint stands still too"
        );
    }

    #[tokio::test]
    async fn a_launch_decodes_with_the_head_that_saw_it() {
        let mut w = watcher_with(
            vec![900_010],
            vec![launch_log_json(
                900_005,
                "9d0d1d2b3c4d5e6f708192a3b4c5d6e7f8091a2b",
            )],
            WatchConfig::default(),
        );
        let s = w.sweep().await.unwrap();
        assert_eq!(s.launches.len(), 1);
        let l = &s.launches[0];
        assert_eq!(l.block, 900_005);
        assert_eq!(l.head_at_sight, 900_010);
        assert_eq!(l.behind_blocks(), 5);
        assert_eq!(l.age_estimate_ms(), 5 * addr::BLOCK_MS);
        assert!(l.is_native_pair());
        assert!(l.still_in_the_window(3_000));
    }

    /// The age estimate is what tells a stale sighting from a live one.
    #[test]
    fn a_launch_from_before_the_window_is_not_in_it() {
        let l = Launch {
            token: Address::repeat_byte(1),
            curve: Address::repeat_byte(2),
            deployer: Address::repeat_byte(3),
            pair_token: Address::ZERO,
            launch_config_id: 1,
            graduation_threshold: U256::ZERO,
            block: 900_000,
            tx_hash: B256::ZERO,
            log_index: 0,
            head_at_sight: 900_100,
            ordinal: 0,
            total: 1,
        };
        assert_eq!(l.age_estimate_ms(), 100 * addr::BLOCK_MS);
        assert!(!l.still_in_the_window(3_000));
    }

    /// A big backlog is declared lost, not replayed. The gap names the blocks, so the user
    /// sees what was missed instead of an unexplained quiet patch.
    #[tokio::test]
    async fn a_long_stall_reports_a_gap_rather_than_replaying_stale_launches() {
        let cfg = WatchConfig::default();
        let mut w = watcher_with(
            vec![900_000, 950_000],
            vec![no_logs(), no_logs()],
            cfg.clone(),
        );
        w.sweep().await.unwrap();
        let s = w.sweep().await.unwrap();
        let gap = s.gap.expect("coverage was lost and must be reported");
        assert_eq!(gap.from, 900_001);
        assert_eq!(gap.to, 950_000 - cfg.lookback_blocks - 1);
        assert_eq!(s.from, 950_000 - cfg.lookback_blocks);
        assert!(gap.describe().contains("never seen"));
    }

    /// A backlog inside the ceiling is caught up over several sweeps, none of them larger
    /// than the endpoint will answer.
    #[tokio::test]
    async fn catching_up_is_split_across_sweeps() {
        let cfg = WatchConfig {
            max_blocks_per_poll: 100,
            ..Default::default()
        };
        let mut w = watcher_with(
            vec![900_000, 900_500, 900_500],
            vec![no_logs(), no_logs(), no_logs()],
            cfg,
        );
        w.sweep().await.unwrap();
        let s = w.sweep().await.unwrap();
        assert_eq!(s.to - s.from + 1, 100, "one poll is capped");
        assert_eq!(
            s.behind(),
            900_500 - s.to,
            "and it says how far behind it still is"
        );
        let s2 = w.sweep().await.unwrap();
        assert_eq!(s2.from, s.to + 1);
    }

    /// A failed sweep must not step over the blocks it failed on.
    #[tokio::test]
    async fn a_failed_sweep_leaves_the_checkpoint_alone() {
        #[derive(Debug)]
        struct Broken;
        #[async_trait]
        impl Transport for Broken {
            async fn post(&self, _u: &str, _b: &str) -> Result<HttpResponse, TransportError> {
                Err(TransportError::Network("endpoint down".into()))
            }
        }
        let gate = Gate::new(
            Box::new(Broken),
            vec![Endpoint::new("http://test", "test", true)],
            GateConfig {
                spacing: Duration::ZERO,
                logs_spacing: Duration::ZERO,
                cooldown: Duration::ZERO,
                bench: Duration::ZERO,
                max_attempts: 1,
                ..Default::default()
            },
        );
        let mut w = Watcher::new(Client::new(gate), WatchConfig::default());
        assert!(w.sweep().await.is_err());
        assert_eq!(
            w.next_from(),
            None,
            "nothing was covered, so nothing advances"
        );
    }

    /// The loop stops when the consumer goes away, with no flag to get wrong.
    #[tokio::test]
    async fn the_loop_ends_when_the_receiver_is_dropped() {
        let w = watcher_with(
            vec![900_000],
            vec![launch_log_json(
                900_000,
                "9d0d1d2b3c4d5e6f708192a3b4c5d6e7f8091a2b",
            )],
            WatchConfig::default(),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(run(w, tx));
        let first = rx.recv().await.expect("a sighting");
        assert!(matches!(first, Sighting::Launch(_)), "launches come first");
        drop(rx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the loop must end when nobody is listening")
            .unwrap();
    }

    #[test]
    fn the_topic_filtered_on_is_the_one_the_factory_emits() {
        assert_eq!(
            hex::encode_prefixed(launch_log::topic0()),
            "0x8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607"
        );
    }
}
