//! The private key lives here and nowhere else (spec §3.8, invariant 8).
//!
//! `scripts/check-trust-boundary.ps1` asserts that the string `PRIVATE_KEY` appears
//! nowhere in `crates/` outside this crate, and that nothing below `live` in the
//! dependency graph can reach it. This file is the reason both of those checks have
//! something to point at.
//!
//! # Why a trait
//!
//! Spec §7.3 asks for one, so an encrypted keystore or an external signer can be added
//! later without touching call sites. That is not hypothetical politeness: a plaintext key
//! in `.env` is the weakest part of this design, and the honest way to ship it is to make
//! replacing it a matter of another implementation rather than a refactor.
//!
//! # The trade-off, stated
//!
//! [`EnvSigner`] reads a hex private key from the environment. Anything that can read the
//! process environment or the `.env` file can take the funds. `docs/SAFETY.md` says so in
//! the user's words; this comment says so in the developer's. The mitigation the product
//! actually relies on is §7.3's session budget: the amount reachable in one session is
//! capped by configuration, so the key protects less than it would otherwise.

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    #[error(
        "no key configured. Live trading needs PRIVATE_KEY in .env; every other command \
         runs without one"
    )]
    NoKey,
    #[error("PRIVATE_KEY is not a 32-byte hex key: {0}")]
    BadKey(String),
    #[error("signing failed: {0}")]
    Signing(String),
}

type Result<T> = std::result::Result<T, SignerError>;

/// An unsigned EIP-1559 transaction, in the terms this chain uses.
///
/// Its own type rather than a re-export so the rest of the crate never has to name an
/// `alloy` transaction type, and so a different signer backend can be added without the
/// call sites learning about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxRequest {
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub gas_limit: u64,
    /// Robinhood Chain is an Arbitrum stack with no priority fee auction — there is no
    /// mempool to bid into (spec §2) — so this is set to zero and the base fee is what
    /// is actually paid.
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// A signed, RLP-encoded transaction, ready for `eth_sendRawTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTx {
    pub raw: Bytes,
    pub hash: B256,
}

/// Whatever can turn a [`TxRequest`] into bytes the chain will accept.
pub trait Signer: Send + Sync + std::fmt::Debug {
    /// The address that will appear as the sender.
    fn address(&self) -> Address;
    fn sign(&self, tx: &TxRequest) -> Result<SignedTx>;
}

/// A key read from the process environment.
///
/// Constructed only where a live session is being armed. Nothing else in the workspace
/// calls this, and nothing else *can*: no other crate depends on `quarrel-live`.
#[derive(Debug)]
pub struct EnvSigner {
    inner: PrivateKeySigner,
}

impl EnvSigner {
    /// Read the key from `PRIVATE_KEY`.
    ///
    /// The variable name appears exactly once in the workspace, here.
    pub fn from_env() -> Result<Self> {
        let raw = std::env::var("PRIVATE_KEY").map_err(|_| SignerError::NoKey)?;
        Self::from_hex(&raw)
    }

    /// Parse a hex key, with or without the `0x`.
    ///
    /// The error deliberately does not echo the input: a key pasted into a log because a
    /// parse failed is a key that has to be rotated.
    pub fn from_hex(raw: &str) -> Result<Self> {
        let trimmed = raw.trim().trim_start_matches("0x");
        if trimmed.len() != 64 {
            return Err(SignerError::BadKey(format!(
                "expected 64 hex characters, got {}",
                trimmed.len()
            )));
        }
        let inner: PrivateKeySigner = trimmed
            .parse()
            .map_err(|_| SignerError::BadKey("not valid hex, or not a valid key".into()))?;
        Ok(Self { inner })
    }
}

impl Signer for EnvSigner {
    fn address(&self) -> Address {
        self.inner.address()
    }

    fn sign(&self, tx: &TxRequest) -> Result<SignedTx> {
        use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
        use alloy_eips::eip2718::Encodable2718;

        let unsigned = TxEip1559 {
            chain_id: tx.chain_id,
            nonce: tx.nonce,
            gas_limit: tx.gas_limit,
            max_fee_per_gas: tx.max_fee_per_gas,
            max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
            to: tx.to.into(),
            value: tx.value,
            access_list: Default::default(),
            input: tx.data.clone(),
        };
        let signature = self
            .inner
            .sign_hash_sync(&unsigned.signature_hash())
            .map_err(|e| SignerError::Signing(e.to_string()))?;
        let envelope: TxEnvelope = unsigned.into_signed(signature).into();
        Ok(SignedTx {
            raw: envelope.encoded_2718().into(),
            hash: *envelope.tx_hash(),
        })
    }
}

/// A signer that holds no key and refuses to sign.
///
/// This is what a dry-run session carries. It is not a stub for tests — it is the thing
/// that makes "dry run cannot spend money" true by construction rather than by a flag
/// somebody remembered to check (spec §3.2).
#[derive(Debug, Default)]
pub struct NoSigner;

impl Signer for NoSigner {
    fn address(&self) -> Address {
        Address::ZERO
    }

    fn sign(&self, _tx: &TxRequest) -> Result<SignedTx> {
        Err(SignerError::NoKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Account #0 of the standard Hardhat and Anvil development mnemonic ("test test …
    /// junk"). Chosen because the pairing is published in both projects' documentation, so
    /// this test checks our signing against a source outside this library rather than
    /// against whatever the library happens to compute.
    ///
    /// It is a public development key. Anything sent to it on any chain is gone.
    const TEST_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    fn tx() -> TxRequest {
        TxRequest {
            chain_id: 4663,
            nonce: 7,
            to: Address::repeat_byte(0x11),
            value: U256::from(1_000u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            gas_limit: 300_000,
            max_fee_per_gas: 100_000_000,
            max_priority_fee_per_gas: 0,
        }
    }

    #[test]
    fn a_known_key_produces_its_known_address() {
        let s = EnvSigner::from_hex(TEST_KEY).unwrap();
        assert_eq!(s.address(), TEST_ADDRESS.parse::<Address>().unwrap());
    }

    #[test]
    fn the_zero_x_prefix_is_optional() {
        let a = EnvSigner::from_hex(TEST_KEY).unwrap().address();
        let b = EnvSigner::from_hex(&format!("0x{TEST_KEY}"))
            .unwrap()
            .address();
        assert_eq!(a, b);
        // And whitespace from a copy-paste does not break it.
        let c = EnvSigner::from_hex(&format!("  {TEST_KEY}\n"))
            .unwrap()
            .address();
        assert_eq!(a, c);
    }

    #[test]
    fn a_malformed_key_is_refused_without_echoing_it() {
        for bad in ["", "0x", "not-a-key", &"ab".repeat(31)] {
            let e = EnvSigner::from_hex(bad).unwrap_err();
            let msg = e.to_string();
            assert!(matches!(e, SignerError::BadKey(_)), "{bad}");
            assert!(
                !msg.contains(bad) || bad.is_empty(),
                "a failed parse must not put the key in a log: {msg}"
            );
        }
    }

    #[test]
    fn a_signed_transaction_recovers_to_the_signing_address() {
        use alloy_consensus::transaction::SignerRecoverable;
        use alloy_eips::eip2718::Decodable2718;

        let s = EnvSigner::from_hex(TEST_KEY).unwrap();
        let signed = s.sign(&tx()).unwrap();

        // Decode it exactly as a node would, and check the sender is who we think.
        let envelope = alloy_consensus::TxEnvelope::decode_2718(&mut signed.raw.as_ref()).unwrap();
        assert_eq!(envelope.recover_signer().unwrap(), s.address());
        assert_eq!(*envelope.tx_hash(), signed.hash);
    }

    #[test]
    fn the_signed_transaction_carries_the_fields_it_was_given() {
        use alloy_consensus::Transaction;
        use alloy_eips::eip2718::Decodable2718;

        let s = EnvSigner::from_hex(TEST_KEY).unwrap();
        let req = tx();
        let signed = s.sign(&req).unwrap();
        let envelope = alloy_consensus::TxEnvelope::decode_2718(&mut signed.raw.as_ref()).unwrap();

        assert_eq!(envelope.chain_id(), Some(req.chain_id));
        assert_eq!(envelope.nonce(), req.nonce);
        assert_eq!(envelope.value(), req.value);
        assert_eq!(envelope.to(), Some(req.to));
        assert_eq!(envelope.input().as_ref(), req.data.as_ref());
        // No priority fee: there is no mempool on this chain to bid into (spec §2).
        assert_eq!(envelope.max_priority_fee_per_gas(), Some(0));
    }

    #[test]
    fn signing_the_same_request_twice_is_deterministic() {
        // RFC 6979 deterministic nonces. Worth pinning: a signer that produced a fresh
        // random k on every call would make a replay of a failed send a different
        // transaction hash, and the journal would lose track of it.
        let s = EnvSigner::from_hex(TEST_KEY).unwrap();
        assert_eq!(s.sign(&tx()).unwrap(), s.sign(&tx()).unwrap());
    }

    #[test]
    fn a_dry_run_signer_holds_no_key_and_cannot_sign() {
        let s = NoSigner;
        assert_eq!(s.address(), Address::ZERO);
        assert!(matches!(s.sign(&tx()), Err(SignerError::NoKey)));
    }

    #[test]
    fn a_missing_environment_variable_says_what_needs_it() {
        // Only meaningful when the variable really is absent, which is the case in CI and
        // on any machine that has not configured live trading.
        if std::env::var("PRIVATE_KEY").is_err() {
            let e = EnvSigner::from_env().unwrap_err();
            assert!(matches!(e, SignerError::NoKey));
            assert!(e.to_string().contains("every other command"));
        }
    }
}
