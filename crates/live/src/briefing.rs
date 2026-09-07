//! What a user is shown before choosing live mode.
//!
//! The wallet, its balance, and every limit that will be enforced. This is the substance
//! of what spec §7.3 called an "arm phrase": the word itself was ceremony, but the
//! information is not — nobody should start trading without seeing which wallet is about
//! to be spent from and how much of it is at risk.
//!
//! The briefing is built here rather than in the UI so there is one of it, and so what the
//! user confirms is what the guards will actually enforce: [`Briefing`] renders from the
//! same `LiveGuards` value that [`crate::Budget`] is constructed with, never from a second
//! copy of the numbers.
//!
//! # Mode is chosen once, at startup
//!
//! The user picks TEST or LIVE when the application opens, and the choice holds for the
//! life of the process — changing it means restarting. A running session cannot drift from
//! one to the other, which is the property that actually matters. The previous design
//! reached it with a launch flag plus a typed word; this reaches it with one deliberate
//! choice and no jargon.

use alloy_primitives::{Address, U256};
use quarrel_core::strategy::LiveGuards;

/// What the user is shown before choosing live mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Briefing {
    pub address: Address,
    pub balance_wei: U256,
    pub chain_id: u64,
    pub guards: LiveGuards,
}

impl Briefing {
    /// Render as the block of text the user reads before typing.
    ///
    /// Every figure the guards will enforce appears here, in the units the user set them
    /// in. A briefing that showed rounded ETH and enforced wei would be a briefing about
    /// a different session than the one about to run.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("LIVE TRADING — real money moves after this point.\n\n");
        s.push_str(&format!("  chain            {}\n", self.chain_id));
        s.push_str(&format!("  wallet           {:#x}\n", self.address));
        s.push_str(&format!(
            "  balance          {} wei ({})\n",
            self.balance_wei,
            eth(self.balance_wei)
        ));
        s.push_str("\n  limits that will be enforced:\n");
        s.push_str(&format!(
            "  size per buy     {} wei ({})\n",
            self.guards.size_per_buy_wei,
            eth(self.guards.size_per_buy_wei)
        ));
        s.push_str(&format!(
            "  position cap     {} wei ({})\n",
            self.guards.position_cap_wei,
            eth(self.guards.position_cap_wei)
        ));
        s.push_str(&format!(
            "  session budget   {} wei ({}) — after this nothing fires, whatever the signal\n",
            self.guards.session_budget_wei,
            eth(self.guards.session_budget_wei)
        ));
        s.push_str(&format!(
            "  max open         {}\n",
            self.guards.max_open_positions
        ));
        s.push_str(&format!(
            "\nMost this session can spend: {} ({}).\n",
            self.guards.session_budget_wei,
            eth(self.guards.session_budget_wei)
        ));
        s.push_str(
            "\nChoosing LIVE starts trading against these limits. \
             TEST runs everything except signing.\n",
        );
        s
    }

    /// True when the wallet cannot fund even one buy at the configured size.
    ///
    /// Worth saying on the choice screen rather than after the first refusal: starting a
    /// live session that can never fire is a worse experience than being told why.
    pub fn cannot_fund_a_single_buy(&self) -> bool {
        self.balance_wei < self.guards.size_per_buy_wei
    }
}

/// Wei as ETH, to four decimal places, without floating point.
///
/// Enough digits to recognise a figure and not enough to imply a precision that the
/// rounding does not have. The wei value is always printed beside it, and that is the
/// number the guards use (spec §12).
fn eth(wei: U256) -> String {
    let unit = U256::from(1_000_000_000_000_000_000u64);
    let whole = wei / unit;
    // Four decimal places: (wei % 1e18) / 1e14.
    let frac = (wei % unit) / U256::from(100_000_000_000_000u64);
    format!("{whole}.{frac:0>4} ETH")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn briefing() -> Briefing {
        Briefing {
            address: "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse()
                .unwrap(),
            balance_wei: U256::from(1_234_500_000_000_000_000u64),
            chain_id: 4663,
            guards: LiveGuards::default(),
        }
    }

    #[test]
    fn the_briefing_shows_every_limit_that_will_be_enforced() {
        let b = briefing();
        let text = b.render();
        // Spec §7.3 names four; all four have to be on screen before the phrase.
        for needed in ["size per buy", "position cap", "session budget", "max open"] {
            assert!(
                text.contains(needed),
                "briefing is missing {needed}:\n{text}"
            );
        }
        assert!(text.contains("LIVE TRADING"));
        assert!(text.contains("real money"));
        assert!(text.contains("TEST runs everything except signing"));
    }

    #[test]
    fn the_briefing_shows_the_wallet_and_its_balance() {
        let text = briefing().render();
        assert!(
            text.contains("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"),
            "{text}"
        );
        assert!(
            text.contains("1234500000000000000"),
            "the exact wei: {text}"
        );
        assert!(text.contains("1.2345 ETH"), "and a readable form: {text}");
    }

    #[test]
    fn the_briefing_states_the_worst_case_before_the_phrase() {
        // The one number a user most needs: the most this session can lose.
        let text = briefing().render();
        assert!(text.contains("Most this session can spend"), "{text}");
        assert!(text.contains("0.0500 ETH"), "{text}");
        assert!(text.contains("whatever the signal"), "{text}");
    }

    #[test]
    fn the_briefing_is_built_from_the_same_guards_the_budget_enforces() {
        // Not a second copy of the numbers: the briefing and `Budget` take one value.
        let guards = LiveGuards {
            size_per_buy_wei: U256::from(7u64),
            position_cap_wei: U256::from(11u64),
            session_budget_wei: U256::from(13u64),
            max_open_positions: 9,
        };
        let b = Briefing {
            guards: guards.clone(),
            ..briefing()
        };
        let text = b.render();
        assert!(text.contains(" 7 wei"));
        assert!(text.contains(" 11 wei"));
        assert!(text.contains(" 13 wei"));
        assert!(text.contains(" 9\n"));

        let budget = crate::Budget::new(guards.clone());
        assert_eq!(
            budget.limits().session_budget_wei,
            guards.session_budget_wei
        );
    }

    #[test]
    fn a_wallet_too_empty_to_buy_is_flagged_before_the_choice() {
        let b = Briefing {
            balance_wei: U256::from(1u64),
            ..briefing()
        };
        assert!(b.cannot_fund_a_single_buy());
        assert!(!briefing().cannot_fund_a_single_buy());
    }

    #[test]
    fn eth_rendering_is_integer_arithmetic_and_does_not_round_up() {
        let unit = 1_000_000_000_000_000_000u64;
        assert_eq!(eth(U256::ZERO), "0.0000 ETH");
        assert_eq!(eth(U256::from(unit)), "1.0000 ETH");
        assert_eq!(eth(U256::from(unit / 100)), "0.0100 ETH");
        assert_eq!(eth(U256::from(unit / 20)), "0.0500 ETH");
        // Truncation, never rounding up: a balance must not read as more than it is.
        assert_eq!(eth(U256::from(unit - 1)), "0.9999 ETH");
        assert_eq!(eth(U256::from(1u64)), "0.0000 ETH");
    }
}
