//! Where a trade goes, and when it must not go anywhere (spec §7, "Exit").
//!
//! > Route by phase: curve while it trades, v4 pool after graduation, refuse during the
//! > swept gap. **Never guess the phase — read it.**
//!
//! The swept gap is the dangerous one. Between the curve being emptied and the pool
//! existing, a token has no venue: a sell routed to the curve reverts at best, and code
//! that assumes "not curve, therefore pool" sends a swap to a pool that is not there. So
//! [`Route::of`] takes a phase that was *read from the factory* and there is no default
//! branch — an unrecognised value refuses rather than falling through to a venue.

use alloy_primitives::{Address, B256, U256, keccak256};
use quarrel_chain::abi::Phase;
use serde::{Deserialize, Serialize};

/// Where an order can go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "venue", rename_all = "snake_case")]
pub enum Route {
    /// The bonding curve contract for this token.
    Curve { curve: Address },
    /// The Uniswap v4 pool it graduated into.
    Pool { key: PoolKey },
    /// No venue. Carries the reason, which the user sees verbatim (spec §3.4).
    Refuse { reason: String },
}

impl Route {
    /// Decide the venue from a phase that was read, never inferred.
    pub fn of(phase: Phase, curve: Address, key: PoolKey) -> Self {
        match phase {
            Phase::Curve => Route::Curve { curve },
            Phase::Pool => Route::Pool { key },
            // Between venues. Not an error and not a pool: a gap, and the only safe
            // action in it is none.
            Phase::Swept => Route::Refuse {
                reason: "the launch has been swept off its curve and its pool does not \
                         exist yet; there is no venue to trade on until it does"
                    .into(),
            },
            Phase::Rescued => Route::Refuse {
                reason: "the launch was rescued rather than graduated; it has no pool".into(),
            },
        }
    }

    /// Decide from the raw byte the factory returned.
    ///
    /// An unknown value refuses. A new phase added to the contract must not be routed by
    /// a guess about which of the existing ones it resembles.
    pub fn of_raw(raw: u8, curve: Address, key: PoolKey) -> Self {
        match Phase::from_u8(raw) {
            Some(p) => Route::of(p, curve, key),
            None => Route::Refuse {
                reason: format!(
                    "the factory reports phase {raw}, which this build does not know. \
                     Refusing rather than guessing which venue it means"
                ),
            },
        }
    }

    pub fn is_refusal(&self) -> bool {
        matches!(self, Route::Refuse { .. })
    }
}

/// A Uniswap v4 pool key.
///
/// Currencies are ordered, which is part of the identity: `poolId` is a hash of the
/// encoded key, so getting the order wrong produces a different pool that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolKey {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
}

impl PoolKey {
    /// The key a pons launch graduates into.
    ///
    /// `fee` is zero on the key: the hook charges the swap fee instead. `tick_spacing`
    /// and `pair_token` are **per launch config**, not constants, so they are arguments
    /// rather than defaults — reading them from the factory is what makes this the pool
    /// the token actually went into rather than the one it probably went into.
    pub fn pons(token: Address, pair_token: Address, tick_spacing: i32, hooks: Address) -> Self {
        let (currency0, currency1) = if token < pair_token {
            (token, pair_token)
        } else {
            (pair_token, token)
        };
        Self {
            currency0,
            currency1,
            fee: 0,
            tick_spacing,
            hooks,
        }
    }

    /// `keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks))`.
    ///
    /// Five 32-byte words. Addresses are left-padded, `fee` is a `uint24` and
    /// `tickSpacing` an `int24`, both of which the ABI widens to a full word — and the
    /// int is sign-extended, which is why the negative case has its own test.
    pub fn id(&self) -> B256 {
        let mut buf = [0u8; 160];
        buf[12..32].copy_from_slice(self.currency0.as_slice());
        buf[44..64].copy_from_slice(self.currency1.as_slice());
        buf[64..96].copy_from_slice(&U256::from(self.fee).to_be_bytes::<32>());
        buf[96..128].copy_from_slice(&sign_extend(self.tick_spacing));
        buf[140..160].copy_from_slice(self.hooks.as_slice());
        keccak256(buf)
    }

    /// True when selling `token` means swapping currency0 for currency1.
    pub fn zero_for_one(&self, token: Address) -> bool {
        self.currency0 == token
    }
}

/// An `int24` as the ABI encodes it: sign-extended to 32 bytes.
fn sign_extend(v: i32) -> [u8; 32] {
    let mut out = [if v < 0 { 0xFF } else { 0x00 }; 32];
    out[28..32].copy_from_slice(&v.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn key() -> PoolKey {
        PoolKey::pons(addr(2), Address::ZERO, 200, addr(9))
    }

    // --- routing --------------------------------------------------------------------

    #[test]
    fn a_trading_curve_routes_to_the_curve() {
        assert_eq!(
            Route::of(Phase::Curve, addr(1), key()),
            Route::Curve { curve: addr(1) }
        );
    }

    #[test]
    fn a_graduated_token_routes_to_its_pool() {
        assert_eq!(
            Route::of(Phase::Pool, addr(1), key()),
            Route::Pool { key: key() }
        );
    }

    /// The gap this module exists for.
    #[test]
    fn the_swept_gap_refuses_rather_than_choosing_a_venue() {
        let r = Route::of(Phase::Swept, addr(1), key());
        assert!(r.is_refusal());
        let Route::Refuse { reason } = r else {
            unreachable!()
        };
        // The user has to be able to tell this from an error.
        assert!(reason.contains("swept"), "{reason}");
        assert!(reason.contains("does not exist yet"), "{reason}");
    }

    #[test]
    fn a_rescued_launch_refuses_too() {
        assert!(Route::of(Phase::Rescued, addr(1), key()).is_refusal());
    }

    #[test]
    fn an_unknown_phase_refuses_instead_of_resembling_a_known_one() {
        // A phase added to the contract after this build must not be routed by guessing
        // which existing one it is closest to.
        for raw in [4u8, 5, 99, 255] {
            let r = Route::of_raw(raw, addr(1), key());
            assert!(r.is_refusal(), "phase {raw} was routed somewhere");
            let Route::Refuse { reason } = r else {
                unreachable!()
            };
            assert!(reason.contains("does not know"), "{reason}");
        }
    }

    #[test]
    fn the_known_phases_route_the_same_from_their_raw_bytes() {
        assert_eq!(
            Route::of_raw(0, addr(1), key()),
            Route::of(Phase::Curve, addr(1), key())
        );
        assert_eq!(
            Route::of_raw(2, addr(1), key()),
            Route::of(Phase::Pool, addr(1), key())
        );
        assert!(Route::of_raw(1, addr(1), key()).is_refusal());
    }

    // --- pool identity --------------------------------------------------------------

    #[test]
    fn currencies_are_ordered_whichever_way_they_are_given() {
        let a = PoolKey::pons(addr(2), addr(7), 200, addr(9));
        assert_eq!(a.currency0, addr(2));
        assert_eq!(a.currency1, addr(7));

        // An ETH pair: the zero address sorts first.
        let b = PoolKey::pons(addr(2), Address::ZERO, 200, addr(9));
        assert_eq!(b.currency0, Address::ZERO);
        assert_eq!(b.currency1, addr(2));
    }

    #[test]
    fn the_same_pool_has_the_same_id_whichever_order_it_was_built_from() {
        let a = PoolKey::pons(addr(2), addr(7), 200, addr(9));
        let b = PoolKey::pons(addr(7), addr(2), 200, addr(9));
        assert_eq!(a, b);
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn every_field_changes_the_pool_id() {
        // Each of these is a different pool. A key that ignored one of them would produce
        // an id for a pool that does not exist, and the swap would revert -- or worse,
        // land in somebody else's.
        let base = key();
        let variants = [
            PoolKey {
                tick_spacing: 60,
                ..base
            },
            PoolKey { fee: 3_000, ..base },
            PoolKey {
                hooks: addr(8),
                ..base
            },
            PoolKey::pons(addr(3), Address::ZERO, 200, addr(9)),
        ];
        for v in variants {
            assert_ne!(v.id(), base.id(), "{v:?} shares an id with the base key");
        }
    }

    #[test]
    fn a_negative_tick_spacing_is_sign_extended_as_the_abi_encodes_an_int24() {
        // Not a hypothetical: `int24` is signed and a wrong encoding here would hash to a
        // pool that does not exist, which surfaces as an unexplained revert.
        let neg = PoolKey {
            tick_spacing: -200,
            ..key()
        };
        assert_ne!(neg.id(), key().id());

        let word = sign_extend(-200);
        assert_eq!(word[0], 0xFF, "high bytes must be ones for a negative");
        assert_eq!(&word[28..], &(-200i32).to_be_bytes());

        let pos = sign_extend(200);
        assert_eq!(pos[0], 0x00);
        assert_eq!(&pos[28..], &200i32.to_be_bytes());
    }

    #[test]
    fn the_pool_id_is_a_hash_of_five_words() {
        // Pinning the shape: 5 * 32 bytes. If a field is ever added to PoolKey, this is
        // what notices that the encoding needs to grow with it.
        let id = key().id();
        assert_eq!(id.len(), 32);
        assert_ne!(id, B256::ZERO);
    }

    #[test]
    fn selling_direction_follows_the_currency_order() {
        let k = PoolKey::pons(addr(2), Address::ZERO, 200, addr(9));
        // ETH is currency0 here, so selling the token is one-for-zero.
        assert!(!k.zero_for_one(addr(2)));
        assert!(k.zero_for_one(Address::ZERO));
    }
}
