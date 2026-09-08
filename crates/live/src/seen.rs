//! What this session can honestly claim to have seen.
//!
//! The Lab computes deployer history by streaming an indexed window in block order, so a
//! launch only ever sees what came before it. The sniper cannot do that: the launch it
//! cares about happened thirty seconds ago and has no row in any table. It has two sources
//! instead, and the whole point of this module is to keep them honest about their seams.
//!
//! ```text
//!   store window                 hole                watched
//!  [--------------------------] ......... [--------------------------->
//!  from_block         to_block            watched_from            head
//! ```
//!
//! * The **store** holds every launch and graduation in its indexed window. All of it is
//!   the past, so counting all of it is point-in-time for a launch happening now.
//! * The **watched** part is what this session has seen since it started.
//! * Between them can be a **hole**: the store was indexed an hour ago and nothing has
//!   watched the hour since. Launches in the hole are invisible, so every deployer in it
//!   looks fresher than it is — the same window-edge bias as PLAN.md C2, but arriving from
//!   the other direction and capable of being much worse.
//!
//! The hole is closed by [`bridge`], a one-off scan of launch and graduation *events*
//! between the store's edge and the head. Events only: no calldata, no receipts, so it is
//! a handful of `eth_getLogs` calls rather than two per launch. When the hole is too large
//! to bridge, it is **reported**, [`Coverage::depth_at`] collapses to what was really
//! watched, and any strategy reading a deployer feature refuses on depth — which is the
//! honest failure, and the same one the Lab already applies.
//!
//! # Fingerprint twins are not bridged
//!
//! A fingerprint needs the launch calldata, which is two calls per launch, so bridging one
//! is not a handful of requests but thousands. Twins are therefore counted only over
//! launches this session enriched itself, and [`Seen::twin_window_covered`] says when that
//! is enough to answer with. Before it is, a strategy using `MaxFingerprintTwins` refuses
//! and says so: an undercount would make the rule *more* permissive, which is the
//! flattering direction and precisely what spec §5.5 forbids.

use std::collections::HashMap;

use alloy_primitives::Address;
use alloy_sol_types::SolEvent;
use banana_chain::abi::IPonsFactory;
use banana_chain::addr;
use banana_chain::gate::{Priority, RpcError};
use banana_chain::launch_log;
use banana_chain::rpc::{Client, LogFilter};
use banana_core::features::{Fingerprint, TWIN_WINDOW_BLOCKS};
use banana_store::{DeployerSeen, History};
use serde::{Deserialize, Serialize};

/// Largest hole the bridge will close, in blocks. ~30 minutes.
///
/// At ~25 launches per 1,000 blocks and 5,000 blocks per request this is six `eth_getLogs`
/// calls, or about three seconds at the gate's logs spacing. A bigger hole than this means
/// the store is stale enough that re-indexing is the right answer, and saying so beats
/// spending a minute of the session's hot budget rebuilding an index badly.
pub const MAX_BRIDGE_BLOCKS: u64 = 18_000;

/// How much history the sniper actually has, and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// The indexed window, when there is a store with one.
    pub store: Option<(u64, u64)>,
    /// The first block this session is accountable for.
    pub watched_from: u64,
    /// Blocks between the store's end and `watched_from`. Zero when the two meet, either
    /// because the store is current or because [`bridge`] closed the gap.
    pub hole: u64,
}

impl Coverage {
    /// Contiguous history ending at `block`, in blocks.
    ///
    /// With a hole, only what was actually watched counts. Adding the store's window
    /// across a hole would claim a continuity that is not there, and the number is used to
    /// decide whether a deployer rule may fire at all.
    pub fn depth_at(&self, block: u64) -> u64 {
        match self.store {
            Some((from, _)) if self.hole == 0 => block.saturating_sub(from),
            _ => block.saturating_sub(self.watched_from),
        }
    }

    /// Said plainly, for the Status view.
    pub fn describe(&self) -> String {
        match (self.store, self.hole) {
            (Some((from, to)), 0) => {
                format!("continuous from block {from}: indexed to {to}, watched since then",)
            }
            (Some((_, to)), h) => format!(
                "the index stops at block {to} and this session began {h} blocks later \
                 (about {}m). Launches in between were never seen, so deployer history \
                 counts only what has been watched since",
                h.saturating_mul(addr::BLOCK_MS) / 60_000
            ),
            (None, _) => "no index: deployer history counts only what has been watched \
                          this session"
                .to_owned(),
        }
    }
}

/// One launch, as the overlay records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Watched {
    deployer: Address,
    block: u64,
    /// `None` for a launch bridged from events, which has no calldata read.
    fingerprint: Option<Fingerprint>,
}

/// The store's window plus everything this session has watched.
#[derive(Debug)]
pub struct Seen {
    coverage: Coverage,
    /// token -> what was seen about it.
    launches: HashMap<Address, Watched>,
    /// deployer -> launches, graduations, both since the store's edge.
    by_deployer: HashMap<Address, (u32, u32)>,
    /// Tokens seen to graduate, so one counted twice stays counted once.
    graduated: HashMap<Address, u64>,
    /// The earliest block whose launches were enriched, so twins have a window.
    enriched_from: Option<u64>,
}

impl Seen {
    pub fn new(coverage: Coverage) -> Self {
        Self {
            coverage,
            launches: HashMap::new(),
            by_deployer: HashMap::new(),
            graduated: HashMap::new(),
            enriched_from: None,
        }
    }

    pub fn coverage(&self) -> Coverage {
        self.coverage
    }

    /// Fold a bridge scan into the overlay and close the hole it covered.
    pub fn absorb(&mut self, bridged: Bridged, history: Option<&History>) {
        for (token, deployer, block) in bridged.launches {
            let _ = self.record_launch(token, deployer, block, None);
        }
        for (token, block) in bridged.graduations {
            self.record_graduation(token, block, history);
        }
        self.coverage.watched_from = bridged.from.min(self.coverage.watched_from);
        self.coverage.hole = 0;
    }

    /// Record a launch, and say whether it had been seen before.
    ///
    /// Idempotent by token, which is what makes a re-covered block range harmless: a sweep
    /// that fails leaves its checkpoint alone on purpose, so the retry re-reads the same
    /// blocks and the same launches come round again. Counting them twice would inflate
    /// the very number a `max_deployer_launches` rule is trying to bound, and processing
    /// them twice would refuse the same token twice in the journal.
    ///
    /// Returns `true` the first time a token is seen and `false` afterwards.
    pub fn record_launch(
        &mut self,
        token: Address,
        deployer: Address,
        block: u64,
        fingerprint: Option<Fingerprint>,
    ) -> bool {
        let known = self.launches.get(&token);
        let first_time = known.is_none();
        // A second sighting may carry the fingerprint the first one lacked, when a bridged
        // launch is later enriched.
        let fingerprint = fingerprint.or_else(|| known.and_then(|w| w.fingerprint.clone()));
        if fingerprint.is_some() {
            self.enriched_from = Some(match self.enriched_from {
                Some(f) => f.min(block),
                None => block,
            });
        }
        self.launches.insert(
            token,
            Watched {
                deployer,
                block,
                fingerprint,
            },
        );
        if first_time {
            self.by_deployer.entry(deployer).or_default().0 += 1;
        }
        first_time
    }

    /// Record a graduation, attributing it to whichever deployer launched the token.
    ///
    /// A token that launched before this session began has no overlay entry, so the store
    /// is asked. When neither knows it, the graduation is still recorded — it just cannot
    /// be credited to a deployer, which is better than crediting it to the wrong one.
    pub fn record_graduation(&mut self, token: Address, block: u64, history: Option<&History>) {
        if self.graduated.contains_key(&token) {
            return;
        }
        self.graduated.insert(token, block);
        let deployer = self
            .launches
            .get(&token)
            .map(|w| w.deployer)
            .or_else(|| history.and_then(|h| h.deployer_of(token).ok().flatten()));
        if let Some(d) = deployer {
            self.by_deployer.entry(d).or_default().1 += 1;
        }
    }

    /// Everything known about a deployer: the store's counts plus this session's.
    ///
    /// The two do not overlap. The store covers `[from, to]`; the overlay starts at the
    /// bridge's first block, which is `to + 1`. A launch counted twice would inflate the
    /// very number a `MaxDeployerLaunches` rule is trying to bound.
    pub fn deployer(&self, deployer: Address, history: Option<&History>) -> DeployerSeen {
        let stored = history
            .and_then(|h| h.deployer_history(deployer).ok())
            .unwrap_or_default();
        let (l, g) = self.by_deployer.get(&deployer).copied().unwrap_or((0, 0));
        DeployerSeen {
            launches: stored.launches.saturating_add(l),
            graduations: stored.graduations.saturating_add(g),
        }
    }

    /// Earlier launches in the preceding 30 minutes sharing this fingerprint, from a
    /// **different** deployer.
    ///
    /// Only over launches this session enriched. Ask [`Self::twin_window_covered`] first:
    /// this number is a lower bound until the window is covered, and a lower bound on a
    /// `max_` rule is the flattering direction.
    pub fn twins_30m(&self, fingerprint: &Fingerprint, deployer: Address, block: u64) -> u32 {
        if fingerprint.is_uninformative() {
            // Two launches nobody could decode are not evidence of a shared operator.
            return 0;
        }
        let cutoff = block.saturating_sub(TWIN_WINDOW_BLOCKS);
        self.launches
            .values()
            .filter(|w| w.block >= cutoff && w.block < block && w.deployer != deployer)
            .filter(|w| w.fingerprint.as_ref() == Some(fingerprint))
            .count() as u32
    }

    /// Whether the 30 minutes before `block` were entirely watched with calldata read.
    ///
    /// False means a twin count would be an undercount, and a strategy that reads twins
    /// must refuse rather than pass on a number it knows is too small.
    pub fn twin_window_covered(&self, block: u64) -> bool {
        match self.enriched_from {
            Some(from) => block.saturating_sub(from) >= TWIN_WINDOW_BLOCKS,
            None => false,
        }
    }

    /// How much of the twin window is covered so far, for a refusal that says how long is
    /// left rather than only that it is not ready.
    pub fn twin_coverage_blocks(&self, block: u64) -> u64 {
        self.enriched_from
            .map(|from| block.saturating_sub(from))
            .unwrap_or(0)
    }

    /// Launches this session has seen, for the Status view.
    pub fn watched_launches(&self) -> usize {
        self.launches.len()
    }
}

/// The result of one bridge scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bridged {
    pub from: u64,
    pub to: u64,
    /// `(token, deployer, block)`.
    pub launches: Vec<(Address, Address, u64)>,
    /// `(token, block)`.
    pub graduations: Vec<(Address, u64)>,
}

/// Close the hole between the store's edge and where watching begins.
///
/// Events only — `TokenLaunched` and `PoolGraduated`, both on the factory, both in one
/// filter — so the cost is a handful of `eth_getLogs` calls rather than two per launch.
/// That is enough for deployer counts, which is what the depth restriction gates on. It is
/// not enough for fingerprints, and the module docs say why that is left undone.
///
/// Refuses a hole larger than [`MAX_BRIDGE_BLOCKS`] rather than spending a minute of the
/// session's hot budget rebuilding an index badly.
pub async fn bridge(
    client: &Client,
    from: u64,
    to: u64,
    max_blocks_per_call: u64,
) -> Result<Bridged, BridgeError> {
    if from > to {
        return Ok(Bridged {
            from,
            to: from.saturating_sub(1),
            ..Default::default()
        });
    }
    let span = to - from + 1;
    if span > MAX_BRIDGE_BLOCKS {
        return Err(BridgeError::TooWide {
            blocks: span,
            max: MAX_BRIDGE_BLOCKS,
        });
    }

    let topics = [
        launch_log::topic0(),
        IPonsFactory::PoolGraduated::SIGNATURE_HASH,
    ];
    let mut out = Bridged {
        from,
        to,
        ..Default::default()
    };
    let mut at = from;
    while at <= to {
        let chunk_end = to.min(at + max_blocks_per_call - 1);
        let filter = LogFilter::new(at, chunk_end)
            .address(addr::PONS_FACTORY)
            .topics(topics);
        let logs = client.get_logs(&filter, Priority::Hot).await?;
        for l in &logs {
            match l.topic0() {
                Some(t) if t == launch_log::topic0() => {
                    if let Some(d) = launch_log::decode(l) {
                        out.launches.push((d.token, d.deployer, d.block));
                    }
                }
                Some(t) if t == IPonsFactory::PoolGraduated::SIGNATURE_HASH => {
                    if let Some(token) = l.topics.get(1) {
                        out.graduations
                            .push((Address::from_slice(&token.0[12..]), l.block_number));
                    }
                }
                _ => {}
            }
        }
        at = chunk_end + 1;
    }
    Ok(out)
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(
        "the index stops {blocks} blocks behind the chain, which is more than the {max} \
         this can bridge. Re-index before trading, or accept that deployer rules will \
         refuse until the session has watched long enough"
    )]
    TooWide { blocks: u64, max: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use banana_core::features::{Presence, Socials};

    fn addr_n(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn fp(dev: u64) -> Fingerprint {
        Fingerprint::new(
            Some(alloy_primitives::U256::from(dev)),
            Some(100),
            Socials {
                twitter: Presence::Present,
                website: Presence::Absent,
                telegram: Presence::Absent,
            },
            Some(0),
        )
    }

    fn covered() -> Coverage {
        Coverage {
            store: Some((100_000, 900_000)),
            watched_from: 900_001,
            hole: 0,
        }
    }

    /// With the store and the session meeting, depth runs from the index's first block.
    #[test]
    fn contiguous_coverage_counts_the_whole_indexed_window() {
        assert_eq!(covered().depth_at(900_500), 800_500);
    }

    /// With a hole, the indexed window is not added across it: the launches in the hole
    /// were never seen, and claiming continuity would be claiming to have seen them.
    #[test]
    fn a_hole_collapses_depth_to_what_was_actually_watched() {
        let c = Coverage {
            store: Some((100_000, 900_000)),
            watched_from: 950_000,
            hole: 49_999,
        };
        assert_eq!(c.depth_at(950_500), 500);
        assert!(c.describe().contains("never seen"));
    }

    #[test]
    fn with_no_store_only_the_session_counts() {
        let c = Coverage {
            store: None,
            watched_from: 950_000,
            hole: 0,
        };
        assert_eq!(c.depth_at(950_500), 500);
    }

    /// The overlay adds to the store rather than replacing it.
    #[test]
    fn deployer_counts_add_the_session_to_the_store() {
        let mut s = Seen::new(covered());
        s.record_launch(addr_n(10), addr_n(1), 900_010, None);
        s.record_launch(addr_n(11), addr_n(1), 900_020, None);
        // No store handle here, so the store's contribution is zero.
        let d = s.deployer(addr_n(1), None);
        assert_eq!(d.launches, 2);
        assert_eq!(d.graduations, 0);
    }

    /// Re-covering a block range after a failed sweep must not double-count.
    /// A failed sweep re-covers its blocks on the retry, so the same launch arrives
    /// twice. It must count once, and the second sighting must say so, because the engine
    /// keys its whole pipeline on that answer.
    #[test]
    fn seeing_the_same_launch_twice_counts_it_once_and_says_so() {
        let mut s = Seen::new(covered());
        assert!(s.record_launch(addr_n(10), addr_n(1), 900_010, None));
        assert!(!s.record_launch(addr_n(10), addr_n(1), 900_010, None));
        assert_eq!(s.deployer(addr_n(1), None).launches, 1);
    }

    #[test]
    fn a_graduation_is_credited_to_the_deployer_that_launched_it() {
        let mut s = Seen::new(covered());
        s.record_launch(addr_n(10), addr_n(1), 900_010, None);
        s.record_graduation(addr_n(10), 900_100, None);
        s.record_graduation(addr_n(10), 900_100, None);
        let d = s.deployer(addr_n(1), None);
        assert_eq!(d.launches, 1);
        assert_eq!(d.graduations, 1, "counted once, not twice");
    }

    /// A graduation for a token nobody here launched is recorded but credited to nobody,
    /// which beats crediting it to the wrong deployer.
    #[test]
    fn an_unattributable_graduation_credits_no_deployer() {
        let mut s = Seen::new(covered());
        s.record_graduation(addr_n(99), 900_100, None);
        assert_eq!(s.deployer(addr_n(1), None).graduations, 0);
    }

    #[test]
    fn twins_are_earlier_launches_from_other_deployers_sharing_a_fingerprint() {
        let mut s = Seen::new(covered());
        s.record_launch(addr_n(10), addr_n(1), 900_010, Some(fp(5)));
        s.record_launch(addr_n(11), addr_n(2), 900_020, Some(fp(5)));
        // Same template, a third wallet, later.
        assert_eq!(s.twins_30m(&fp(5), addr_n(3), 900_030), 2);
        // Its own earlier launch is not its twin.
        assert_eq!(s.twins_30m(&fp(5), addr_n(1), 900_030), 1);
        // A different template is not a twin at all.
        assert_eq!(s.twins_30m(&fp(9), addr_n(3), 900_030), 0);
    }

    #[test]
    fn a_launch_outside_the_window_is_not_a_twin() {
        let mut s = Seen::new(covered());
        s.record_launch(addr_n(10), addr_n(1), 900_010, Some(fp(5)));
        let far = 900_010 + TWIN_WINDOW_BLOCKS + 1;
        assert_eq!(s.twins_30m(&fp(5), addr_n(3), far), 0);
    }

    /// The rule that stops the count being used before it means anything.
    #[test]
    fn the_twin_window_is_not_covered_until_it_has_been_watched() {
        let mut s = Seen::new(covered());
        assert!(!s.twin_window_covered(900_000), "nothing enriched yet");
        s.record_launch(addr_n(10), addr_n(1), 900_000, Some(fp(5)));
        assert!(!s.twin_window_covered(900_000 + TWIN_WINDOW_BLOCKS - 1));
        assert!(s.twin_window_covered(900_000 + TWIN_WINDOW_BLOCKS));
        assert_eq!(
            s.twin_coverage_blocks(900_000 + 100),
            100,
            "and it says how far in it is, so a refusal can name the wait"
        );
    }

    /// A bridged launch has no fingerprint, so it never starts the twin window: counting
    /// twins over launches whose templates were never read would be counting nothing.
    #[test]
    fn a_bridged_launch_does_not_open_the_twin_window() {
        let mut s = Seen::new(covered());
        s.record_launch(addr_n(10), addr_n(1), 900_000, None);
        assert!(!s.twin_window_covered(900_000 + TWIN_WINDOW_BLOCKS * 2));
        assert_eq!(s.deployer(addr_n(1), None).launches, 1, "but it does count");
    }

    /// Enriching a launch that was bridged first keeps the deployer count at one and adds
    /// the template.
    #[test]
    fn enriching_a_bridged_launch_adds_its_template_without_recounting_it() {
        let mut s = Seen::new(covered());
        s.record_launch(addr_n(10), addr_n(1), 900_000, None);
        s.record_launch(addr_n(10), addr_n(1), 900_000, Some(fp(5)));
        assert_eq!(s.deployer(addr_n(1), None).launches, 1);
        assert_eq!(s.twins_30m(&fp(5), addr_n(2), 900_010), 1);
    }

    #[test]
    fn absorbing_a_bridge_closes_the_hole() {
        let mut s = Seen::new(Coverage {
            store: Some((100_000, 900_000)),
            watched_from: 900_500,
            hole: 499,
        });
        s.absorb(
            Bridged {
                from: 900_001,
                to: 900_499,
                launches: vec![(addr_n(10), addr_n(1), 900_100)],
                graduations: vec![(addr_n(10), 900_200)],
            },
            None,
        );
        assert_eq!(s.coverage().hole, 0);
        assert_eq!(s.coverage().watched_from, 900_001);
        assert_eq!(s.coverage().depth_at(900_600), 800_600);
        let d = s.deployer(addr_n(1), None);
        assert_eq!((d.launches, d.graduations), (1, 1));
    }

    #[tokio::test]
    async fn a_hole_too_wide_to_bridge_is_refused_by_name() {
        // No transport is reached: the refusal happens on the span alone.
        let gate = banana_chain::gate::Gate::new(
            Box::new(Unreachable),
            vec![banana_chain::gate::Endpoint::new("http://x", "x", true)],
            Default::default(),
        );
        let client = Client::new(gate);
        let e = bridge(&client, 1, 1 + MAX_BRIDGE_BLOCKS, 5_000)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("Re-index before trading"));
    }

    #[derive(Debug)]
    struct Unreachable;

    #[async_trait::async_trait]
    impl banana_chain::transport::Transport for Unreachable {
        async fn post(
            &self,
            _u: &str,
            _b: &str,
        ) -> Result<banana_chain::transport::HttpResponse, banana_chain::transport::TransportError>
        {
            panic!("the span check must happen before any request");
        }
    }
}
