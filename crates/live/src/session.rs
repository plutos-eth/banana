//! A trading session: the mode, the key, and the budget, as one thing.
//!
//! Spec §3.2: "Dry run by default. Real money moves only behind an explicit `--live`
//! launch flag."
//!
//! The way that is made true here is not a boolean anybody checks. A [`Session`] owns a
//! `Box<dyn Signer>`, and a dry-run session owns a [`NoSigner`] — which has no key and
//! returns an error from `sign`. There is no code path that spends money without a
//! signature, so a dry-run session cannot spend money even if every other check in the
//! program were removed.
//!
//! [`Session::live`] is the only constructor that reads a key, and it takes proof that the
//! user typed the arm phrase. That proof is a value ([`Armed`]) that can only be made by
//! [`crate::arm`] agreeing the phrase matched, so "armed" is not a flag that could be set
//! by mistake somewhere else.

use alloy_primitives::{Address, U256};
use quarrel_core::strategy::LiveGuards;

use crate::arm::{Briefing, phrase_arms};
use crate::guards::{Budget, Refused, Spend};
use crate::signer::{EnvSigner, NoSigner, SignedTx, Signer, SignerError, TxRequest};

/// Whether real money can move (spec §3.2). Displayed on every surface, always.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    DryRun,
    Live,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::DryRun => "DRY RUN",
            Mode::Live => "LIVE",
        }
    }
}

/// Proof that a human typed the arm phrase after seeing the briefing.
///
/// The only way to construct one is [`Armed::from_input`], which checks the phrase. It is
/// not `Clone` and not `Default`: an arming cannot be copied to a second session or
/// conjured by a struct literal somewhere else in the codebase.
#[derive(Debug)]
pub struct Armed(());

impl Armed {
    /// Check what the user typed against the briefing they were shown.
    ///
    /// Returns `None` when the phrase does not match, which is the same as declining.
    pub fn from_input(_briefing: &Briefing, typed: &str) -> Option<Self> {
        phrase_arms(typed).then_some(Armed(()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Signer(#[from] SignerError),
    #[error("{0}")]
    Refused(Box<Refused>),
}

/// One run of the sniper.
#[derive(Debug)]
pub struct Session {
    mode: Mode,
    signer: Box<dyn Signer>,
    budget: Budget,
}

impl Session {
    /// The default. Everything runs; nothing can be signed.
    pub fn dry_run(limits: LiveGuards) -> Self {
        let mut budget = Budget::new(limits);
        // A dry run is armed so the rest of the pipeline exercises the same path a live
        // session takes -- the guards, the refusals, the journal. What it cannot do is
        // sign, and that is the difference that matters.
        budget.arm();
        Self {
            mode: Mode::DryRun,
            signer: Box::new(NoSigner),
            budget,
        }
    }

    /// A session that can spend. Requires the `--live` flag *and* the arm phrase.
    ///
    /// The `Armed` argument is not decoration: it is the only way to get one, and it can
    /// only be got by showing a user a briefing and having them type a word.
    pub fn live(limits: LiveGuards, _armed: Armed) -> Result<Self, SessionError> {
        let signer = EnvSigner::from_env()?;
        let mut budget = Budget::new(limits);
        budget.arm();
        Ok(Self {
            mode: Mode::Live,
            signer: Box::new(signer),
            budget,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn address(&self) -> Address {
        self.signer.address()
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Build the briefing a user sees before arming.
    pub fn briefing(&self, balance_wei: U256, chain_id: u64) -> Briefing {
        Briefing {
            address: self.address(),
            balance_wei,
            chain_id,
            guards: self.budget.limits().clone(),
        }
    }

    /// Ask the guards for permission to spend.
    pub fn authorise(
        &self,
        token: Address,
        wei: U256,
        balance: U256,
    ) -> Result<Spend, SessionError> {
        self.budget
            .authorise(token, wei, balance)
            .map_err(SessionError::Refused)
    }

    /// Sign an authorised transaction, consuming the permission.
    ///
    /// In dry run this always fails, because the session holds no key. That is the
    /// invariant: not "we check a flag before signing", but "there is nothing to sign
    /// with". The `Spend` is consumed either way, so a refused signature does not leave a
    /// live permission behind.
    pub fn sign(&mut self, spend: Spend, tx: &TxRequest) -> Result<SignedTx, SessionError> {
        let signed = self.signer.sign(tx)?;
        self.budget.commit(spend);
        Ok(signed)
    }

    /// Record a spend that did not need signing, for the dry-run journal.
    ///
    /// Keeps the guards' accounting honest in dry run: a simulated session that never
    /// consumed its budget would report that a strategy fits inside limits it would in
    /// fact have blown through.
    pub fn commit_simulated(&mut self, spend: Spend) {
        debug_assert_eq!(self.mode, Mode::DryRun, "live spends go through sign()");
        self.budget.commit(spend);
    }

    pub fn close(&mut self, token: Address) {
        self.budget.close(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn wei(hundredths: u64) -> U256 {
        U256::from(10_000_000_000_000_000u64 * hundredths)
    }

    fn tx() -> TxRequest {
        TxRequest {
            chain_id: 4663,
            nonce: 0,
            to: addr(1),
            value: wei(1),
            data: Default::default(),
            gas_limit: 300_000,
            max_fee_per_gas: 100_000_000,
            max_priority_fee_per_gas: 0,
        }
    }

    #[test]
    fn the_default_session_is_dry_run_and_holds_no_key() {
        let s = Session::dry_run(LiveGuards::default());
        assert_eq!(s.mode(), Mode::DryRun);
        assert_eq!(s.mode().label(), "DRY RUN");
        assert_eq!(s.address(), Address::ZERO, "no key, no address");
    }

    /// The invariant, stated as a test: a dry run cannot spend money.
    #[test]
    fn a_dry_run_session_cannot_sign_even_with_a_valid_authorisation() {
        let mut s = Session::dry_run(LiveGuards::default());
        // The guards are perfectly happy.
        let spend = s.authorise(addr(1), wei(1), wei(1000)).unwrap();
        // And it still cannot spend, because there is nothing to sign with.
        let e = s.sign(spend, &tx()).unwrap_err();
        assert!(matches!(e, SessionError::Signer(SignerError::NoKey)));
        assert_eq!(s.budget().spent(), U256::ZERO, "and nothing was recorded");
    }

    #[test]
    fn the_arm_phrase_is_the_only_way_to_get_the_proof_a_live_session_needs() {
        let b = Briefing {
            address: addr(3),
            balance_wei: wei(100),
            chain_id: 4663,
            guards: LiveGuards::default(),
        };
        assert!(Armed::from_input(&b, "arm").is_some());
        for typed in ["", "y", "yes", "ARM", "arm now"] {
            assert!(
                Armed::from_input(&b, typed).is_none(),
                "{typed:?} produced an arming"
            );
        }
    }

    #[test]
    fn a_live_session_without_a_key_fails_rather_than_falling_back_to_dry_run() {
        // Silently downgrading would be the worst of both: the user believes they are
        // live and no orders fire, or they believe they are safe and later a key appears.
        if std::env::var("PRIVATE_KEY").is_ok() {
            return; // a configured machine cannot exercise this
        }
        let b = Briefing {
            address: Address::ZERO,
            balance_wei: wei(100),
            chain_id: 4663,
            guards: LiveGuards::default(),
        };
        let armed = Armed::from_input(&b, "arm").unwrap();
        let e = Session::live(LiveGuards::default(), armed).unwrap_err();
        assert!(matches!(e, SessionError::Signer(SignerError::NoKey)));
    }

    #[test]
    fn the_guards_still_refuse_in_dry_run() {
        // A dry run that ignored the guards would report that a strategy fits inside
        // limits it would have blown through.
        let s = Session::dry_run(LiveGuards::default());
        let e = s.authorise(addr(1), wei(50), wei(1000)).unwrap_err();
        assert!(matches!(e, SessionError::Refused(_)));
        assert!(e.to_string().contains("per-buy cap"));
    }

    #[test]
    fn a_simulated_session_spends_its_budget_like_a_real_one() {
        let mut s = Session::dry_run(LiveGuards::default());
        for i in 1..=3 {
            let spend = s.authorise(addr(i), wei(1), wei(1000)).unwrap();
            s.commit_simulated(spend);
        }
        assert_eq!(s.budget().spent(), wei(3));
        // And the open-position cap bites in dry run exactly as it would live.
        assert!(s.authorise(addr(4), wei(1), wei(1000)).is_err());
    }

    #[test]
    fn the_briefing_a_session_offers_is_built_from_its_own_limits() {
        let limits = LiveGuards {
            session_budget_wei: wei(7),
            ..LiveGuards::default()
        };
        let s = Session::dry_run(limits);
        let b = s.briefing(wei(100), 4663);
        assert_eq!(b.guards.session_budget_wei, wei(7));
        assert_eq!(b.address, s.address());
        assert!(b.render().contains("LIVE TRADING"));
    }
}
