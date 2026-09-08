//! `doctor` — check that the world still looks the way this binary assumes.
//!
//! Spec §2 says to verify every chain fact before relying on it, because factory
//! parameters can change and the numbers here were measured on one particular day. An
//! address baked into a binary that has since moved is a way to lose money quietly, so
//! this exists to make that loud instead.
//!
//! Without `--probe` it checks only what can be checked offline. With `--probe` it talks
//! to the configured endpoints and confirms each assumption against the live chain.

use alloy_primitives::{Address, U256};
use banana_core::curve::LaunchConfig;

use crate::abi::{IPonsCurve, IPonsFactory};
use crate::addr;
use crate::gate::Priority;
use crate::rpc::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    /// Different from what was recorded, but not necessarily broken. A changed factory
    /// parameter is a warning, not a failure: the chain is allowed to move, and the point
    /// is that the user finds out.
    Warn,
    Fail,
}

impl Status {
    pub fn glyph(self) -> &'static str {
        match self {
            Status::Pass => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn pass(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Pass,
            detail: detail.into(),
        }
    }
    fn warn(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
        }
    }
    fn expect<T: PartialEq + std::fmt::Display>(name: &str, got: T, want: T) -> Self {
        if got == want {
            Check::pass(name, format!("{got}"))
        } else {
            Check::warn(name, format!("got {got}, recorded {want}"))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn push(&mut self, c: Check) {
        self.checks.push(c);
    }

    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count()
    }

    pub fn ok(&self) -> bool {
        self.failures() == 0
    }

    /// A plain report, one line per check.
    pub fn render(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or(0)
            .max(4);
        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!(
                "  {}  {:width$}  {}\n",
                c.status.glyph(),
                c.name,
                c.detail
            ));
        }
        out.push_str(&format!(
            "\n{} checks, {} warnings, {} failures\n",
            self.checks.len(),
            self.warnings(),
            self.failures()
        ));
        out
    }
}

/// Checks that need no network.
pub fn offline() -> Report {
    let mut r = Report::default();
    r.push(Check::pass(
        "chain id (recorded)",
        addr::CHAIN_ID.to_string(),
    ));
    r.push(Check::pass(
        "block time (measured)",
        format!("{} ms", addr::BLOCK_MS),
    ));
    let c = LaunchConfig::live_id_0();
    r.push(Check::pass(
        "launch config 0 (recorded)",
        format!(
            "supply {}, fee {} bps, phantom {} wei, threshold {} wei",
            c.supply, c.curve_fee_bps, c.phantom_quote, c.graduation_threshold
        ),
    ));
    r
}

/// Everything `offline` does, plus live verification of every assumption.
pub async fn probe(client: &Client) -> Report {
    let mut r = Report::default();
    let p = Priority::Hot;

    // --- chain identity ---------------------------------------------------------------
    match client.chain_id(p).await {
        Ok(id) => {
            let c = Check::expect("chain id", id, addr::CHAIN_ID);
            // A wrong chain id is not a warning. Trading against the wrong chain with the
            // right-looking addresses is the worst outcome this tool can produce.
            r.push(if id == addr::CHAIN_ID {
                c
            } else {
                Check::fail("chain id", format!("got {id}, expected {}", addr::CHAIN_ID))
            });
        }
        Err(e) => r.push(Check::fail("chain id", e.to_string())),
    }

    match client.block_number(p).await {
        Ok(n) => r.push(Check::pass("head block", n.to_string())),
        Err(e) => r.push(Check::fail("head block", e.to_string())),
    }

    // --- every hardcoded address is still a contract ---------------------------------
    for (name, a) in [
        ("pons factory", addr::PONS_FACTORY),
        ("pons router", addr::PONS_ROUTER),
        ("fee escrow", addr::PONS_ESCROW),
        ("pons hook", addr::PONS_HOOK),
        ("multicall3", addr::MULTICALL3),
        ("v4 quoter", addr::V4_QUOTER),
        ("v4 state view", addr::V4_STATE_VIEW),
        ("v4 pool manager", addr::V4_POOL_MANAGER),
        ("universal router", addr::UNIVERSAL_ROUTER),
        ("permit2", addr::PERMIT2),
    ] {
        match client.has_code(a, p).await {
            Ok(0) => r.push(Check::fail(name, format!("{a} has NO code"))),
            Ok(n) => r.push(Check::pass(name, format!("{a}, {n} bytes"))),
            Err(e) => r.push(Check::fail(name, format!("{a}: {e}"))),
        }
    }

    // --- factory parameters, all in one multicall -------------------------------------
    //
    // Proving aggregate3 works is itself a check: it is the only batching the public RPC
    // accepts, and the enrichment path depends on it.
    let f = addr::PONS_FACTORY;
    let bundle = vec![
        (
            f,
            IPonsFactory::snipeTaxStartBpsCall {}.abi_encode_checked(),
        ),
        (f, IPonsFactory::snipeTaxSecondsCall {}.abi_encode_checked()),
        (f, IPonsFactory::launchFeeCall {}.abi_encode_checked()),
        (
            f,
            IPonsFactory::maxCreatorTaxBpsCall {}.abi_encode_checked(),
        ),
        (f, IPonsFactory::launchEnabledCall {}.abi_encode_checked()),
        (f, IPonsFactory::feeEscrowCall {}.abi_encode_checked()),
        (f, IPonsFactory::memeHookCall {}.abi_encode_checked()),
    ];
    match client.multicall(bundle, p).await {
        Err(e) => r.push(Check::fail("multicall3 aggregate3", e.to_string())),
        Ok(res) => {
            r.push(Check::pass(
                "multicall3 aggregate3",
                format!("{} results in one eth_call", res.len()),
            ));

            if let Some(v) = res
                .first()
                .and_then(|c| c.decode::<IPonsFactory::snipeTaxStartBpsCall>())
            {
                // Spec §2: launches open behind a 99% tax.
                r.push(Check::expect("snipe tax start", v, U256::from(9_900)));
            }
            if let Some(v) = res
                .get(1)
                .and_then(|c| c.decode::<IPonsFactory::snipeTaxSecondsCall>())
            {
                // Spec §2: it decays to zero over 3 seconds. This is the number the whole
                // entry model is built on.
                r.push(Check::expect("snipe tax seconds", v, U256::from(3)));
            }
            if let Some(v) = res
                .get(2)
                .and_then(|c| c.decode::<IPonsFactory::launchFeeCall>())
            {
                r.push(Check::expect(
                    "launch fee",
                    v,
                    U256::from(500_000_000_000_000u64),
                ));
            }
            if let Some(v) = res
                .get(3)
                .and_then(|c| c.decode::<IPonsFactory::maxCreatorTaxBpsCall>())
            {
                r.push(Check::pass("max creator tax", format!("{v} bps")));
            }
            if let Some(v) = res
                .get(4)
                .and_then(|c| c.decode::<IPonsFactory::launchEnabledCall>())
            {
                r.push(if v {
                    Check::pass("launches enabled", "true")
                } else {
                    Check::warn("launches enabled", "false: the factory is not launching")
                });
            }
            if let Some(v) = res
                .get(5)
                .and_then(|c| c.decode::<IPonsFactory::feeEscrowCall>())
            {
                r.push(addr_check("fee escrow matches", v, addr::PONS_ESCROW));
            }
            if let Some(v) = res
                .get(6)
                .and_then(|c| c.decode::<IPonsFactory::memeHookCall>())
            {
                r.push(addr_check("meme hook matches", v, addr::PONS_HOOK));
            }
        }
    }

    // --- launch config 0 against what core hardcodes ----------------------------------
    match client
        .call(f, &IPonsFactory::getLaunchConfigCall { id: U256::ZERO }, p)
        .await
    {
        Err(e) => r.push(Check::fail("launch config 0", e.to_string())),
        Ok(cfg) => {
            let want = LaunchConfig::live_id_0();
            let mismatches = [
                ("supply", cfg.supply, want.supply),
                ("phantom quote", cfg.phantomQuote, want.phantom_quote),
                (
                    "graduation threshold",
                    cfg.graduationThreshold,
                    want.graduation_threshold,
                ),
                (
                    "curve fee bps",
                    cfg.curveFeeBps,
                    U256::from(want.curve_fee_bps),
                ),
            ]
            .into_iter()
            .filter(|(_, got, want)| got != want)
            .map(|(n, got, want)| format!("{n}: got {got}, recorded {want}"))
            .collect::<Vec<_>>();

            if mismatches.is_empty() {
                r.push(Check::pass(
                    "launch config 0",
                    "supply, phantom, threshold and fee all match the recorded values",
                ));
            } else {
                // The curve math is calibrated to these. If they move, every quote and
                // every replayed entry price moves with them.
                r.push(Check::warn("launch config 0", mismatches.join("; ")));
            }
            r.push(if cfg.enabled {
                Check::pass("launch config 0 enabled", "true")
            } else {
                Check::warn("launch config 0 enabled", "false")
            });
        }
    }

    // --- endpoint capability ----------------------------------------------------------
    for e in client.gate().stats().endpoints {
        r.push(Check::pass(
            &format!("endpoint {}", e.label),
            format!(
                "logs: {}{}",
                if e.logs { "yes" } else { "no" },
                if e.benched { ", BENCHED" } else { "" }
            ),
        ));
    }

    r
}

/// Probe a single curve, to confirm the curve ABI still matches a deployed contract.
pub async fn probe_curve(client: &Client, curve: Address) -> Report {
    let mut r = Report::default();
    let p = Priority::Hot;
    let bundle = vec![
        (curve, IPonsCurve::getReservesCall {}.abi_encode_checked()),
        (curve, IPonsCurve::feeBpsCall {}.abi_encode_checked()),
        (curve, IPonsCurve::creatorTaxBpsCall {}.abi_encode_checked()),
        (curve, IPonsCurve::graduatedCall {}.abi_encode_checked()),
        (
            curve,
            IPonsCurve::currentSnipeTaxBpsCall {
                recipient: addr::DEAD,
            }
            .abi_encode_checked(),
        ),
    ];
    match client.multicall(bundle, p).await {
        Err(e) => r.push(Check::fail("curve probe", e.to_string())),
        Ok(res) => {
            match res
                .first()
                .and_then(|c| c.decode::<IPonsCurve::getReservesCall>())
            {
                Some(rs) => r.push(Check::pass(
                    "curve reserves",
                    format!("quote {}, token {}", rs.quoteReserve, rs.tokenReserve),
                )),
                None => r.push(Check::fail("curve reserves", "getReserves did not decode")),
            }
            if let Some(v) = res
                .get(1)
                .and_then(|c| c.decode::<IPonsCurve::feeBpsCall>())
            {
                r.push(Check::expect("curve fee", v, U256::from(100)));
            }
            if let Some(v) = res
                .get(4)
                .and_then(|c| c.decode::<IPonsCurve::currentSnipeTaxBpsCall>())
            {
                r.push(Check::pass("current snipe tax", format!("{v} bps")));
            }
        }
    }
    r
}

fn addr_check(name: &str, got: Address, want: Address) -> Check {
    if got == want {
        Check::pass(name, got.to_string())
    } else {
        Check::warn(name, format!("factory says {got}, recorded {want}"))
    }
}

/// `abi_encode` for a call with no arguments, as a named helper so the bundles above read
/// cleanly.
trait EncodeChecked {
    fn abi_encode_checked(&self) -> Vec<u8>;
}

impl<T: alloy_sol_types::SolCall> EncodeChecked for T {
    fn abi_encode_checked(&self) -> Vec<u8> {
        self.abi_encode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_checks_need_no_network() {
        let r = offline();
        assert!(r.ok());
        assert!(r.checks.len() >= 3);
    }

    #[test]
    fn a_report_renders_one_line_per_check() {
        let mut r = Report::default();
        r.push(Check::pass("a", "fine"));
        r.push(Check::warn("bb", "moved"));
        r.push(Check::fail("ccc", "gone"));
        let out = r.render();
        assert_eq!(out.lines().filter(|l| l.starts_with("  ")).count(), 3);
        assert!(out.contains("3 checks, 1 warnings, 1 failures"));
        assert!(!r.ok(), "a failure must make the report not ok");
    }

    #[test]
    fn a_changed_parameter_warns_but_a_wrong_chain_fails() {
        // The distinction matters: the chain is allowed to change its parameters, and the
        // point of doctor is that the user finds out. Being on the wrong chain entirely is
        // a different category.
        assert_eq!(Check::expect("x", 1, 2).status, Status::Warn);
        assert_eq!(Check::expect("x", 2, 2).status, Status::Pass);
        assert_eq!(Check::fail("chain id", "wrong").status, Status::Fail);
    }

    #[test]
    fn warnings_alone_do_not_make_a_report_fail() {
        let mut r = Report::default();
        r.push(Check::warn("param", "moved"));
        assert!(r.ok());
        assert_eq!(r.warnings(), 1);
    }
}
