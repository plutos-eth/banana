//! Spec §7.3: the walls between a bad hour and a drained wallet.
//!
//! Every one of these is enforced here, in code, and individually tested. They are not
//! advisory, the UI cannot relax them past what `LiveGuards` sets, and there is no path
//! that spends money without passing through [`Budget::authorise`].
//!
//! # Why this is a type and not a set of `if`s
//!
//! An `if` at each call site is a guard you can forget at the next call site. [`Budget`]
//! owns the running totals, and the only way to get a [`Spend`] — the token
//! [`crate::exec`] requires before it will build a buy — is to ask it. A new entry path
//! that forgets the guards does not compile, because it has nothing to hand the executor.
//!
//! # Integers only
//!
//! Every figure is `U256` wei or an integer count. There is no floating point anywhere in
//! this file: a rounding error here is money (spec §12).

use alloy_primitives::{Address, U256};
use quarrel_core::strategy::LiveGuards;
use serde::{Deserialize, Serialize};

/// Why an entry was refused by the money guards, in the user's words.
///
/// Spec §3.4 wants the specific rule and the specific values, and a refusal to spend is
/// the one a user most needs to understand: "nothing fired all afternoon" has to be
/// answerable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "guard", rename_all = "snake_case")]
pub enum Refused {
    #[error("not armed: the session has not been armed for live trading")]
    NotArmed,
    #[error("size {requested} wei exceeds the per-buy cap of {cap} wei")]
    SizePerBuy { requested: U256, cap: U256 },
    #[error(
        "position cap: this token already holds {held} wei and {requested} more would pass \
         the {cap} wei limit"
    )]
    PositionCap {
        held: U256,
        requested: U256,
        cap: U256,
    },
    #[error(
        "session budget: {spent} wei of {budget} spent, and {requested} more would pass it. \
         Nothing fires now regardless of signal"
    )]
    SessionBudget {
        spent: U256,
        requested: U256,
        budget: U256,
    },
    #[error("already holding {open} positions, the maximum is {cap}")]
    MaxOpenPositions { open: u32, cap: u32 },
    #[error("wallet holds {balance} wei, which is less than the {requested} wei requested")]
    InsufficientBalance { balance: U256, requested: U256 },
}

/// Permission to spend exactly this much on exactly this token, once.
///
/// Produced only by [`Budget::authorise`] and consumed by the executor. It is deliberately
/// **not** `Copy` and **not** `Clone`: a permission that could be duplicated would let one
/// authorisation fund two buys, which is the whole failure the session budget exists to
/// prevent.
#[derive(Debug, PartialEq, Eq)]
pub struct Spend {
    token: Address,
    wei: U256,
}

impl Spend {
    pub fn token(&self) -> Address {
        self.token
    }

    pub fn wei(&self) -> U256 {
        self.wei
    }
}

/// One open position, as the guards need to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub token: Address,
    /// What has gone into this position so far, across adds.
    pub cost_wei: U256,
}

/// The running state of a trading session.
#[derive(Debug, Clone)]
pub struct Budget {
    limits: LiveGuards,
    armed: bool,
    spent: U256,
    open: Vec<Held>,
}

impl Budget {
    /// A session that cannot spend until it is armed (spec §3.2, §7.3).
    pub fn new(limits: LiveGuards) -> Self {
        Self {
            limits,
            armed: false,
            spent: U256::ZERO,
            open: Vec::new(),
        }
    }

    /// Arm the session. Only [`crate::arm`] should call this, and only after the user has
    /// seen the address, the balance and the limits and typed the phrase.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn spent(&self) -> U256 {
        self.spent
    }

    pub fn remaining(&self) -> U256 {
        self.limits.session_budget_wei.saturating_sub(self.spent)
    }

    pub fn open(&self) -> &[Held] {
        &self.open
    }

    pub fn limits(&self) -> &LiveGuards {
        &self.limits
    }

    /// Check every guard, in the order a user would ask about them.
    ///
    /// `balance` is the wallet's own balance; it is checked last because it is the one
    /// that is not a policy but a fact, and a user reading a refusal wants to know which
    /// of their own limits stopped them before they wonder whether they are broke.
    pub fn authorise(
        &self,
        token: Address,
        wei: U256,
        balance: U256,
    ) -> Result<Spend, Box<Refused>> {
        if !self.armed {
            return Err(Box::new(Refused::NotArmed));
        }
        if wei > self.limits.size_per_buy_wei {
            return Err(Box::new(Refused::SizePerBuy {
                requested: wei,
                cap: self.limits.size_per_buy_wei,
            }));
        }

        let held = self.cost_of(token);
        // Saturating rather than checked: an overflow here would have to come from a
        // config with a U256::MAX cap, and treating that as "no room" is the safe way to
        // be wrong.
        if held.saturating_add(wei) > self.limits.position_cap_wei {
            return Err(Box::new(Refused::PositionCap {
                held,
                requested: wei,
                cap: self.limits.position_cap_wei,
            }));
        }
        if self.spent.saturating_add(wei) > self.limits.session_budget_wei {
            return Err(Box::new(Refused::SessionBudget {
                spent: self.spent,
                requested: wei,
                budget: self.limits.session_budget_wei,
            }));
        }
        // Opening a NEW position is what the count limits; adding to one already open is
        // governed by the position cap instead.
        if held.is_zero() && self.open.len() as u32 >= self.limits.max_open_positions {
            return Err(Box::new(Refused::MaxOpenPositions {
                open: self.open.len() as u32,
                cap: self.limits.max_open_positions,
            }));
        }
        if wei > balance {
            return Err(Box::new(Refused::InsufficientBalance {
                balance,
                requested: wei,
            }));
        }
        Ok(Spend { token, wei })
    }

    /// Record that an authorised spend actually happened.
    ///
    /// Takes the `Spend` by value, so a permission cannot be committed twice.
    pub fn commit(&mut self, spend: Spend) {
        self.spent = self.spent.saturating_add(spend.wei);
        match self.open.iter_mut().find(|h| h.token == spend.token) {
            Some(h) => h.cost_wei = h.cost_wei.saturating_add(spend.wei),
            None => self.open.push(Held {
                token: spend.token,
                cost_wei: spend.wei,
            }),
        }
    }

    /// Close a position. The session budget is **not** refunded.
    ///
    /// Deliberate: the budget limits how much is put at risk over a session, not how much
    /// is outstanding at any moment. Refunding it on a sale would let a losing session
    /// churn indefinitely, which is exactly the afternoon §7.3 exists to end.
    pub fn close(&mut self, token: Address) {
        self.open.retain(|h| h.token != token);
    }

    fn cost_of(&self, token: Address) -> U256 {
        self.open
            .iter()
            .find(|h| h.token == token)
            .map(|h| h.cost_wei)
            .unwrap_or(U256::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_ETH: u64 = 1_000_000_000_000_000_000;

    fn wei(hundredths: u64) -> U256 {
        U256::from(ONE_ETH / 100 * hundredths)
    }

    fn token(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    /// The defaults of §7.3: 0.01 per buy, 0.01 position cap, 0.05 session, 3 open.
    fn armed() -> Budget {
        let mut b = Budget::new(LiveGuards::default());
        b.arm();
        b
    }

    #[test]
    fn a_session_cannot_spend_until_it_is_armed() {
        let b = Budget::new(LiveGuards::default());
        assert!(!b.is_armed());
        assert_eq!(
            *b.authorise(token(1), wei(1), wei(1000)).unwrap_err(),
            Refused::NotArmed
        );
    }

    #[test]
    fn an_armed_session_authorises_a_buy_within_every_limit() {
        let b = armed();
        let s = b.authorise(token(1), wei(1), wei(1000)).unwrap();
        assert_eq!(s.wei(), wei(1));
        assert_eq!(s.token(), token(1));
    }

    // --- guard 1: size per buy ------------------------------------------------------

    #[test]
    fn a_buy_larger_than_the_per_buy_cap_is_refused_with_both_numbers() {
        let b = armed();
        let e = *b.authorise(token(1), wei(2), wei(1000)).unwrap_err();
        assert!(matches!(e, Refused::SizePerBuy { .. }));
        // Spec §3.4: the refusal names the value and the threshold.
        let msg = e.to_string();
        assert!(msg.contains("20000000000000000"), "{msg}");
        assert!(msg.contains("10000000000000000"), "{msg}");
    }

    #[test]
    fn a_buy_exactly_at_the_cap_is_allowed() {
        // The boundary matters: a guard that refuses its own documented default would
        // make the shipped configuration unusable.
        assert!(armed().authorise(token(1), wei(1), wei(1000)).is_ok());
    }

    // --- guard 2: position cap ------------------------------------------------------

    #[test]
    fn adding_to_a_position_past_its_cap_is_refused() {
        let mut b = armed();
        let s = b.authorise(token(1), wei(1), wei(1000)).unwrap();
        b.commit(s);
        // The position cap is 0.01 and the position already holds 0.01.
        let e = *b.authorise(token(1), wei(1), wei(1000)).unwrap_err();
        assert!(matches!(e, Refused::PositionCap { .. }), "{e}");
    }

    #[test]
    fn the_position_cap_counts_adds_not_just_the_first_buy() {
        let mut b = Budget::new(LiveGuards {
            size_per_buy_wei: wei(1),
            position_cap_wei: wei(3),
            ..LiveGuards::default()
        });
        b.arm();
        for _ in 0..3 {
            let s = b.authorise(token(1), wei(1), wei(1000)).unwrap();
            b.commit(s);
        }
        let e = *b.authorise(token(1), wei(1), wei(1000)).unwrap_err();
        assert!(matches!(e, Refused::PositionCap { .. }), "{e}");
    }

    // --- guard 3: session budget ----------------------------------------------------

    #[test]
    fn once_the_session_budget_is_spent_nothing_fires_regardless_of_signal() {
        let mut b = armed();
        // Five buys of 0.01 exhaust the 0.05 default, across five different tokens so
        // neither the position cap nor the open count is what stops it.
        let mut b = {
            b.limits.max_open_positions = 99;
            b
        };
        for i in 1..=5 {
            let s = b.authorise(token(i), wei(1), wei(1000)).unwrap();
            b.commit(s);
        }
        assert_eq!(b.remaining(), U256::ZERO);

        let e = *b.authorise(token(9), wei(1), wei(1000)).unwrap_err();
        assert!(matches!(e, Refused::SessionBudget { .. }), "{e}");
        assert!(e.to_string().contains("regardless of signal"));
    }

    #[test]
    fn selling_does_not_refund_the_session_budget() {
        // The budget limits what a session risks, not what it holds. Refunding on a sale
        // would let a losing afternoon churn forever.
        let mut b = armed();
        let s = b.authorise(token(1), wei(1), wei(1000)).unwrap();
        b.commit(s);
        let after_buy = b.remaining();
        b.close(token(1));
        assert_eq!(b.remaining(), after_buy, "closing must not restore budget");
        assert!(b.open().is_empty());
    }

    // --- guard 4: max open positions ------------------------------------------------

    #[test]
    fn a_fourth_position_is_refused_at_the_default_of_three() {
        let mut b = armed();
        for i in 1..=3 {
            let s = b.authorise(token(i), wei(1), wei(1000)).unwrap();
            b.commit(s);
        }
        let e = *b.authorise(token(4), wei(1), wei(1000)).unwrap_err();
        assert!(matches!(e, Refused::MaxOpenPositions { open: 3, cap: 3 }));
    }

    #[test]
    fn adding_to_an_open_position_is_not_a_new_position() {
        let mut b = Budget::new(LiveGuards {
            size_per_buy_wei: wei(1),
            position_cap_wei: wei(2),
            max_open_positions: 1,
            ..LiveGuards::default()
        });
        b.arm();
        let s = b.authorise(token(1), wei(1), wei(1000)).unwrap();
        b.commit(s);
        // At the position limit, but this is the same token, so the count does not apply.
        assert!(b.authorise(token(1), wei(1), wei(1000)).is_ok());
        // A different token is a new position and is refused.
        assert!(matches!(
            *b.authorise(token(2), wei(1), wei(1000)).unwrap_err(),
            Refused::MaxOpenPositions { .. }
        ));
    }

    #[test]
    fn closing_a_position_frees_a_slot() {
        let mut b = armed();
        for i in 1..=3 {
            let s = b.authorise(token(i), wei(1), wei(1000)).unwrap();
            b.commit(s);
        }
        b.close(token(2));
        assert!(b.authorise(token(4), wei(1), wei(1000)).is_ok());
    }

    // --- guard 5: the wallet's own balance ------------------------------------------

    #[test]
    fn a_buy_larger_than_the_balance_is_refused() {
        let b = armed();
        let e = *b.authorise(token(1), wei(1), U256::ZERO).unwrap_err();
        assert!(matches!(e, Refused::InsufficientBalance { .. }), "{e}");
    }

    #[test]
    fn a_policy_limit_is_reported_before_an_empty_wallet() {
        // Both would refuse. The user's own configuration is the more useful answer,
        // because it is the one they can change.
        let b = armed();
        let e = *b.authorise(token(1), wei(50), U256::ZERO).unwrap_err();
        assert!(matches!(e, Refused::SizePerBuy { .. }), "{e}");
    }

    // --- the permission itself ------------------------------------------------------

    #[test]
    fn a_spend_cannot_be_committed_twice() {
        // Enforced by the type: `commit` takes the `Spend` by value and `Spend` is not
        // `Clone`, so there is no way to spend one authorisation on two buys. This test
        // documents the property; the compiler is what enforces it.
        let mut b = armed();
        let s = b.authorise(token(1), wei(1), wei(1000)).unwrap();
        b.commit(s);
        // `b.commit(s)` here would not compile: `s` has been moved.
        assert_eq!(b.spent(), wei(1));
    }

    #[test]
    fn the_shipped_defaults_are_the_documented_ones() {
        let g = LiveGuards::default();
        assert_eq!(g.session_budget_wei, wei(5), "0.05 ETH");
        assert_eq!(g.size_per_buy_wei, wei(1), "0.01 ETH");
        assert_eq!(g.position_cap_wei, wei(1), "0.01 ETH");
        assert_eq!(g.max_open_positions, 3);
    }

    #[test]
    fn every_refusal_names_a_number_the_user_can_act_on() {
        let cases = [
            Refused::NotArmed,
            Refused::SizePerBuy {
                requested: wei(2),
                cap: wei(1),
            },
            Refused::PositionCap {
                held: wei(1),
                requested: wei(1),
                cap: wei(1),
            },
            Refused::SessionBudget {
                spent: wei(5),
                requested: wei(1),
                budget: wei(5),
            },
            Refused::MaxOpenPositions { open: 3, cap: 3 },
            Refused::InsufficientBalance {
                balance: U256::ZERO,
                requested: wei(1),
            },
        ];
        for c in cases {
            let s = c.to_string();
            assert!(!s.is_empty());
            // Every one but "not armed" quotes at least one figure.
            if !matches!(c, Refused::NotArmed) {
                assert!(
                    s.chars().any(|ch| ch.is_ascii_digit()),
                    "a refusal with no number in it: {s}"
                );
            }
        }
    }
}
