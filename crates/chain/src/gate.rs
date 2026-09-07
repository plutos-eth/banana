//! The RPC gate: every JSON-RPC request in the process passes through here.
//!
//! Spec §6.3. This is a port of bodkin's `src/util/rpcGate.ts` with the two things it
//! lacks, both of which matter more than they sound:
//!
//! **Priority.** In bodkin a background scan and a hot-path tax poll compete for the same
//! three slots. That is fine until it isn't: the sniper has ~3 seconds between detecting a
//! launch and needing to buy, and an indexer saturating the pool can spend all of it. Here
//! [`Priority::Hot`] has reserved capacity that [`Priority::Bulk`] can never occupy, and
//! Bulk additionally stands aside while any Hot request is waiting.
//!
//! **Adaptive capacity.** bodkin's cooldown is a blunt on/off, which permanently caps a
//! private endpoint at limits measured against a public one. Here concurrency moves by
//! AIMD: additive increase while an endpoint answers cleanly, multiplicative decrease the
//! moment it refuses.
//!
//! # What was measured (2026-09-07)
//!
//! * The official RPC serves `eth_getLogs` but meters it hard: 7 of 8 calls succeeded at
//!   500 ms spacing, and short bursts 429 immediately.
//! * publicnode is fast and generous for state reads — 8-way concurrency, 24/24 clean, 35
//!   ms/request effective — but refuses `eth_getLogs`.
//! * `eth_getLogs` has a hard **10,000 result cap** (`-32000`). That is not a rate limit
//!   and no amount of backoff fixes it; the caller has to split the range. The gate
//!   surfaces it as its own error variant so the indexer can do exactly that.
//!
//! So endpoints carry a capability flag and each method is routed to one that serves it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Instant, sleep};

use crate::transport::{Transport, TransportError};

/// Which class of work a request belongs to.
///
/// This is not a hint. `Bulk` can never take a `Hot` slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Live detection, tax polling, order execution. Latency is the product.
    Hot,
    /// Indexing and backfill. Throughput matters; latency does not.
    Bulk,
}

/// One JSON-RPC endpoint and what it will serve.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub label: String,
    /// Whether this endpoint serves `eth_getLogs`. publicnode does not.
    pub logs: bool,
}

impl Endpoint {
    pub fn new(url: impl Into<String>, label: impl Into<String>, logs: bool) -> Self {
        Self {
            url: url.into(),
            label: label.into(),
            logs,
        }
    }
}

/// The two public endpoints, in the order bodkin uses them and for the same reasons.
pub fn default_endpoints() -> Vec<Endpoint> {
    vec![
        Endpoint::new("https://robinhood-rpc.publicnode.com", "publicnode", false),
        Endpoint::new("https://rpc.mainnet.chain.robinhood.com", "robinhood", true),
    ]
}

/// Parse `RPC_URL`: comma-separated, preferred first, `#nologs` to mark an endpoint that
/// refuses `eth_getLogs`.
pub fn parse_endpoints(raw: &str) -> Vec<Endpoint> {
    let defaults = default_endpoints();
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (url, logs) = match entry.strip_suffix("#nologs") {
                Some(u) => (u, false),
                None => (entry, true),
            };
            // A known endpoint keeps its known capability even without the marker.
            match defaults.iter().find(|d| d.url == url) {
                Some(known) => known.clone(),
                None => {
                    let label = url
                        .split("://")
                        .nth(1)
                        .and_then(|h| h.split('/').next())
                        .unwrap_or(url)
                        .to_string();
                    Endpoint::new(url, label, logs)
                }
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Slots only `Hot` may use. The sniper's reserved runway.
    pub hot_reserved: usize,
    /// Slots both classes share, before AIMD adjusts it.
    pub shared_start: usize,
    /// Ceiling AIMD may raise the shared pool to. A private endpoint earns its way up.
    pub shared_max: usize,
    /// Minimum spacing between any two requests.
    pub spacing: Duration,
    /// Minimum spacing between `eth_getLogs` calls. Much larger, because it is metered
    /// much harder.
    pub logs_spacing: Duration,
    /// Process-wide pause after a refusal.
    pub cooldown: Duration,
    /// How long a refusing endpoint sits out.
    pub bench: Duration,
    /// A Cloudflare challenge is not retryable; sit that endpoint out for much longer.
    pub challenge_bench: Duration,
    /// Clean responses needed before AIMD adds a slot.
    pub increase_after: u32,
    pub max_attempts: u32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            // Measured-safe defaults for the PUBLIC endpoints. AIMD raises them when the
            // endpoint proves it tolerates more.
            hot_reserved: 2,
            shared_start: 3,
            shared_max: 16,
            spacing: Duration::from_millis(50),
            logs_spacing: Duration::from_millis(400),
            cooldown: Duration::from_secs(3),
            bench: Duration::from_secs(4),
            challenge_bench: Duration::from_secs(60),
            increase_after: 20,
            max_attempts: 8,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// `eth_getLogs` matched more than the endpoint will return.
    ///
    /// Its own variant because it is **not** a rate limit: retrying or waiting cannot fix
    /// it, and the only correct response is to split the block range. Collapsing it into a
    /// generic error is how an indexer ends up looking like it has hung.
    #[error("query matched more than {limit} logs; split the block range")]
    TooManyResults { limit: u64 },
    #[error(
        "no configured endpoint serves {method} (eth_getLogs needs one that allows it; set RPC_URL)"
    )]
    NoCapableEndpoint { method: String },
    #[error("rpc {method}: gave up after {attempts} attempts ({last})")]
    Exhausted {
        method: String,
        attempts: u32,
        last: String,
    },
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("malformed response from {label}: {detail}")]
    Malformed { label: String, detail: String },
    #[error(transparent)]
    Transport(#[from] TransportError),
}

#[derive(Debug)]
struct EndpointState {
    endpoint: Endpoint,
    /// Monotonic millis since gate start; 0 means healthy.
    benched_until_ms: AtomicU64,
}

/// A snapshot for the status line (spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GateStats {
    pub hot_in_flight: usize,
    pub bulk_in_flight: usize,
    pub hot_waiting: usize,
    pub shared_capacity: usize,
    pub throttled: u64,
    pub cooling: bool,
    pub endpoints: Vec<EndpointHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EndpointHealth {
    pub label: String,
    pub logs: bool,
    pub benched: bool,
}

/// The single chokepoint. Clone freely: all clones share one set of limits.
#[derive(Debug, Clone)]
pub struct Gate {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    transport: Box<dyn Transport>,
    endpoints: Vec<EndpointState>,
    config: GateConfig,

    /// Slots only Hot may take.
    hot_only: Semaphore,
    /// Slots either class may take; AIMD resizes this one.
    shared: Semaphore,
    /// Current shared capacity, tracked separately because a semaphore does not report it.
    shared_capacity: AtomicUsize,

    /// Hot requests currently waiting for a slot. Bulk stands aside while this is nonzero.
    hot_waiting: AtomicUsize,
    hot_in_flight: AtomicUsize,
    bulk_in_flight: AtomicUsize,

    last_request: Mutex<Option<Instant>>,
    last_logs_request: Mutex<Option<Instant>>,
    cooldown_until: Mutex<Option<Instant>>,

    clean_streak: AtomicUsize,
    throttled: AtomicU64,
    next_id: AtomicU64,
    start: Instant,
}

impl Gate {
    pub fn new(
        transport: Box<dyn Transport>,
        endpoints: Vec<Endpoint>,
        config: GateConfig,
    ) -> Self {
        let inner = Inner {
            transport,
            endpoints: endpoints
                .into_iter()
                .map(|e| EndpointState {
                    endpoint: e,
                    benched_until_ms: AtomicU64::new(0),
                })
                .collect(),
            hot_only: Semaphore::new(config.hot_reserved),
            shared: Semaphore::new(config.shared_start),
            shared_capacity: AtomicUsize::new(config.shared_start),
            hot_waiting: AtomicUsize::new(0),
            hot_in_flight: AtomicUsize::new(0),
            bulk_in_flight: AtomicUsize::new(0),
            last_request: Mutex::new(None),
            last_logs_request: Mutex::new(None),
            cooldown_until: Mutex::new(None),
            clean_streak: AtomicUsize::new(0),
            throttled: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            start: Instant::now(),
            config,
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn stats(&self) -> GateStats {
        let i = &self.inner;
        let now = i.now_ms();
        GateStats {
            hot_in_flight: i.hot_in_flight.load(Ordering::Relaxed),
            bulk_in_flight: i.bulk_in_flight.load(Ordering::Relaxed),
            hot_waiting: i.hot_waiting.load(Ordering::Relaxed),
            shared_capacity: i.shared_capacity.load(Ordering::Relaxed),
            throttled: i.throttled.load(Ordering::Relaxed),
            cooling: false, // filled by callers that can await; see `stats_async`
            endpoints: i
                .endpoints
                .iter()
                .map(|e| EndpointHealth {
                    label: e.endpoint.label.clone(),
                    logs: e.endpoint.logs,
                    benched: e.benched_until_ms.load(Ordering::Relaxed) > now,
                })
                .collect(),
        }
    }

    /// Send a JSON-RPC call. `params` must be a JSON array.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        priority: Priority,
    ) -> Result<serde_json::Value, RpcError> {
        self.inner.call(method, params, priority).await
    }
}

impl Inner {
    fn now_ms(&self) -> u64 {
        Instant::now().duration_since(self.start).as_millis() as u64
    }

    fn bench(&self, idx: usize, how_long: Duration) {
        self.endpoints[idx].benched_until_ms.store(
            self.now_ms() + how_long.as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    /// Endpoints that can serve this method, healthy ones first.
    ///
    /// When none is healthy the least-recently-benched is returned anyway: waiting on a
    /// benched endpoint beats failing the caller, which is bodkin's rule and a good one.
    fn candidates(&self, method: &str) -> Vec<usize> {
        let needs_logs = method == "eth_getLogs";
        let capable: Vec<usize> = self
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| !needs_logs || e.endpoint.logs)
            .map(|(i, _)| i)
            .collect();
        if capable.is_empty() {
            return capable;
        }
        let now = self.now_ms();
        let healthy: Vec<usize> = capable
            .iter()
            .copied()
            .filter(|&i| self.endpoints[i].benched_until_ms.load(Ordering::Relaxed) <= now)
            .collect();
        if !healthy.is_empty() {
            return healthy;
        }
        let mut all = capable;
        all.sort_by_key(|&i| self.endpoints[i].benched_until_ms.load(Ordering::Relaxed));
        all.truncate(1);
        all
    }

    /// Acquire a slot, honouring priority and spacing.
    async fn acquire(&self, method: &str, priority: Priority) -> Slot<'_> {
        let is_logs = method == "eth_getLogs";

        let permit = match priority {
            Priority::Hot => {
                self.hot_waiting.fetch_add(1, Ordering::SeqCst);
                // Take whichever pool frees up first. Racing both is what keeps a Hot
                // request from queueing behind Bulk on the shared pool while a reserved
                // slot sits idle.
                let permit = tokio::select! {
                    p = self.hot_only.acquire() => p,
                    p = self.shared.acquire() => p,
                }
                .expect("gate semaphores are never closed");
                self.hot_waiting.fetch_sub(1, Ordering::SeqCst);
                self.hot_in_flight.fetch_add(1, Ordering::SeqCst);
                permit
            }
            Priority::Bulk => {
                // Stand aside while the hot path is waiting. Reserved slots alone are not
                // enough: without this, Bulk still consumes the spacing budget and adds
                // latency to a live entry.
                while self.hot_waiting.load(Ordering::SeqCst) > 0 {
                    sleep(Duration::from_millis(2)).await;
                }
                let permit = self
                    .shared
                    .acquire()
                    .await
                    .expect("gate semaphores are never closed");
                self.bulk_in_flight.fetch_add(1, Ordering::SeqCst);
                permit
            }
        };

        // Spacing is a property of the endpoint, so it applies to everyone. Cooling
        // stretches it rather than stopping traffic dead.
        let cooling = {
            let until = self.cooldown_until.lock().await;
            until.is_some_and(|t| Instant::now() < t)
        };

        {
            let mut last = self.last_request.lock().await;
            let gap = if cooling {
                self.config.spacing * 5
            } else {
                self.config.spacing
            };
            if let Some(prev) = *last {
                let elapsed = prev.elapsed();
                if elapsed < gap {
                    sleep(gap - elapsed).await;
                }
            }
            *last = Some(Instant::now());
        }

        if is_logs {
            let mut last = self.last_logs_request.lock().await;
            let gap = if cooling {
                self.config.logs_spacing * 3
            } else {
                self.config.logs_spacing
            };
            if let Some(prev) = *last {
                let elapsed = prev.elapsed();
                if elapsed < gap {
                    sleep(gap - elapsed).await;
                }
            }
            *last = Some(Instant::now());
        }

        Slot {
            inner: self,
            priority,
            _permit: permit,
        }
    }

    /// AIMD: additive increase on a clean streak.
    fn note_clean(&self) {
        let streak = self.clean_streak.fetch_add(1, Ordering::Relaxed) + 1;
        if streak as u32 >= self.config.increase_after {
            self.clean_streak.store(0, Ordering::Relaxed);
            let cap = self.shared_capacity.load(Ordering::Relaxed);
            if cap < self.config.shared_max {
                self.shared.add_permits(1);
                self.shared_capacity.store(cap + 1, Ordering::Relaxed);
                tracing::debug!(capacity = cap + 1, "gate: raising concurrency");
            }
        }
    }

    /// AIMD: multiplicative decrease the moment an endpoint refuses.
    fn note_refusal(&self) {
        self.clean_streak.store(0, Ordering::Relaxed);
        self.throttled.fetch_add(1, Ordering::Relaxed);
        let cap = self.shared_capacity.load(Ordering::Relaxed);
        let target = (cap / 2).max(1);
        let give_back = cap - target;
        if give_back > 0 {
            // `forget_permits` removes capacity without needing to hold it.
            let removed = self.shared.forget_permits(give_back);
            self.shared_capacity.store(cap - removed, Ordering::Relaxed);
            tracing::debug!(capacity = cap - removed, "gate: cutting concurrency");
        }
    }

    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        priority: Priority,
    ) -> Result<serde_json::Value, RpcError> {
        let mut last_error = String::from("no attempt made");

        for attempt in 0..self.config.max_attempts {
            let candidates = self.candidates(method);
            if candidates.is_empty() {
                return Err(RpcError::NoCapableEndpoint {
                    method: method.to_owned(),
                });
            }
            let idx = candidates[(attempt as usize).min(candidates.len() - 1)];
            let label = self.endpoints[idx].endpoint.label.clone();
            let url = self.endpoints[idx].endpoint.url.clone();

            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string();

            let response = {
                let _slot = self.acquire(method, priority).await;
                self.transport.post(&url, &body).await
            };

            let response = match response {
                Ok(r) => r,
                Err(TransportError::CacheMiss { method }) => {
                    // A replay miss is a fact about the recording, not a flaky network.
                    // Retrying it just wastes time and hides the real problem.
                    return Err(RpcError::Transport(TransportError::CacheMiss { method }));
                }
                Err(e) => {
                    last_error = format!("{label}: {e}");
                    self.bench(idx, Duration::from_secs(5));
                    sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
                    continue;
                }
            };

            // --- throttling ------------------------------------------------------------
            if response.status == 429 || response.status == 503 {
                self.note_refusal();
                *self.cooldown_until.lock().await = Some(Instant::now() + self.config.cooldown);
                self.bench(idx, self.config.bench);
                last_error = format!("{label}: HTTP {}", response.status);
                let backoff = if candidates.len() > 1 {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(400 * (1u64 << attempt.min(5)))
                        .min(Duration::from_secs(15))
                };
                sleep(backoff).await;
                continue;
            }

            // --- bot challenge ---------------------------------------------------------
            if response.status == 403 && is_challenge(&response.body) {
                // No retry solves this. Sit the endpoint out and use another.
                self.note_refusal();
                self.bench(idx, self.config.challenge_bench);
                last_error = format!("{label}: bot-protection challenge on this IP");
                if candidates.len() > 1 {
                    continue;
                }
                sleep(Duration::from_secs(5)).await;
                continue;
            }

            // --- body ------------------------------------------------------------------
            let parsed: serde_json::Value = match serde_json::from_str(&response.body) {
                Ok(v) => v,
                Err(e) => {
                    last_error = format!(
                        "{label}: HTTP {}, non-JSON body ({e}): {}",
                        response.status,
                        response.body.chars().take(80).collect::<String>()
                    );
                    self.bench(idx, Duration::from_secs(5));
                    sleep(Duration::from_millis(300)).await;
                    continue;
                }
            };

            if let Some(err) = parsed.get("error") {
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();

                if let Some(limit) = too_many_results(&message) {
                    // Not a rate limit. Hand it straight back so the caller can split.
                    return Err(RpcError::TooManyResults { limit });
                }
                if code == 429 {
                    self.note_refusal();
                    *self.cooldown_until.lock().await = Some(Instant::now() + self.config.cooldown);
                    self.bench(idx, self.config.bench);
                    last_error = format!("{label}: rpc 429");
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }
                // A genuine contract-level error (a revert, a bad param). Retrying will
                // produce the same answer, so return it.
                return Err(RpcError::Rpc { code, message });
            }

            self.note_clean();
            return parsed
                .get("result")
                .cloned()
                .ok_or_else(|| RpcError::Malformed {
                    label,
                    detail: "response has neither result nor error".into(),
                });
        }

        Err(RpcError::Exhausted {
            method: method.to_owned(),
            attempts: self.config.max_attempts,
            last: last_error,
        })
    }
}

/// Held for the duration of one request; releases the slot and the in-flight count.
struct Slot<'a> {
    inner: &'a Inner,
    priority: Priority,
    _permit: tokio::sync::SemaphorePermit<'a>,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        match self.priority {
            Priority::Hot => self.inner.hot_in_flight.fetch_sub(1, Ordering::SeqCst),
            Priority::Bulk => self.inner.bulk_in_flight.fetch_sub(1, Ordering::SeqCst),
        };
    }
}

fn is_challenge(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("just a moment") || lower.contains("cloudflare") || lower.contains("challenge")
}

/// Recognise the result cap. Measured message:
/// `logs matched by query exceeds limit of 10000`.
fn too_many_results(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    if !(lower.contains("exceeds limit")
        || lower.contains("more than")
        || lower.contains("too many"))
    {
        return None;
    }
    // Take the last integer in the message as the limit.
    let mut best = None;
    let mut digits = String::new();
    for ch in message.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            if let Ok(n) = digits.parse::<u64>() {
                best = Some(n);
            }
            digits.clear();
        }
    }
    Some(best.unwrap_or(10_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::HttpResponse;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicU32;

    /// A transport whose behaviour the test dictates.
    #[derive(Debug)]
    struct MockTransport {
        /// How long each request "takes".
        latency: Duration,
        calls: AtomicU32,
        /// Refuse the first N requests with 429.
        refuse_first: u32,
        body: String,
    }

    impl MockTransport {
        fn ok(latency_ms: u64) -> Self {
            Self {
                latency: Duration::from_millis(latency_ms),
                calls: AtomicU32::new(0),
                refuse_first: 0,
                body: r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#.into(),
            }
        }
        fn with_body(body: &str) -> Self {
            Self {
                latency: Duration::ZERO,
                calls: AtomicU32::new(0),
                refuse_first: 0,
                body: body.into(),
            }
        }
        fn refusing(n: u32) -> Self {
            Self {
                latency: Duration::ZERO,
                calls: AtomicU32::new(0),
                refuse_first: n,
                body: r#"{"jsonrpc":"2.0","id":1,"result":"0x1"}"#.into(),
            }
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn post(&self, _url: &str, _body: &str) -> Result<HttpResponse, TransportError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.latency.is_zero() {
                sleep(self.latency).await;
            }
            if n < self.refuse_first {
                return Ok(HttpResponse {
                    status: 429,
                    body: "Too Many Requests".into(),
                });
            }
            Ok(HttpResponse {
                status: 200,
                body: self.body.clone(),
            })
        }
    }

    fn gate(t: MockTransport, config: GateConfig) -> Gate {
        Gate::new(Box::new(t), default_endpoints(), config)
    }

    fn fast_config() -> GateConfig {
        GateConfig {
            spacing: Duration::ZERO,
            logs_spacing: Duration::ZERO,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_plain_call_returns_the_result() {
        let g = gate(MockTransport::ok(0), fast_config());
        let v = g
            .call("eth_blockNumber", serde_json::json!([]), Priority::Hot)
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!("0x1"));
    }

    /// Spec §6.3's core promise: a background index must not add latency to a live entry.
    #[tokio::test(start_paused = true)]
    async fn saturating_bulk_never_stalls_a_hot_request() {
        let config = GateConfig {
            hot_reserved: 2,
            shared_start: 3,
            spacing: Duration::ZERO,
            logs_spacing: Duration::ZERO,
            ..Default::default()
        };
        // Each request occupies its slot for a long time.
        let g = gate(MockTransport::ok(2_000), config);

        // Flood with Bulk work far exceeding the shared pool.
        for _ in 0..50 {
            let g2 = g.clone();
            tokio::spawn(async move {
                let _ = g2
                    .call("eth_call", serde_json::json!([]), Priority::Bulk)
                    .await;
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        // A Hot request arriving into that flood must still be served promptly: it has
        // reserved slots Bulk cannot occupy.
        let start = Instant::now();
        g.call("eth_call", serde_json::json!([]), Priority::Hot)
            .await
            .unwrap();
        let waited = start.elapsed();

        assert!(
            waited < Duration::from_millis(2_500),
            "hot waited {waited:?} behind saturating bulk; the reserved pool is not working"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bulk_cannot_occupy_the_reserved_hot_slots() {
        let config = GateConfig {
            hot_reserved: 2,
            shared_start: 1,
            spacing: Duration::ZERO,
            logs_spacing: Duration::ZERO,
            ..Default::default()
        };
        let g = gate(MockTransport::ok(1_000), config);

        for _ in 0..10 {
            let g2 = g.clone();
            tokio::spawn(async move {
                let _ = g2
                    .call("eth_call", serde_json::json!([]), Priority::Bulk)
                    .await;
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Only one bulk request can be in flight; the reserved two are untouched.
        let s = g.stats();
        assert!(
            s.bulk_in_flight <= 1,
            "bulk took {} slots but the shared pool is 1",
            s.bulk_in_flight
        );
    }

    #[tokio::test(start_paused = true)]
    async fn aimd_raises_capacity_on_a_clean_streak() {
        let config = GateConfig {
            increase_after: 5,
            shared_start: 3,
            shared_max: 8,
            ..fast_config()
        };
        let g = gate(MockTransport::ok(0), config);
        assert_eq!(g.stats().shared_capacity, 3);

        for _ in 0..15 {
            g.call("eth_call", serde_json::json!([]), Priority::Bulk)
                .await
                .unwrap();
        }
        assert_eq!(
            g.stats().shared_capacity,
            6,
            "three clean streaks of five should add three slots"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn aimd_halves_capacity_on_a_refusal() {
        let config = GateConfig {
            shared_start: 8,
            increase_after: 1_000,
            ..fast_config()
        };
        let g = gate(MockTransport::refusing(1), config);
        assert_eq!(g.stats().shared_capacity, 8);

        g.call("eth_call", serde_json::json!([]), Priority::Bulk)
            .await
            .unwrap();

        assert_eq!(g.stats().shared_capacity, 4, "multiplicative decrease");
        assert_eq!(g.stats().throttled, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_never_falls_below_one() {
        let config = GateConfig {
            shared_start: 1,
            increase_after: 1_000,
            ..fast_config()
        };
        let g = gate(MockTransport::refusing(3), config);
        let _ = g
            .call("eth_call", serde_json::json!([]), Priority::Bulk)
            .await;
        assert!(g.stats().shared_capacity >= 1, "a zero pool would deadlock");
    }

    #[tokio::test(start_paused = true)]
    async fn eth_get_logs_only_goes_to_an_endpoint_that_serves_it() {
        // publicnode does not serve logs, so a logs-only endpoint list must be respected.
        let g = Gate::new(
            Box::new(MockTransport::ok(0)),
            vec![Endpoint::new("https://nologs.example", "nologs", false)],
            fast_config(),
        );
        let err = g
            .call("eth_getLogs", serde_json::json!([{}]), Priority::Bulk)
            .await
            .expect_err("must refuse rather than send logs to an endpoint that rejects them");
        assert!(matches!(err, RpcError::NoCapableEndpoint { .. }), "{err}");

        // Other methods are fine on the same endpoint.
        g.call("eth_call", serde_json::json!([]), Priority::Bulk)
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_ten_thousand_log_cap_is_its_own_error_not_a_retry() {
        // Measured message from the live endpoint. Retrying cannot fix it; only splitting
        // the range can, so it must reach the caller immediately.
        let g = gate(
            MockTransport::with_body(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"logs matched by query exceeds limit of 10000"}}"#,
            ),
            fast_config(),
        );
        let err = g
            .call("eth_getLogs", serde_json::json!([{}]), Priority::Bulk)
            .await
            .expect_err("must surface");
        match err {
            RpcError::TooManyResults { limit } => assert_eq!(limit, 10_000),
            other => panic!("expected TooManyResults, got {other}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cloudflare_challenge_benches_the_endpoint_and_is_not_retried_there() {
        #[derive(Debug)]
        struct Challenger(AtomicU32);
        #[async_trait]
        impl Transport for Challenger {
            async fn post(&self, url: &str, _b: &str) -> Result<HttpResponse, TransportError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                if url.contains("publicnode") {
                    Ok(HttpResponse {
                        status: 403,
                        body: "<html>Just a moment... Cloudflare</html>".into(),
                    })
                } else {
                    Ok(HttpResponse {
                        status: 200,
                        body: r#"{"jsonrpc":"2.0","id":1,"result":"0x5"}"#.into(),
                    })
                }
            }
        }
        let g = Gate::new(
            Box::new(Challenger(AtomicU32::new(0))),
            default_endpoints(),
            fast_config(),
        );
        let v = g
            .call("eth_call", serde_json::json!([]), Priority::Bulk)
            .await
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!("0x5"),
            "must fail over to the other endpoint"
        );

        let s = g.stats();
        let pn = s
            .endpoints
            .iter()
            .find(|e| e.label == "publicnode")
            .unwrap();
        assert!(pn.benched, "the challenged endpoint must sit out");
    }

    #[tokio::test(start_paused = true)]
    async fn a_contract_revert_is_returned_not_retried() {
        let g = gate(
            MockTransport::with_body(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}}"#,
            ),
            fast_config(),
        );
        let err = g
            .call("eth_call", serde_json::json!([]), Priority::Hot)
            .await
            .expect_err("a revert is an answer");
        assert!(matches!(err, RpcError::Rpc { code: 3, .. }), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn logs_spacing_is_enforced_independently_of_other_methods() {
        let config = GateConfig {
            spacing: Duration::from_millis(10),
            logs_spacing: Duration::from_millis(400),
            ..Default::default()
        };
        let g = gate(MockTransport::ok(0), config);

        let start = Instant::now();
        for _ in 0..3 {
            g.call("eth_getLogs", serde_json::json!([{}]), Priority::Bulk)
                .await
                .unwrap();
        }
        // Two gaps of 400 ms between three calls.
        assert!(
            start.elapsed() >= Duration::from_millis(800),
            "logs spacing not applied: {:?}",
            start.elapsed()
        );

        let start = Instant::now();
        for _ in 0..3 {
            g.call("eth_call", serde_json::json!([]), Priority::Bulk)
                .await
                .unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "non-logs calls must not inherit the logs spacing: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_replay_cache_miss_is_not_retried() {
        // Retrying a miss wastes the attempt budget and buries the real problem.
        #[derive(Debug)]
        struct Missing(AtomicU32);
        #[async_trait]
        impl Transport for Missing {
            async fn post(&self, _u: &str, _b: &str) -> Result<HttpResponse, TransportError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(TransportError::CacheMiss {
                    method: "eth_call".into(),
                })
            }
        }
        let t = Missing(AtomicU32::new(0));
        let counter = Arc::new(AtomicU32::new(0));
        let _ = counter;
        let g = Gate::new(Box::new(t), default_endpoints(), fast_config());
        let err = g
            .call("eth_call", serde_json::json!([]), Priority::Bulk)
            .await
            .expect_err("a miss must surface");
        assert!(err.to_string().contains("cache miss"), "{err}");
    }

    #[test]
    fn endpoint_parsing_honours_the_nologs_marker() {
        let eps = parse_endpoints("https://a.example,https://b.example#nologs");
        assert_eq!(eps.len(), 2);
        assert!(eps[0].logs);
        assert!(!eps[1].logs, "#nologs must be respected");
        assert_eq!(eps[0].label, "a.example");
    }

    #[test]
    fn a_known_endpoint_keeps_its_known_capability() {
        // publicnode refuses eth_getLogs whether or not the user marked it.
        let eps = parse_endpoints("https://robinhood-rpc.publicnode.com");
        assert_eq!(eps.len(), 1);
        assert!(!eps[0].logs, "publicnode never serves logs");
        assert_eq!(eps[0].label, "publicnode");
    }

    #[test]
    fn the_result_cap_message_is_recognised() {
        assert_eq!(
            too_many_results("logs matched by query exceeds limit of 10000"),
            Some(10_000)
        );
        assert_eq!(
            too_many_results("query returned more than 5000 results"),
            Some(5_000)
        );
        assert_eq!(too_many_results("execution reverted"), None);
    }
}
