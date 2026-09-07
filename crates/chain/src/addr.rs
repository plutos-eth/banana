//! Contract addresses and chain constants.
//!
//! Every address was read from the live factory (`memeHook()`, `feeEscrow()`, ...) or from
//! the official deployment pages, by way of bodkin's `src/chain.ts`. **`doctor --probe`
//! re-checks each one**, because an address baked into a binary that has since moved is a
//! way to lose money quietly.

use alloy_primitives::{Address, address};

/// Robinhood Chain mainnet. Arbitrum stack, ETH gas, ~100 ms blocks.
///
/// Measured 2026-09-07: 100.87 ms mean over 800,000 blocks, ~856,500 blocks/day, and block
/// timestamps have one-second granularity so roughly ten blocks share a value.
pub const CHAIN_ID: u64 = 4663;

/// Measured mean block time in milliseconds. Used to size block ranges and to interpolate
/// timestamps between sampled anchors (PLAN.md F2).
pub const BLOCK_MS: u64 = 101;

pub const PONS_FACTORY: Address = address!("0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e");
pub const PONS_ROUTER: Address = address!("0xe33E9E479dF8802cb0866d5d05258bEc4cF62948");
pub const PONS_DEPLOYER: Address = address!("0x3711ceA4feaDE896C913C68F01Eda97Cb06D1A42");
pub const PONS_ESCROW: Address = address!("0xd3AFEB2a57f70eF218Aa82451c51B2fb0416Ac9e");
pub const PONS_HOOK: Address = address!("0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044");
pub const PONS_LOCKER: Address = address!("0x267444D099b10fB5Ed7c3Cc7B7c767AdcA574952");
pub const WETH: Address = address!("0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73");
pub const PERMIT2: Address = address!("0x000000000022D473030F116dDEE9F6B43aC78BA3");
pub const V4_POOL_MANAGER: Address = address!("0x8366a39cc670b4001a1121b8f6a443a643e40951");
pub const V4_QUOTER: Address = address!("0x8dc178efb8111bb0973dd9d722ebeff267c98f94");
pub const V4_STATE_VIEW: Address = address!("0xf3334192d15450cdd385c8b70e03f9a6bd9e673b");
pub const UNIVERSAL_ROUTER: Address = address!("0x8876789976decbfcbbbe364623c63652db8c0904");

/// Canonical Multicall3, verified live: 3808 bytes of code and `aggregate3` answers.
///
/// The "L2 Multicall" listed in the chain docs at `0x2cAC2D89…` is **not**
/// `aggregate3`-compatible, so it must not be substituted.
pub const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

/// The native quote asset, encoded as the zero address in `pairToken`.
pub const NATIVE: Address = Address::ZERO;

/// A throwaway recipient for quotes that must not name the user's wallet.
///
/// `currentSnipeTaxBps` is per-recipient, so reading it with a real address would leak
/// which wallet is watching. Dry-run quotes use this instead.
pub const DEAD: Address = address!("0x000000000000000000000000000000000000dEaD");

/// Explorer links. These are shown as text and opened only when the user clicks them:
/// nothing here is ever fetched by the app (spec §3.1, PLAN.md C1).
pub mod explorer {
    pub const BLOCKSCOUT: &str = "https://robinhoodchain.blockscout.com";

    pub fn tx(hash: &str) -> String {
        format!("{BLOCKSCOUT}/tx/{hash}")
    }
    pub fn address(addr: &str) -> String {
        format!("{BLOCKSCOUT}/address/{addr}")
    }
    pub fn token(addr: &str) -> String {
        format!("{BLOCKSCOUT}/token/{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_id_is_the_one_probed() {
        // eth_chainId returned 0x1237 on 2026-09-07.
        assert_eq!(CHAIN_ID, 0x1237);
    }

    #[test]
    fn multicall3_is_the_canonical_address_not_the_chain_docs_one() {
        assert_eq!(
            MULTICALL3.to_string().to_lowercase(),
            "0xca11bde05977b3631167028862be2a173976ca11"
        );
    }

    #[test]
    fn the_native_pair_token_is_the_zero_address() {
        // A launch with pairToken == 0 trades in native ETH; the real launch probed had
        // exactly this.
        assert!(NATIVE.is_zero());
    }

    #[test]
    fn explorer_links_are_plain_strings_not_fetches() {
        assert_eq!(
            explorer::tx("0xabc"),
            "https://robinhoodchain.blockscout.com/tx/0xabc"
        );
    }
}
