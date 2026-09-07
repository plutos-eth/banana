//! Encoding rules shared by every table.
//!
//! Spec §5.1 says to pick one representation for `U256` and be consistent. The choice and
//! its reasoning are PLAN.md D1: raw money is `BLOB(32)` big-endian, derived analytical
//! figures are `INTEGER` basis points.
//!
//! Big-endian matters beyond compactness: fixed-width big-endian blobs sort in SQLite's
//! `memcmp` order exactly as the numbers do, so `ORDER BY` and `MAX()` on a price column
//! are correct without decoding anything.

use alloy_primitives::{Address, B256, U256};

/// 32 bytes, big-endian. Always exactly 32, never trimmed, so ordering is total.
pub fn u256_to_blob(v: U256) -> [u8; 32] {
    v.to_be_bytes()
}

pub fn u256_from_blob(b: &[u8]) -> Option<U256> {
    if b.len() != 32 {
        return None;
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(b);
    Some(U256::from_be_bytes(buf))
}

/// Addresses and hashes are stored as lowercase `0x` hex text.
///
/// Text rather than a blob because these are joined, grouped and shown to the user far
/// more often than they are arithmetic, and a consistent lowercase form makes equality
/// comparisons exact without a collation.
pub fn addr_key(a: Address) -> String {
    format!("{a:#x}")
}

pub fn hash_key(h: B256) -> String {
    format!("{h:#x}")
}

/// Price is quote-wei per 10^18 tokens.
///
/// Scaling by 10^18 keeps a whole-token price in integers without losing the small values
/// a fresh curve produces: the real dev buy probed on 2026-09-07 works out to ~1.8e9 at
/// this scale, comfortably inside `U256` and far above the rounding floor.
pub const PRICE_SCALE: u64 = 1_000_000_000_000_000_000;

/// `quote * 10^18 / tokens`, or `None` when no tokens moved.
///
/// Integer division throughout; there is no floating point anywhere in this path.
pub fn price_of(quote: U256, tokens: U256) -> Option<U256> {
    if tokens.is_zero() {
        return None;
    }
    quote
        .checked_mul(U256::from(PRICE_SCALE))
        .map(|scaled| scaled / tokens)
}

/// A multiple of `entry`, in basis points. 20000 = 2x.
pub fn multiple_bps(price: U256, entry: U256) -> Option<u64> {
    if entry.is_zero() {
        return None;
    }
    let scaled = price.checked_mul(U256::from(10_000u64))?;
    let m = scaled / entry;
    // Clamp rather than wrap: a 10^15x multiple is a data error, not a moonshot, and it
    // must not silently become a small number.
    Some(m.try_into().unwrap_or(u64::MAX))
}

/// Which rule produced an `outcomes` row's entry price (PLAN.md F9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryRule {
    /// A real fill by a real buyer, after the opening-tax window.
    ObservedUntaxedBuy = 0,
    /// No untaxed buy exists, so the curve was replayed to the end of the tax window and a
    /// reference-size buy quoted against it.
    ///
    /// These are real entries the sniper would have taken, so they stay in the
    /// denominator. The distinction is surfaced as its own funnel stage rather than
    /// averaged in, because it changes what `entry_price` *means* for the row.
    ReconstructedAtWindowEnd = 1,
}

impl EntryRule {
    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(EntryRule::ObservedUntaxedBuy),
            1 => Some(EntryRule::ReconstructedAtWindowEnd),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EntryRule::ObservedUntaxedBuy => "observed",
            EntryRule::ReconstructedAtWindowEnd => "reconstructed",
        }
    }
}

/// Buy or sell, as stored in `trades.side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy = 0,
    Sell = 1,
}

impl Side {
    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(Side::Buy),
            1 => Some(Side::Sell),
            _ => None,
        }
    }
}

/// `Presence` as stored: 0 absent, 1 present, 2 unknown.
pub fn presence_to_i64(p: quarrel_core::features::Presence) -> i64 {
    use quarrel_core::features::Presence;
    match p {
        Presence::Absent => 0,
        Presence::Present => 1,
        Presence::Unknown => 2,
    }
}

pub fn presence_from_i64(v: i64) -> quarrel_core::features::Presence {
    use quarrel_core::features::Presence;
    match v {
        1 => Presence::Present,
        0 => Presence::Absent,
        // Anything unrecognised is Unknown, never Present: an unreadable value must not
        // become an assertion about the token.
        _ => Presence::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u256_blobs_round_trip_at_full_width() {
        for v in [
            U256::ZERO,
            U256::from(1u64),
            U256::from(88_421_000_000_000_000u64),
            U256::MAX,
        ] {
            let b = u256_to_blob(v);
            assert_eq!(b.len(), 32, "always 32 bytes so ordering is total");
            assert_eq!(u256_from_blob(&b), Some(v));
        }
    }

    #[test]
    fn a_wrong_width_blob_is_rejected_rather_than_padded() {
        assert_eq!(u256_from_blob(&[0u8; 31]), None);
        assert_eq!(u256_from_blob(&[0u8; 33]), None);
    }

    #[test]
    fn big_endian_blobs_sort_like_the_numbers_they_encode() {
        // This is why SQLite can ORDER BY and MAX() a price column without decoding.
        let small = u256_to_blob(U256::from(1u64));
        let mid = u256_to_blob(U256::from(1_000_000u64));
        let big = u256_to_blob(U256::MAX);
        assert!(small < mid, "byte order must match numeric order");
        assert!(mid < big);
    }

    #[test]
    fn price_of_the_real_dev_buy_is_well_inside_the_integer_range() {
        // The launch probed on 2026-09-07: 0.088421 ETH for 4.95e25 tokens.
        let quote = U256::from(88_421_000_000_000_000u64);
        let tokens = U256::from_str_radix("49524734362106262014495324", 10).unwrap();
        let p = price_of(quote, tokens).unwrap();
        assert!(
            p > U256::from(1_000_000_000u64),
            "not rounded away to nothing: {p}"
        );
        assert!(p < U256::from(10_000_000_000u64), "and not absurd: {p}");
    }

    #[test]
    fn price_of_nothing_is_none_not_zero() {
        assert_eq!(price_of(U256::from(1u64), U256::ZERO), None);
    }

    #[test]
    fn multiples_are_integer_basis_points() {
        let entry = U256::from(1_000u64);
        assert_eq!(multiple_bps(entry, entry), Some(10_000), "1x");
        assert_eq!(
            multiple_bps(U256::from(2_000u64), entry),
            Some(20_000),
            "2x"
        );
        assert_eq!(multiple_bps(U256::from(100u64), entry), Some(1_000), "0.1x");
        assert_eq!(multiple_bps(U256::ZERO, entry), Some(0), "to zero");
        assert_eq!(
            multiple_bps(entry, U256::ZERO),
            None,
            "no entry, no multiple"
        );
    }

    #[test]
    fn an_absurd_multiple_clamps_or_refuses_but_never_wraps() {
        // A wrapped multiple would silently become a small, plausible-looking number,
        // which is the one outcome that must not happen. Two guards, in order:

        // 1. Representable in U256 but larger than u64: clamp.
        let huge = U256::from(u128::MAX);
        assert_eq!(multiple_bps(huge, U256::from(1u64)), Some(u64::MAX));

        // 2. Not even representable, because scaling by 10000 overflows U256: refuse.
        //    None says "cannot compute", which is honest; any number here would be a lie.
        assert_eq!(multiple_bps(U256::MAX, U256::from(1u64)), None);
    }

    #[test]
    fn presence_round_trips_and_unknown_is_the_safe_default() {
        use quarrel_core::features::Presence;
        for p in [Presence::Absent, Presence::Present, Presence::Unknown] {
            assert_eq!(presence_from_i64(presence_to_i64(p)), p);
        }
        // A value the schema never wrote must not decode as Present.
        assert_eq!(presence_from_i64(99), Presence::Unknown);
        assert_eq!(presence_from_i64(-1), Presence::Unknown);
    }

    #[test]
    fn entry_rule_round_trips() {
        for r in [
            EntryRule::ObservedUntaxedBuy,
            EntryRule::ReconstructedAtWindowEnd,
        ] {
            assert_eq!(EntryRule::from_i64(r as i64), Some(r));
        }
        assert_eq!(
            EntryRule::from_i64(7),
            None,
            "an unknown rule is not guessed at"
        );
    }

    #[test]
    fn address_keys_are_lowercase_hex_so_equality_needs_no_collation() {
        let a: Address = "0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e"
            .parse()
            .unwrap();
        let k = addr_key(a);
        assert_eq!(k, k.to_lowercase());
        assert!(k.starts_with("0x"));
        assert_eq!(k.len(), 42);
    }

    #[test]
    fn hash_keys_are_lowercase_hex() {
        let h = B256::repeat_byte(0xAB);
        let k = hash_key(h);
        assert_eq!(k, k.to_lowercase());
        assert_eq!(k.len(), 66);
    }
}
