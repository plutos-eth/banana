//! The engine: the whole pipeline, running.
//!
//! `detect -> enrich -> filter -> wait -> buy -> mark -> exit -> sell`, with the pieces
//! that were already built doing the deciding and this file doing the sequencing.
//!
//! # TEST runs all of it
//!
//! There is one path. TEST does not skip the buy, does not skip the simulation, and does
//! not skip the guards — it runs to `sign`, where the session's [`crate::NoSigner`] has no
//! key and returns an error. A rehearsal that skipped steps would rehearse the wrong thing,
//! and the reason it is safe to run every step is structural rather than conditional.
//!
//! # Where the concurrency is
//!
//! One task owns the journal, the store handle and the session, because two of those are
//! not `Sync` and the third holds the money. Around it:
//!
//! * **Enrichment** is spawned, capped, and touches only the network.
//! * **Waiting for the opening tax to decay** is spawned: it takes up to three seconds and
//!   the loop must keep detecting while it runs.
//! * **Buying, selling and marking** happen in the loop itself. One transaction in flight
//!   at a time, which is the simple correct thing and what the nonce handling assumes.
//!
//! # Every refusal is recorded
//!
//! A launch that does not become a position writes a row saying which rule stopped it and
//! what the values were. "Nothing fired all afternoon" is a question the program has to be
//! able to answer, and the journal is the only place the answer can come from.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use banana_chain::abi::Phase;
use banana_chain::gate::Priority;
use banana_chain::rpc::Client;
use banana_core::features::Pair;
use banana_core::strategy::StrategyConfig;
use banana_core::{BPS, Bps, curve};
use banana_store::types::Side;
use banana_store::{History, Journal, journal};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::enrich::{self, ChainState, Reading};
use crate::entry::{Achieved, Observation, Schedule, Step, TaxReport};
use crate::exec::{self, Order};
use crate::exits::{self, Exit, Mark};
use crate::guards::Refused;
use crate::route::Route;
use crate::seen::{self, Coverage, Seen};
use crate::session::{Mode, Session};
use crate::watch::{self, Launch, Sighting, WatchConfig};

/// How the engine behaves.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub watch: WatchConfig,
    /// Simultaneous enrichments. Spec §7 suggests about three; launches arrive at roughly
    /// one every four seconds, so this is headroom for a burst rather than a throughput
    /// setting.
    pub max_enrichments: usize,
    /// How often every open position is re-priced.
    pub mark_interval: Duration,
    /// How far past the decay window a launch is still worth reading.
    ///
    /// Not zero: a launch seen at 3,100 ms is still worth knowing about for the feed and
    /// for the deployer counts, even though its tax window has shut.
    pub stale_grace_ms: u64,
    /// The address orders are priced and simulated against.
    ///
    /// LIVE always uses the session's wallet. TEST uses whatever key the user has saved,
    /// so a rehearsal is priced against a real balance — and still cannot sign.
    pub wallet: Option<Address>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            watch: WatchConfig::default(),
            max_enrichments: 3,
            mark_interval: Duration::from_secs(5),
            stale_grace_ms: 2_000,
            wallet: None,
        }
    }
}

/// What the engine tells the application.
///
/// Money crosses as decimal strings: `U256` does not fit a JavaScript number, and rounding
/// a balance to 53 bits of mantissa on the way to a screen is exactly the kind of quiet
/// wrongness §12 forbids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Started {
        mode: Mode,
        wallet: Option<String>,
        coverage: String,
        /// Anything the user should know before trusting what follows.
        notes: Vec<String>,
    },
    Health {
        head: u64,
        behind_blocks: u64,
        latency_ms: u64,
        watched: usize,
    },
    Gap {
        detail: String,
    },
    Trouble {
        detail: String,
        consecutive: u32,
    },
    /// A launch entered the pipeline.
    Seen {
        token: String,
        block: u64,
        age_ms: u64,
    },
    Refused {
        token: String,
        symbol: String,
        rule: String,
        detail: String,
    },
    /// Passed the filter; waiting for the opening tax to come down.
    Waiting {
        token: String,
        symbol: String,
        tax_bps: Bps,
    },
    Entered {
        token: String,
        symbol: String,
        quote_wei: String,
        tokens: String,
        tax_bps: Bps,
        tax_paid_bps: Option<Bps>,
        tx_hash: Option<String>,
        simulated: bool,
        detail: String,
    },
    Marked {
        token: String,
        symbol: String,
        mult_bps: u64,
        peak_bps: u64,
        remaining_bps: u32,
    },
    Exited {
        token: String,
        symbol: String,
        rule: String,
        detail: String,
        sell_bps: u32,
        quote_wei: String,
        tx_hash: Option<String>,
        simulated: bool,
        closed: bool,
    },
    /// Something went wrong on a specific token, which is not the same as the feed
    /// breaking.
    Failed {
        token: String,
        symbol: String,
        detail: String,
    },
    Stopped {
        detail: String,
        /// The achieved-tax KPI for the session (PLAN.md F7).
        tax_summary: String,
    },
}

/// A position the engine is holding.
#[derive(Debug, Clone)]
struct Held {
    journal_id: i64,
    launch: Launch,
    symbol: String,
    cost_wei: U256,
    tokens_bought: U256,
    tokens_held: U256,
    peak_bps: u64,
    opened: tokio::time::Instant,
    /// Cumulative fraction already sold, in bps of the original.
    sold_bps: u32,
}

impl Held {
    fn remaining_bps(&self) -> u32 {
        BPS.saturating_sub(self.sold_bps)
    }

    /// What the money still at risk cost.
    fn cost_of_remaining(&self) -> U256 {
        self.cost_wei
            .saturating_mul(U256::from(self.remaining_bps()))
            / U256::from(BPS)
    }
}

/// What a candidate carries from enrichment to the buy.
///
/// Deliberately not the state it was enriched with. The curve moves for the whole of the
/// decay window, and an order sized against a price from three seconds ago is an order
/// that reverts on slippage — so the buy re-reads.
#[derive(Debug, Clone)]
struct Candidate {
    launch: Launch,
    symbol: String,
}

/// Everything the engine owns for one run.
pub struct Engine {
    client: Client,
    session: Session,
    strategy: StrategyConfig,
    journal: Journal,
    history: Option<History>,
    seen: Seen,
    config: EngineConfig,
    session_id: i64,
    held: HashMap<Address, Held>,
    tax: TaxReport,
    events: mpsc::Sender<Event>,
}

impl Engine {
    /// Set a session up: bridge the store's edge to the chain head, open the journal.
    ///
    /// The bridge is what stops a stale index quietly understating every deployer's
    /// history. When it cannot be done, the reason is reported and the depth restriction
    /// does the refusing (see [`crate::seen`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        client: Client,
        session: Session,
        strategy: StrategyConfig,
        mut journal: Journal,
        history: Option<History>,
        config: EngineConfig,
        events: mpsc::Sender<Event>,
    ) -> Result<Self, EngineError> {
        let head = client.block_number(Priority::Hot).await?;
        let watched_from = head.saturating_sub(config.watch.lookback_blocks);
        let store_window = history
            .as_ref()
            .and_then(|h| h.window().ok().flatten())
            .map(|w| (w.from_block, w.to_block));

        let mut notes = Vec::new();
        let mut coverage = Coverage {
            store: store_window,
            watched_from,
            hole: store_window
                .map(|(_, to)| watched_from.saturating_sub(to.saturating_add(1)))
                .unwrap_or(0),
        };
        if store_window.is_none() {
            notes.push(
                "No index in this data directory, so deployer history counts only what \
                 this session watches. Any rule reading a deployer feature will refuse \
                 until enough has been watched."
                    .into(),
            );
        }

        let mut seen_state = Seen::new(coverage);
        if let Some((_, to)) = store_window
            && coverage.hole > 0
        {
            match seen::bridge(
                &client,
                to.saturating_add(1),
                watched_from.saturating_sub(1),
                config.watch.max_blocks_per_poll,
            )
            .await
            {
                Ok(bridged) => {
                    let n = bridged.launches.len();
                    let blocks = bridged.to.saturating_sub(bridged.from) + 1;
                    seen_state.absorb(bridged, history.as_ref());
                    coverage = seen_state.coverage();
                    notes.push(format!(
                        "Bridged the {blocks} blocks between where the index stops and \
                         where this session starts watching, picking up {n} launches, so \
                         deployer history is continuous."
                    ));
                }
                Err(e) => notes.push(e.to_string()),
            }
        }

        let wallet = config
            .wallet
            .or_else(|| (session.mode() == Mode::Live).then_some(session.address()));
        if session.mode() == Mode::Test && wallet.is_none() {
            notes.push(
                "No wallet configured, so TEST prices orders against a notional balance \
                 and simulations that need funds will be refused by the chain. Save a key \
                 in Settings to rehearse against a real balance."
                    .into(),
            );
        }

        let limits = session.budget().limits().clone();
        let session_id = journal.begin_session(&journal::NewSession {
            mode: match session.mode() {
                Mode::Test => "test".into(),
                Mode::Live => "live".into(),
            },
            wallet: wallet.unwrap_or(Address::ZERO),
            started_at: now(),
            session_budget_wei: limits.session_budget_wei,
            size_per_buy_wei: limits.size_per_buy_wei,
            position_cap_wei: limits.position_cap_wei,
            max_open_positions: limits.max_open_positions,
            strategy_json: serde_json::to_string(&strategy).unwrap_or_default(),
        })?;

        let _ = events
            .send(Event::Started {
                mode: session.mode(),
                wallet: wallet.map(|a| format!("{a:#x}")),
                coverage: coverage.describe(),
                notes,
            })
            .await;

        let mut config = config;
        config.wallet = wallet;
        Ok(Self {
            client,
            session,
            strategy,
            journal,
            history,
            seen: seen_state,
            config,
            session_id,
            held: HashMap::new(),
            tax: TaxReport::default(),
            events,
        })
    }

    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    /// Run until `stop` fires or the sink goes away.
    pub async fn run(mut self, mut stop: mpsc::Receiver<()>) {
        let (tx, mut sightings) = mpsc::channel(256);
        let watcher = watch::Watcher::new(self.client.clone(), self.config.watch.clone());
        let watch_task = tokio::spawn(watch::run(watcher, tx));

        let permits = Arc::new(Semaphore::new(self.config.max_enrichments));
        let mut enriching: JoinSet<(Launch, Result<Reading, String>)> = JoinSet::new();
        let mut waiting: JoinSet<(Candidate, Step)> = JoinSet::new();
        let mut ticker = tokio::time::interval(self.config.mark_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let reason = loop {
            tokio::select! {
                _ = stop.recv() => break "stopped by the user".to_owned(),
                sighting = sightings.recv() => match sighting {
                    Some(s) => self.on_sighting(s, &permits, &mut enriching).await,
                    None => break "the watcher stopped".to_owned(),
                },
                Some(done) = enriching.join_next(), if !enriching.is_empty() => {
                    if let Ok((launch, result)) = done {
                        self.on_enriched(launch, result, &mut waiting).await;
                    }
                }
                Some(done) = waiting.join_next(), if !waiting.is_empty() => {
                    if let Ok((candidate, step)) = done {
                        self.on_decayed(candidate, step).await;
                    }
                }
                _ = ticker.tick() => self.mark_all().await,
            }
        };

        watch_task.abort();
        let _ = self.journal.end_session(self.session_id, now());
        let _ = self
            .events
            .send(Event::Stopped {
                detail: reason,
                tax_summary: self.tax.summary(),
            })
            .await;
    }

    // --- detect -----------------------------------------------------------------------

    async fn on_sighting(
        &mut self,
        s: Sighting,
        permits: &Arc<Semaphore>,
        enriching: &mut JoinSet<(Launch, Result<Reading, String>)>,
    ) {
        match s {
            Sighting::Health(h) => {
                self.emit(Event::Health {
                    head: h.head,
                    behind_blocks: h.behind_blocks,
                    latency_ms: h.latency_ms,
                    watched: self.seen.watched_launches(),
                })
                .await;
            }
            Sighting::Gap(g) => {
                self.emit(Event::Gap {
                    detail: g.describe(),
                })
                .await
            }
            Sighting::Trouble {
                detail,
                consecutive,
            } => {
                self.emit(Event::Trouble {
                    detail,
                    consecutive,
                })
                .await
            }
            Sighting::Launch(l) => {
                // Counted before anything else: a launch the filter refuses is still a
                // launch this deployer made, and the deployer counts must include it.
                //
                // The answer also gates everything below. A sweep that fails leaves its
                // checkpoint alone by design, so the retry re-reads the same blocks and
                // the same launches arrive again; without this the token would be enriched
                // twice, refused twice in the journal, and — if it passed — authorised
                // twice.
                if !self.seen.record_launch(l.token, l.deployer, l.block, None) {
                    return;
                }
                self.emit(Event::Seen {
                    token: format!("{:#x}", l.token),
                    block: l.block,
                    age_ms: l.age_estimate_ms(),
                })
                .await;

                if !enrich::worth_enriching(&l, self.decay_ms(), self.config.stale_grace_ms) {
                    self.refuse(
                        &l,
                        "",
                        "stale",
                        format!(
                            "seen {} ms after its block, past the {} ms opening-tax window \
                             plus {} ms of grace. Nothing to snipe here any more",
                            l.age_estimate_ms(),
                            self.decay_ms(),
                            self.config.stale_grace_ms
                        ),
                    )
                    .await;
                    return;
                }

                let client = self.client.clone();
                let permits = permits.clone();
                enriching.spawn(async move {
                    // Held for the read, so at most `max_enrichments` are in flight. A
                    // closed semaphore cannot happen: the engine owns it for its lifetime.
                    let _permit = permits.acquire_owned().await;
                    let r = enrich::read(&client, &l, l.ordinal, l.total)
                        .await
                        .map_err(|e| e.to_string());
                    (l, r)
                });
            }
        }
    }

    // --- enrich and filter --------------------------------------------------------------

    async fn on_enriched(
        &mut self,
        launch: Launch,
        result: Result<Reading, String>,
        waiting: &mut JoinSet<(Candidate, Step)>,
    ) {
        let reading = match result {
            Ok(r) => r,
            Err(detail) => {
                self.refuse(&launch, "", "unreadable", detail).await;
                return;
            }
        };
        let symbol = display_symbol(&reading);

        // Record the template now that it is known, so later launches can be compared
        // against it even if this one is refused. Already counted, so the answer is
        // `false` here and means nothing.
        let _ = self.seen.record_launch(
            launch.token,
            launch.deployer,
            launch.block,
            Some(reading.fingerprint()),
        );

        for gap in &reading.gaps {
            tracing::debug!(token = %launch.token, "{gap}");
        }

        // --- what this session is able to trade at all ---------------------------------
        if !matches!(reading.pair, Pair::Eth) {
            self.refuse(
                &launch,
                &symbol,
                "pair_unsupported",
                format!(
                    "pairs against {}, and this build only trades ETH-paired launches: a \
                     non-ETH quote needs an allowance the program does not manage. Add a \
                     pair rule to keep these out of the feed",
                    reading.pair.label()
                ),
            )
            .await;
            return;
        }

        // --- what this session is able to answer honestly ------------------------------
        let depth = self.seen.coverage().depth_at(launch.block);
        if self.strategy.entry_filter.uses_deployer_history()
            && depth < banana_backtest_depth_floor()
        {
            self.refuse(
                &launch,
                &symbol,
                "deployer_depth",
                format!(
                    "this strategy reads a deployer feature, and only {} blocks of \
                     deployer history are continuous here against the {} the Lab requires. \
                     Passing on a count we know is short would be the flattering direction",
                    depth,
                    banana_backtest_depth_floor()
                ),
            )
            .await;
            return;
        }
        let uses_twins = self
            .strategy
            .entry_filter
            .conditions()
            .iter()
            .any(|c| c.rule_id() == "fingerprint_twins");
        if uses_twins && !self.seen.twin_window_covered(launch.block) {
            self.refuse(
                &launch,
                &symbol,
                "twin_coverage",
                format!(
                    "counting fingerprint twins needs 30 minutes of watched launches and \
                     this session has {} blocks of them. The count would be an undercount, \
                     which would make the rule more permissive rather than less",
                    self.seen.twin_coverage_blocks(launch.block)
                ),
            )
            .await;
            return;
        }

        // --- the rule engine, the same one the Lab uses --------------------------------
        let deployer = self.seen.deployer(launch.deployer, self.history.as_ref());
        let twins = self
            .seen
            .twins_30m(&reading.fingerprint(), launch.deployer, launch.block);
        let state = reading.state.clone();
        let features = reading.into_features(deployer, twins, depth);
        let decision = self.strategy.entry_filter.evaluate(&features);
        if !decision.passed {
            let rule = decision
                .refusals
                .first()
                .map(|r| r.rule.clone())
                .unwrap_or_else(|| "filter".into());
            let detail = decision
                .refusals
                .iter()
                .map(|r| r.detail.clone())
                .collect::<Vec<_>>()
                .join("; ");
            self.refuse(&launch, &symbol, &rule, detail).await;
            return;
        }

        // --- the venue, read and never guessed -----------------------------------------
        match state.route(launch.token, launch.curve, launch.pair_token) {
            Route::Curve { .. } => {}
            Route::Pool { .. } => {
                self.refuse(
                    &launch,
                    &symbol,
                    "already_graduated",
                    "this token has already left its curve, and the opening-tax edge is \
                     gone with it"
                        .into(),
                )
                .await;
                return;
            }
            Route::Refuse { reason } => {
                self.refuse(&launch, &symbol, "no_venue", reason).await;
                return;
            }
        }
        if !state.priced {
            self.refuse(
                &launch,
                &symbol,
                "unpriceable",
                "the curve's reserves did not read, so no order can be sized against it".into(),
            )
            .await;
            return;
        }

        self.emit(Event::Waiting {
            token: format!("{:#x}", launch.token),
            symbol: symbol.clone(),
            tax_bps: state.snipe_tax_bps,
        })
        .await;

        // --- wait for the opening tax to come down --------------------------------------
        let client = self.client.clone();
        let candidate = Candidate {
            launch: launch.clone(),
            symbol,
        };
        let ceiling = self.strategy.entry_model.max_tax_bps;
        let max_wait = self.strategy.entry_model.max_wait_ms;
        let already = launch.age_estimate_ms();
        waiting.spawn(async move {
            let step =
                wait_for_decay(&client, candidate.launch.curve, ceiling, max_wait, already).await;
            (candidate, step)
        });
    }

    // --- buy ----------------------------------------------------------------------------

    async fn on_decayed(&mut self, candidate: Candidate, step: Step) {
        // The state read at enrichment is deliberately not carried through: the curve has
        // moved for the whole of the decay window, and sizing an order against a price
        // from three seconds ago is how a buy reverts on slippage.
        let Candidate { launch, symbol, .. } = candidate;
        let tax_bps = match step {
            Step::Buy { tax_bps } => tax_bps,
            Step::GiveUp {
                last_bps,
                waited_ms,
            } => {
                self.refuse(
                    &launch,
                    &symbol,
                    "tax_never_fell",
                    format!(
                        "waited {waited_ms} ms and the opening tax was still {last_bps} bps, \
                         above the {} bps this strategy will pay",
                        self.strategy.entry_model.max_tax_bps
                    ),
                )
                .await;
                return;
            }
            // The scheduler only returns Wait to its own loop.
            Step::Wait { .. } => return,
        };

        let state = match enrich::refresh(&self.client, &launch).await {
            Ok(fresh) => fresh,
            Err(e) => {
                self.fail(
                    &launch,
                    &symbol,
                    format!("could not re-read the curve: {e}"),
                )
                .await;
                return;
            }
        };
        if state.phase != Some(Phase::Curve) || !state.priced {
            self.refuse(
                &launch,
                &symbol,
                "moved",
                "the curve changed phase or stopped pricing while the tax was decaying".into(),
            )
            .await;
            return;
        }

        let size = self.strategy.entry_model.size_wei;
        let balance = self.balance_for_guards(size).await;
        let spend = match self.session.budget().authorise(launch.token, size, balance) {
            Ok(s) => s,
            Err(refused) => {
                self.refuse(&launch, &symbol, guard_rule(&refused), refused.to_string())
                    .await;
                return;
            }
        };

        // Slippage bounds the rate, from the quote the curve itself implies.
        let cs = state.curve_state();
        let expected = match curve::quote_buy(&cs, size) {
            Ok(q) => q.tokens_out,
            Err(e) => {
                self.fail(&launch, &symbol, format!("cannot price a buy: {e}"))
                    .await;
                return;
            }
        };
        let order = Order {
            route: Route::Curve {
                curve: launch.curve,
            },
            side: Side::Buy,
            quote_wei: size,
            tokens_in: U256::ZERO,
            min_out: curve::min_out_from_rate(expected, self.strategy.entry_model.slippage_bps),
            recipient: self.recipient(),
            from: self.recipient(),
        };

        let filled = match exec::buy(&self.client, &mut self.session, spend, &order, tax_bps).await
        {
            Ok(f) => f,
            Err(e) => {
                self.fail(&launch, &symbol, e.to_string()).await;
                return;
            }
        };

        // TEST commits the spend too, so the guards' accounting is the accounting a live
        // run would have had. A rehearsal that never consumed its budget would report a
        // strategy as fitting inside limits it would in fact have blown through.
        if filled.simulated {
            self.session.commit_simulated(
                match self.session.budget().authorise(launch.token, size, balance) {
                    Ok(s) => s,
                    Err(_) => return,
                },
            );
        }

        let achieved = Achieved {
            quote_in: filled.quote_wei,
            snipe_tax: filled.snipe_tax_wei.unwrap_or(U256::ZERO),
            decided_at_bps: tax_bps,
        };
        self.tax.record(achieved);

        let journal_id = match self.journal.open_position(&journal::NewPosition {
            session_id: self.session_id,
            token: launch.token,
            curve: launch.curve,
            symbol: symbol.clone(),
            pair: "ETH".into(),
            opened_at: now(),
            launch_block: launch.block,
        }) {
            Ok(id) => id,
            Err(e) => {
                self.fail(&launch, &symbol, format!("journal: {e}")).await;
                return;
            }
        };
        let _ = self.journal.record_fill(&journal::Fill {
            position_id: journal_id,
            at: now(),
            side: Side::Buy,
            quote_wei: filled.quote_wei,
            tokens: filled.tokens,
            tx_hash: filled.tx_hash,
            venue: filled.venue.clone(),
            rule: "entry".into(),
            detail: filled.detail.clone(),
            snipe_tax_wei: filled.snipe_tax_wei,
            tax_bps_at_decision: Some(tax_bps),
            gas_wei: filled.gas_wei,
        });

        self.held.insert(
            launch.token,
            Held {
                journal_id,
                launch: launch.clone(),
                symbol: symbol.clone(),
                cost_wei: filled.quote_wei,
                tokens_bought: filled.tokens,
                tokens_held: filled.tokens,
                peak_bps: BPS as u64,
                opened: tokio::time::Instant::now(),
                sold_bps: 0,
            },
        );

        self.emit(Event::Entered {
            token: format!("{:#x}", launch.token),
            symbol,
            quote_wei: filled.quote_wei.to_string(),
            tokens: filled.tokens.to_string(),
            tax_bps,
            tax_paid_bps: achieved.paid_bps(),
            tx_hash: filled.tx_hash.map(|h| format!("{h:#x}")),
            simulated: filled.simulated,
            detail: filled.detail,
        })
        .await;
    }

    // --- mark and exit ------------------------------------------------------------------

    async fn mark_all(&mut self) {
        let tokens: Vec<Address> = self.held.keys().copied().collect();
        for token in tokens {
            let Some(h) = self.held.get(&token).cloned() else {
                continue;
            };
            let state = match enrich::refresh(&self.client, &h.launch).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%token, error = %e, "mark failed");
                    continue;
                }
            };
            let Some(mark) = self.mark_of(&h, &state) else {
                continue;
            };

            if let Some(held) = self.held.get_mut(&token) {
                held.peak_bps = mark.peak_bps;
            }
            let _ = self.journal.mark(h.journal_id, mark.mult_bps);
            self.emit(Event::Marked {
                token: format!("{token:#x}"),
                symbol: h.symbol.clone(),
                mult_bps: mark.mult_bps,
                peak_bps: mark.peak_bps,
                remaining_bps: mark.remaining_bps,
            })
            .await;

            if let Exit::Sell {
                sell_bps,
                rule,
                detail,
            } = exits::decide(&self.strategy.exits, mark)
            {
                self.exit(token, sell_bps, rule, detail, &state).await;
            }
        }
    }

    /// Price a position by what selling it would actually fetch.
    ///
    /// A real sell quote for what is **still held**, not a mid-price and not the entry
    /// price marked up. That is why a fresh entry marks below par: the round trip costs
    /// the fee, the creator tax and the curve's own slope, and pretending otherwise would
    /// show a profit that does not exist (spec §1).
    fn mark_of(&self, h: &Held, state: &ChainState) -> Option<Mark> {
        if !state.priced || h.tokens_held.is_zero() {
            return None;
        }
        let out = curve::quote_sell(&state.curve_state(), h.tokens_held).ok()?;
        let cost = h.cost_of_remaining();
        if cost.is_zero() {
            return None;
        }
        let mult = out.saturating_mul(U256::from(BPS)) / cost;
        let mult_bps: u64 = mult.try_into().unwrap_or(u64::MAX);
        Some(Mark {
            mult_bps,
            peak_bps: h.peak_bps.max(mult_bps),
            held_secs: h.opened.elapsed().as_secs(),
            remaining_bps: h.remaining_bps(),
        })
    }

    async fn exit(
        &mut self,
        token: Address,
        sell_bps: u32,
        rule: String,
        detail: String,
        state: &ChainState,
    ) {
        let Some(h) = self.held.get(&token).cloned() else {
            return;
        };
        // Of the ORIGINAL position, which is what the rules count in.
        let tokens_in = h.tokens_bought.saturating_mul(U256::from(sell_bps)) / U256::from(BPS);
        let tokens_in = tokens_in.min(h.tokens_held);
        if tokens_in.is_zero() {
            return;
        }

        let route = state.route(token, h.launch.curve, h.launch.pair_token);
        let expected = curve::quote_sell(&state.curve_state(), tokens_in).unwrap_or(U256::ZERO);
        let order = Order {
            route,
            side: Side::Sell,
            quote_wei: U256::ZERO,
            tokens_in,
            min_out: curve::min_out_from_rate(expected, self.strategy.entry_model.slippage_bps),
            recipient: self.recipient(),
            from: self.recipient(),
        };

        // The curve moves tokens with `transferFrom`, so it needs an allowance. Read
        // first: one call to confirm beats an approval per sale.
        if self.session.mode() == Mode::Live
            && let Err(e) = exec::ensure_allowance(
                &self.client,
                &mut self.session,
                token,
                h.launch.curve,
                tokens_in,
            )
            .await
        {
            self.fail(&h.launch, &h.symbol, format!("approval failed: {e}"))
                .await;
            return;
        }

        let filled = match exec::sell(&self.client, &mut self.session, &order).await {
            Ok(f) => f,
            Err(e) => {
                self.fail(&h.launch, &h.symbol, e.to_string()).await;
                return;
            }
        };

        let _ = self.journal.record_fill(&journal::Fill {
            position_id: h.journal_id,
            at: now(),
            side: Side::Sell,
            quote_wei: filled.quote_wei,
            tokens: filled.tokens.max(tokens_in),
            tx_hash: filled.tx_hash,
            venue: filled.venue.clone(),
            rule: rule.clone(),
            detail: detail.clone(),
            snipe_tax_wei: None,
            tax_bps_at_decision: None,
            gas_wei: filled.gas_wei,
        });

        let closed = {
            let held = self.held.get_mut(&token).expect("checked above");
            held.sold_bps = held.sold_bps.saturating_add(sell_bps).min(BPS);
            held.tokens_held = held.tokens_held.saturating_sub(tokens_in);
            held.sold_bps >= BPS || held.tokens_held.is_zero()
        };
        if closed {
            let _ = self
                .journal
                .close_position(h.journal_id, now(), &format!("{rule}: {detail}"));
            self.held.remove(&token);
            self.session.close(token);
        }

        self.emit(Event::Exited {
            token: format!("{token:#x}"),
            symbol: h.symbol,
            rule,
            detail,
            sell_bps,
            quote_wei: filled.quote_wei.to_string(),
            tx_hash: filled.tx_hash.map(|hash| format!("{hash:#x}")),
            simulated: filled.simulated,
            closed,
        })
        .await;
    }

    // --- helpers -------------------------------------------------------------------------

    fn decay_ms(&self) -> u64 {
        // The opening window, as measured. Not a strategy setting: it is a property of the
        // contract, and `max_wait_ms` is how long *we* are prepared to wait inside it.
        3_000
    }

    fn recipient(&self) -> Address {
        self.config.wallet.unwrap_or(banana_chain::addr::DEAD)
    }

    /// The balance the guards check against.
    ///
    /// A real read when there is a wallet. Without one — TEST, no key saved — the guards
    /// are given the size they are being asked about, so the other four still apply and
    /// only the balance check is stood down. That is stated in the startup notes rather
    /// than hidden.
    /// `&mut self` for the same reason [`Self::emit`] takes it: a future holding a shared
    /// reference to an engine that owns SQLite connections is not `Send`.
    async fn balance_for_guards(&mut self, size: U256) -> U256 {
        match self.config.wallet {
            Some(w) => self
                .client
                .balance(w, Priority::Hot)
                .await
                .unwrap_or(U256::ZERO),
            None => size,
        }
    }

    /// Takes `&mut self` rather than `&self`, and that is load-bearing: the engine owns
    /// two SQLite connections, which are `Send` but not `Sync`. A future holding a shared
    /// reference across an await would not be `Send` and could not be spawned; a future
    /// holding an exclusive one is fine. Nothing here needs shared access anyway.
    async fn emit(&mut self, e: Event) {
        let _ = self.events.send(e).await;
    }

    async fn refuse(&mut self, l: &Launch, symbol: &str, rule: &str, detail: String) {
        let _ = self.journal.record_refusal(&journal::Refusal {
            session_id: self.session_id,
            at: now(),
            token: l.token,
            symbol: symbol.to_owned(),
            block: l.block,
            rule: rule.to_owned(),
            detail: detail.clone(),
        });
        self.emit(Event::Refused {
            token: format!("{:#x}", l.token),
            symbol: symbol.to_owned(),
            rule: rule.to_owned(),
            detail,
        })
        .await;
    }

    async fn fail(&mut self, l: &Launch, symbol: &str, detail: String) {
        tracing::warn!(token = %l.token, "{detail}");
        self.emit(Event::Failed {
            token: format!("{:#x}", l.token),
            symbol: symbol.to_owned(),
            detail,
        })
        .await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Rpc(#[from] banana_chain::gate::RpcError),
    #[error(transparent)]
    Store(#[from] banana_store::StoreError),
}

/// Poll the curve's own `currentSnipeTaxBps` until it is low enough to buy at.
///
/// The schedule is [`crate::entry::Schedule`], which projects the crossing from the rate
/// the tax has actually been falling rather than from an assumed curve — so a contract
/// whose decay is not what the documentation says still gets a well-timed buy.
async fn wait_for_decay(
    client: &Client,
    curve: Address,
    ceiling_bps: Bps,
    max_wait_ms: u64,
    already_waited_ms: u64,
) -> Step {
    let started = tokio::time::Instant::now();
    let mut schedule = Schedule::new();
    loop {
        let at_ms = already_waited_ms + started.elapsed().as_millis() as u64;
        let tax_bps = match client
            .call(
                curve,
                &banana_chain::abi::IPonsCurve::currentSnipeTaxBpsCall {
                    recipient: banana_chain::addr::DEAD,
                },
                Priority::Hot,
            )
            .await
        {
            Ok(v) => v.saturating_to::<u32>(),
            // A failed read is not a zero tax. Treating it as one would buy at the top of
            // the decay; treating it as the ceiling plus one keeps waiting.
            Err(e) => {
                tracing::warn!(%curve, error = %e, "tax poll failed");
                ceiling_bps.saturating_add(1)
            }
        };
        match schedule.step(Observation { at_ms, tax_bps }, ceiling_bps, max_wait_ms) {
            Step::Wait { for_ } => tokio::time::sleep(for_).await,
            other => return other,
        }
    }
}

/// The Lab's deployer-depth floor, so the sniper applies the same restriction the backtest
/// did.
///
/// Duplicated as a constant rather than depending on `banana-backtest`: the sniper must
/// not pull in the Lab, and 12 hours in blocks is a number, not a behaviour.
fn banana_backtest_depth_floor() -> u64 {
    12 * 3_600 * 1_000 / banana_chain::addr::BLOCK_MS
}

fn guard_rule(r: &Refused) -> &'static str {
    match r {
        Refused::SizePerBuy { .. } => "size_per_buy",
        Refused::PositionCap { .. } => "position_cap",
        Refused::SessionBudget { .. } => "session_budget",
        Refused::MaxOpenPositions { .. } => "max_open_positions",
        Refused::InsufficientBalance { .. } => "insufficient_balance",
    }
}

fn display_symbol(r: &Reading) -> String {
    if r.symbol.trim().is_empty() {
        "?".to_owned()
    } else {
        r.symbol.clone()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use banana_core::strategy::ExitPolicy;

    fn held(cost: u64, bought: u64, held_tokens: u64, sold_bps: u32) -> Held {
        Held {
            journal_id: 1,
            launch: Launch {
                token: Address::repeat_byte(1),
                curve: Address::repeat_byte(2),
                deployer: Address::repeat_byte(3),
                pair_token: Address::ZERO,
                launch_config_id: 0,
                graduation_threshold: U256::from(4_000_000_000_000_000_000u64),
                block: 1,
                tx_hash: B256::ZERO,
                log_index: 0,
                head_at_sight: 1,
                ordinal: 0,
                total: 1,
            },
            symbol: "TKN".into(),
            cost_wei: U256::from(cost),
            tokens_bought: U256::from(bought),
            tokens_held: U256::from(held_tokens),
            peak_bps: BPS as u64,
            opened: tokio::time::Instant::now(),
            sold_bps,
        }
    }

    /// A partial sale must not make the rest look free. The multiple is against the cost
    /// of what is still at risk, so selling half at 2x leaves the remainder marked on its
    /// own half of the cost.
    #[test]
    fn the_cost_of_the_remainder_scales_with_what_is_left() {
        let h = held(1_000, 10_000, 5_000, 5_000);
        assert_eq!(h.remaining_bps(), 5_000);
        assert_eq!(h.cost_of_remaining(), U256::from(500u64));
    }

    #[test]
    fn an_untouched_position_costs_all_of_what_went_in() {
        let h = held(1_000, 10_000, 10_000, 0);
        assert_eq!(h.remaining_bps(), 10_000);
        assert_eq!(h.cost_of_remaining(), U256::from(1_000u64));
    }

    /// The depth floor the sniper applies is the same 12 hours the Lab restricts by, or a
    /// backtest would be a promise about a different universe from the one that trades.
    #[test]
    fn the_depth_floor_matches_the_labs() {
        assert_eq!(
            banana_backtest_depth_floor(),
            banana_backtest::run::deployer_depth_blocks()
        );
    }

    /// Every money guard maps to a rule id a user can group refusals by.
    #[test]
    fn every_guard_names_itself() {
        let ids = [
            guard_rule(&Refused::SizePerBuy {
                requested: U256::ZERO,
                cap: U256::ZERO,
            }),
            guard_rule(&Refused::PositionCap {
                held: U256::ZERO,
                requested: U256::ZERO,
                cap: U256::ZERO,
            }),
            guard_rule(&Refused::SessionBudget {
                spent: U256::ZERO,
                requested: U256::ZERO,
                budget: U256::ZERO,
            }),
            guard_rule(&Refused::MaxOpenPositions { open: 0, cap: 0 }),
            guard_rule(&Refused::InsufficientBalance {
                balance: U256::ZERO,
                requested: U256::ZERO,
            }),
        ];
        assert_eq!(ids.len(), 5);
        assert!(ids.iter().all(|s| !s.is_empty()));
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            5,
            "each guard must be distinguishable in the journal"
        );
    }

    /// A position at 3x is not sold by a policy that names no take-profit. An exit that
    /// fires on a default policy would sell everything the moment it moved.
    #[test]
    fn a_policy_with_no_rules_never_sells() {
        let empty = ExitPolicy {
            take_profit_bps: None,
            stop_loss_bps: None,
            trailing_bps: None,
            max_hold_secs: None,
            partials: Vec::new(),
        };
        assert!(exits::decide(&empty, Mark::new(30_000, 60)).is_hold());
        assert!(exits::decide(&empty, Mark::new(1_000, 60)).is_hold());
    }

    #[test]
    fn a_symbol_that_did_not_decode_is_shown_as_a_question_mark() {
        let r = Reading {
            launch: held(0, 0, 0, 0).launch,
            name: String::new(),
            symbol: "  ".into(),
            description: String::new(),
            socials: banana_core::features::Socials::UNKNOWN,
            exempt_wallets: None,
            creator_tax_bps: None,
            fee_recipient: banana_core::features::FeeRecipient::Unknown,
            dev_buy_bps: None,
            dev_buy_quote: None,
            pair: Pair::Eth,
            state: ChainState::default(),
            decoded: false,
            gaps: Vec::new(),
            elapsed_ms: 0,
        };
        assert_eq!(display_symbol(&r), "?");
    }

    /// The events the UI reads must survive the trip as JSON, money included.
    #[test]
    fn events_round_trip_with_money_as_strings() {
        let e = Event::Entered {
            token: "0x01".into(),
            symbol: "TKN".into(),
            quote_wei: "10000000000000000".into(),
            tokens: "123456789012345678901234567890".into(),
            tax_bps: 250,
            tax_paid_bps: Some(240),
            tx_hash: None,
            simulated: true,
            detail: "TEST".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"kind\":\"entered\""));
        assert!(
            json.contains("\"123456789012345678901234567890\""),
            "a token quantity must not go through a JavaScript number"
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), e);
    }
}
