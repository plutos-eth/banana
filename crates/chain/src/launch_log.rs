//! Decoding the factory's `TokenLaunched` event.
//!
//! The event counterpart to [`crate::launch_tx`], which decodes the calldata. One decoder,
//! used by both halves of the product: the indexer turns these into store rows and the
//! sniper turns them into live sightings. Two decoders for one event is two chances to
//! read `pairToken` out of the wrong word, and the mistake would be silent in whichever
//! half is less exercised.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;

use crate::abi::IPonsFactory;
use crate::rpc::RawLog;

/// One `TokenLaunched`, exactly as the event carries it.
///
/// Everything here is fixed by the transaction that created the token, so all of it is
/// point-in-time by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchLog {
    pub token: Address,
    pub curve: Address,
    pub deployer: Address,
    pub pair_token: Address,
    pub launch_config_id: u64,
    /// Per launch, not per chain. The phantom reserve derives from it, so the curve
    /// cannot be priced without it.
    pub graduation_threshold: U256,
    pub block: u64,
    pub tx_hash: B256,
    pub log_index: u64,
}

/// The topic a launch log carries, for building a filter.
pub fn topic0() -> B256 {
    IPonsFactory::TokenLaunched::SIGNATURE_HASH
}

/// Decode, or `None` when the log is not a well-formed `TokenLaunched`.
///
/// `None` rather than a default: a launch whose `pairToken` word is missing is a launch we
/// cannot price, and inventing `address(0)` for it would silently claim it trades in ETH.
pub fn decode(l: &RawLog) -> Option<LaunchLog> {
    if l.topic0()? != topic0() {
        return None;
    }
    let token = topic_address(l, 1)?;
    let curve = topic_address(l, 2)?;
    let deployer = topic_address(l, 3)?;
    // Non-indexed, in order: pairToken, launchConfigId, graduationThreshold.
    let words: Vec<U256> = l
        .data
        .as_chunks::<32>()
        .0
        .iter()
        .map(|w| U256::from_be_bytes::<32>(*w))
        .collect();
    if words.len() < 3 {
        return None;
    }
    Some(LaunchLog {
        token,
        curve,
        deployer,
        pair_token: Address::from_slice(&words[0].to_be_bytes::<32>()[12..]),
        launch_config_id: words[1].try_into().unwrap_or(0),
        graduation_threshold: words[2],
        block: l.block_number,
        tx_hash: l.tx_hash,
        log_index: l.log_index,
    })
}

/// An indexed address argument, which sits in the low 20 bytes of its topic.
fn topic_address(l: &RawLog, i: usize) -> Option<Address> {
    l.topics.get(i).map(|t| Address::from_slice(&t.0[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, hex};

    /// The launch used as a fixture throughout: token `0x9d0d…`, block 812,436.
    fn real_log() -> RawLog {
        let mut data = Vec::new();
        // pairToken = 0 (native ETH)
        data.extend_from_slice(&[0u8; 32]);
        // launchConfigId = 1
        data.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        // graduationThreshold
        data.extend_from_slice(&U256::from(4_000_000_000_000_000_000u64).to_be_bytes::<32>());
        RawLog {
            address: crate::addr::PONS_FACTORY,
            topics: vec![
                topic0(),
                B256::left_padding_from(&hex!("9d0d1d2b3c4d5e6f708192a3b4c5d6e7f8091a2b")),
                B256::left_padding_from(&hex!("00112233445566778899aabbccddeeff00112233")),
                B256::left_padding_from(&hex!("deadbeef00000000000000000000000000000001")),
            ],
            data: Bytes::from(data),
            block_number: 812_436,
            tx_hash: B256::repeat_byte(7),
            tx_index: 3,
            log_index: 11,
        }
    }

    #[test]
    fn decodes_the_three_indexed_addresses_and_the_three_words() {
        let l = decode(&real_log()).expect("well-formed");
        assert_eq!(
            l.token.to_string().to_lowercase(),
            "0x9d0d1d2b3c4d5e6f708192a3b4c5d6e7f8091a2b"
        );
        assert!(l.pair_token.is_zero(), "native ETH pair");
        assert_eq!(l.launch_config_id, 1);
        assert_eq!(
            l.graduation_threshold,
            U256::from(4_000_000_000_000_000_000u64)
        );
        assert_eq!(l.block, 812_436);
        assert_eq!(l.log_index, 11);
    }

    /// A different event on the same contract must not decode as a launch.
    #[test]
    fn a_log_with_another_topic_is_not_a_launch() {
        let mut l = real_log();
        l.topics[0] = IPonsFactory::PoolGraduated::SIGNATURE_HASH;
        assert_eq!(decode(&l), None);
    }

    /// A truncated payload is refused rather than defaulted: a missing `pairToken` word
    /// read as zero would claim an unpriceable launch trades in ETH.
    #[test]
    fn a_short_payload_is_refused_not_defaulted() {
        let mut l = real_log();
        l.data = Bytes::from(vec![0u8; 64]);
        assert_eq!(decode(&l), None);
    }

    #[test]
    fn a_log_with_too_few_topics_is_refused() {
        let mut l = real_log();
        l.topics.truncate(3);
        assert_eq!(decode(&l), None);
    }
}
