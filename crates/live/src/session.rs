//! A trading session: the mode, the key, and the budget, as one thing.
//!
//! Two modes, chosen once when the application starts:
//!
//! * **TEST** — everything runs and nothing is signed.
//! * **LIVE** — entries are signed and sent, inside the money guards.
//!
//! The way "TEST cannot spend" is made true is not a boolean anybody checks. A [`Session`]
//! owns a `Box<dyn Signer>`, and a test session owns a [`NoSigner`] — which holds no key
//! and returns an error from `sign`. There is no code path that spends money without a
//! signature, so a test session cannot spend even if every other check in the program were
//! removed.
//!
//! The choice holds for the life of the process. Changing it means restarting, which is
//! what keeps a running session from drifting into spending money it was not started to
//! spend.

use alloy_primitives::{Address, U256};
use banana_core::strategy::LiveGuards;

use crate::briefing::Briefing;
use crate::guards::{Budget, Refused, Spend};
use crate::signer::{KeySigner, NoSigner, SignedTx, Signer, SignerError, TxRequest};

/// Whether real money can move. Displayed on every surface, always (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Everything runs; nothing is signed. No key is loaded.
    Test,
    /// Entries are signed and sent, inside the money guards.
    Live,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Test => "TEST",
            Mode::Live => "LIVE",
        }
    }

    /// The only mode in which a transaction can be signed.
    pub fn can_spend(self) -> bool {
        matches!(self, Mode::Live)
    }

    /// What this mode means, for the user rather than for the code.
    pub fn explain(self) -> &'static str {
        match self {
            Mode::Test => {
                "Everything runs and nothing can be signed: this session holds no key.                  Restart to switch to live trading."
            }
            Mode::Live => {
                "Entries will be signed and sent, inside the session budget. Restart to                  switch back to test."
            }
        }
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
    /// Everything runs; nothing can be signed, because there is no key to sign with.
    pub fn test(limits: LiveGuards) -> Self {
        Self {
            mode: Mode::Test,
            signer: Box::new(NoSigner),
            budget: Budget::new(limits),
        }
    }

    /// A session that can spend. Reads the key, and fails if there is not one.
    ///
    /// Failing is deliberate: silently falling back to test would leave the user believing
    /// they are trading when nothing fires, which is worse than an error.
    pub fn live(limits: LiveGuards, data_dir: &std::path::Path) -> Result<Self, SessionError> {
        Ok(Self {
            mode: Mode::Live,
            signer: Box::new(KeySigner::load(data_dir)?),
            budget: Budget::new(limits),
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

    /// Sign a transaction that spends nothing: an exit, or an approval.
    ///
    /// No [`Spend`], deliberately. The money guards cap what is put **at risk**, and a
    /// guard that could refuse a sale would be a guard that loses money — the position is
    /// already open, and the only thing left to decide is whether to keep holding it.
    ///
    /// In TEST this fails exactly as `sign` does, for the same reason: no key.
    pub fn sign_exit(&self, tx: &TxRequest) -> Result<SignedTx, SessionError> {
        Ok(self.signer.sign(tx)?)
    }

    /// Record a spend that did not need signing, for the test-mode journal.
    ///
    /// Keeps the guards' accounting honest in test mode: a simulated session that never
    /// consumed its budget would report that a strategy fits inside limits it would in
    /// fact have blown through.
    pub fn commit_simulated(&mut self, spend: Spend) {
        debug_assert_eq!(self.mode, Mode::Test, "live spends go through sign()");
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
    fn a_test_session_holds_no_key() {
        let s = Session::test(LiveGuards::default());
        assert_eq!(s.mode(), Mode::Test);
        assert_eq!(s.mode().label(), "TEST");
        assert!(!s.mode().can_spend());
        assert_eq!(s.address(), Address::ZERO, "no key, no address");
    }

    /// The invariant, stated as a test: TEST cannot spend money.
    #[test]
    fn a_test_session_cannot_sign_even_with_a_valid_authorisation() {
        let mut s = Session::test(LiveGuards::default());
        // The guards are perfectly happy.
        let spend = s.authorise(addr(1), wei(1), wei(1000)).unwrap();
        // And it still cannot spend, because there is nothing to sign with.
        let e = s.sign(spend, &tx()).unwrap_err();
        assert!(matches!(e, SessionError::Signer(SignerError::NoKey)));
        assert_eq!(s.budget().spent(), U256::ZERO, "and nothing was recorded");
    }

    #[test]
    fn a_live_session_without_a_key_fails_rather_than_falling_back_to_test() {
        // Silently downgrading would be the worst of both: the user believes they are
        // live and no orders fire.
        if std::env::var("PRIVATE_KEY").is_ok() {
            return; // a configured machine cannot exercise this
        }
        let e = Session::live(LiveGuards::default(), &std::env::temp_dir()).unwrap_err();
        assert!(matches!(e, SessionError::Signer(SignerError::NoKey)));
        assert!(e.to_string().contains("Add one in Settings"));
    }

    #[test]
    fn the_guards_still_refuse_in_test_mode() {
        // A test run that ignored the guards would report that a strategy fits inside
        // limits it would have blown through.
        let s = Session::test(LiveGuards::default());
        let e = s.authorise(addr(1), wei(50), wei(1000)).unwrap_err();
        assert!(matches!(e, SessionError::Refused(_)));
        assert!(e.to_string().contains("per-buy cap"));
    }

    #[test]
    fn a_simulated_session_spends_its_budget_like_a_real_one() {
        let mut s = Session::test(LiveGuards::default());
        for i in 1..=3 {
            let spend = s.authorise(addr(i), wei(1), wei(1000)).unwrap();
            s.commit_simulated(spend);
        }
        assert_eq!(s.budget().spent(), wei(3));
        // And the open-position cap bites in test exactly as it would live.
        assert!(s.authorise(addr(4), wei(1), wei(1000)).is_err());
    }

    #[test]
    fn the_briefing_a_session_offers_is_built_from_its_own_limits() {
        let limits = LiveGuards {
            session_budget_wei: wei(7),
            ..LiveGuards::default()
        };
        let s = Session::test(limits);
        let b = s.briefing(wei(100), 4663);
        assert_eq!(b.guards.session_budget_wei, wei(7));
        assert_eq!(b.address, s.address());
        assert!(b.render().contains("LIVE TRADING"));
    }

    #[test]
    fn each_mode_says_what_it_means_rather_than_only_naming_itself() {
        for m in [Mode::Test, Mode::Live] {
            assert!(!m.label().is_empty());
            assert!(m.explain().len() > 40, "{m:?} explains nothing");
        }
        assert!(Mode::Test.explain().contains("holds no key"));
        assert!(Mode::Live.explain().contains("signed and sent"));
    }
}
