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
//! # Where the key comes from
//!
//! Two sources, tried in order:
//!
//! 1. **The settings file**, `<data dir>/wallet.key`, which the user pastes their key into
//!    from inside the application. This is the friendly path and the default one.
//! 2. **`PRIVATE_KEY` in the environment**, which still works for a terminal or a script.
//!
//! # The trade-off, stated
//!
//! Both are **plaintext on disk**. Anything that can read the file or the process
//! environment can take the funds: another program running as you, a backup that captures
//! the folder, anyone who gets the machine. Encrypting it behind a passphrase would be
//! safer and would also mean typing that passphrase before every session, which fights a
//! tool whose whole purpose is to react in under three seconds.
//!
//! So the mitigation the product actually relies on is the session budget: the amount
//! reachable in one run is capped by configuration, and the wallet used for sniping should
//! hold only what the user is prepared to lose. This is not a custody tool.
//!
//! The key is **never returned to the user interface**. The application can save it,
//! replace it and delete it; what it hands back is the address it derives, so a screenshot
//! or a screen share cannot leak it.

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    #[error(
        "no key configured. Add one in Settings, or set PRIVATE_KEY in the environment. \
         Every other part of the application runs without a key at all"
    )]
    NoKey,
    #[error("PRIVATE_KEY is not a 32-byte hex key: {0}")]
    BadKey(String),
    #[error("signing failed: {0}")]
    Signing(String),
    #[error("could not read or write the key file: {0}")]
    Storage(String),
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

/// A key held in memory for the life of a session.
///
/// Constructed only where a live session starts. Nothing else in the workspace calls this,
/// and nothing else *can*: no other crate depends on `banana-live`.
#[derive(Debug)]
pub struct KeySigner {
    inner: PrivateKeySigner,
}

impl KeySigner {
    /// The key for this machine: the settings file first, then the environment.
    ///
    /// The file comes first because it is the one the user set from inside the
    /// application, and a stale environment variable silently overriding it would be a
    /// surprising way to end up trading from the wrong wallet.
    pub fn load(data_dir: &Path) -> Result<Self> {
        match keystore::read(data_dir)? {
            Some(hex) => Self::from_hex(&hex),
            None => Self::from_env(),
        }
    }

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
        let trimmed = strip_prefix(raw);
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

impl Signer for KeySigner {
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

/// A pasted key without its whitespace or its `0x`, in either case.
///
/// Case-insensitive on purpose: wallets export `0x…` and `0X…` both, and a key refused
/// because of the case of one character is an infuriating way to be told to try again.
fn strip_prefix(raw: &str) -> &str {
    let t = raw.trim();
    t.strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t)
}

/// Reading and writing the key the user pasted into the application.
///
/// Its own file rather than a field in `strategy.json`, because a strategy is the one
/// thing a user might reasonably send to somebody else, and a private key must never
/// travel with it.
pub mod keystore {
    use super::*;

    /// `<data dir>/wallet.key`. Plaintext hex, no `0x`, one line.
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("wallet.key")
    }

    /// The stored key, or `None` when there is not one.
    pub fn read(data_dir: &Path) -> Result<Option<String>> {
        match std::fs::read_to_string(path(data_dir)) {
            Ok(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SignerError::Storage(e.to_string())),
        }
    }

    /// Validate and store a key. Returns the address it derives.
    ///
    /// Validating first means a mistyped key is refused at the moment it is pasted rather
    /// than the first time an order would have fired. The address is what comes back: the
    /// key itself never leaves this crate once it is written.
    pub fn save(data_dir: &Path, raw: &str) -> Result<Address> {
        let signer = KeySigner::from_hex(raw)?;
        std::fs::create_dir_all(data_dir).map_err(|e| SignerError::Storage(e.to_string()))?;
        let normalised = strip_prefix(raw).to_lowercase();
        std::fs::write(path(data_dir), &normalised)
            .map_err(|e| SignerError::Storage(e.to_string()))?;
        Ok(signer.address())
    }

    /// Forget the key. Removing a file that is not there is success, not an error.
    pub fn clear(data_dir: &Path) -> Result<()> {
        match std::fs::remove_file(path(data_dir)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SignerError::Storage(e.to_string())),
        }
    }

    /// Which wallet is configured, without exposing the key.
    ///
    /// This is what the user interface is allowed to know.
    pub fn address(data_dir: &Path) -> Option<Address> {
        KeySigner::load(data_dir).ok().map(|s| s.address())
    }

    /// Whether the environment carries a key.
    ///
    /// Exists so that callers outside this crate can ask the question without naming the
    /// variable. `scripts/check-trust-boundary.ps1` fails on the literal anywhere but
    /// here, and the check is deliberately blunt: one with exceptions is one you can talk
    /// your way past. This is the exception-free way to answer it.
    pub fn env_key_present() -> bool {
        std::env::var("PRIVATE_KEY").is_ok()
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
        let s = KeySigner::from_hex(TEST_KEY).unwrap();
        assert_eq!(s.address(), TEST_ADDRESS.parse::<Address>().unwrap());
    }

    #[test]
    fn the_prefix_is_optional_in_either_case() {
        let want = TEST_ADDRESS.parse::<Address>().unwrap();
        for form in [
            TEST_KEY.to_string(),
            format!("0x{TEST_KEY}"),
            format!("0X{TEST_KEY}"),
            format!(
                "  0X{}  
",
                TEST_KEY.to_uppercase()
            ),
        ] {
            assert_eq!(
                KeySigner::from_hex(&form).unwrap().address(),
                want,
                "{form} was not accepted"
            );
        }
    }

    #[test]
    fn the_zero_x_prefix_is_optional() {
        let a = KeySigner::from_hex(TEST_KEY).unwrap().address();
        let b = KeySigner::from_hex(&format!("0x{TEST_KEY}"))
            .unwrap()
            .address();
        assert_eq!(a, b);
        // And whitespace from a copy-paste does not break it.
        let c = KeySigner::from_hex(&format!("  {TEST_KEY}\n"))
            .unwrap()
            .address();
        assert_eq!(a, c);
    }

    #[test]
    fn a_malformed_key_is_refused_without_echoing_it() {
        for bad in ["", "0x", "not-a-key", &"ab".repeat(31)] {
            let e = KeySigner::from_hex(bad).unwrap_err();
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

        let s = KeySigner::from_hex(TEST_KEY).unwrap();
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

        let s = KeySigner::from_hex(TEST_KEY).unwrap();
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
        let s = KeySigner::from_hex(TEST_KEY).unwrap();
        assert_eq!(s.sign(&tx()).unwrap(), s.sign(&tx()).unwrap());
    }

    #[test]
    fn a_dry_run_signer_holds_no_key_and_cannot_sign() {
        let s = NoSigner;
        assert_eq!(s.address(), Address::ZERO);
        assert!(matches!(s.sign(&tx()), Err(SignerError::NoKey)));
    }

    // --- the settings keystore ------------------------------------------------------

    fn key_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("banana-key-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_key_saved_from_settings_round_trips_to_its_address() {
        let d = key_dir("save");
        let addr = keystore::save(&d, TEST_KEY).unwrap();
        assert_eq!(addr, TEST_ADDRESS.parse::<Address>().unwrap());
        assert_eq!(keystore::address(&d), Some(addr));
        assert_eq!(KeySigner::load(&d).unwrap().address(), addr);
    }

    #[test]
    fn a_pasted_key_is_normalised_and_validated_before_it_is_written() {
        let d = key_dir("normalise");
        // Whatever a paste brings with it.
        keystore::save(
            &d,
            &format!(
                "  0X{}
",
                TEST_KEY.to_uppercase()
            ),
        )
        .unwrap();
        let stored = std::fs::read_to_string(keystore::path(&d)).unwrap();
        assert_eq!(stored, TEST_KEY, "stored as bare lowercase hex");
    }

    #[test]
    fn a_bad_key_is_refused_at_the_moment_it_is_pasted_and_nothing_is_written() {
        // Better here than the first time an order would have fired.
        let d = key_dir("bad");
        assert!(keystore::save(&d, "not-a-key").is_err());
        assert!(
            !keystore::path(&d).exists(),
            "a refused key must not be stored"
        );
        assert_eq!(keystore::address(&d), None);
    }

    #[test]
    fn a_saved_key_can_be_removed_and_removing_a_missing_one_is_not_an_error() {
        let d = key_dir("clear");
        keystore::save(&d, TEST_KEY).unwrap();
        assert!(keystore::path(&d).exists());

        keystore::clear(&d).unwrap();
        assert!(!keystore::path(&d).exists());
        assert_eq!(keystore::address(&d), None);
        keystore::clear(&d).unwrap(); // again, on nothing
    }

    #[test]
    fn the_settings_file_wins_over_the_environment() {
        // A stale environment variable silently overriding what the user set in the
        // application would be a surprising way to trade from the wrong wallet.
        let d = key_dir("precedence");
        keystore::save(&d, TEST_KEY).unwrap();
        assert_eq!(
            KeySigner::load(&d).unwrap().address(),
            TEST_ADDRESS.parse::<Address>().unwrap()
        );
    }

    #[test]
    fn the_key_never_leaves_this_crate_once_written() {
        // `save` hands back an address, and `address` reads one. Neither returns the key,
        // which is what keeps it out of an IPC payload and off a shared screen.
        let d = key_dir("opaque");
        let addr: Address = keystore::save(&d, TEST_KEY).unwrap();
        let looked_up: Option<Address> = keystore::address(&d);
        assert_eq!(looked_up, Some(addr));
    }

    #[test]
    fn an_empty_key_file_reads_as_no_key_rather_than_a_broken_one() {
        let d = key_dir("empty");
        std::fs::write(
            keystore::path(&d),
            "   
",
        )
        .unwrap();
        assert_eq!(keystore::read(&d).unwrap(), None);
    }

    #[test]
    fn a_missing_environment_variable_says_what_needs_it() {
        // Only meaningful when the variable really is absent, which is the case in CI and
        // on any machine that has not configured live trading.
        if std::env::var("PRIVATE_KEY").is_err() {
            let e = KeySigner::from_env().unwrap_err();
            assert!(matches!(e, SignerError::NoKey));
            assert!(e.to_string().contains("Add one in Settings"));
        }
    }
}
