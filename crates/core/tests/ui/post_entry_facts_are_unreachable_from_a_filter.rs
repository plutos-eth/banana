//! Spec 5.3 / PLAN.md D8: a filter must not be able to read the future.
//!
//! `PostEntryFacts` holds values that only exist AFTER entry -- how many distinct buyers
//! arrived in the first minute, whether every early buy paid the opening tax, curve
//! progress. Using any of them as a filter input produces a backtest that looks excellent
//! and is a lie, because at decision time none of it is knowable.
//!
//! The guarantee is structural rather than documented: `evaluate` accepts `&PitFeatures`
//! and there is no other way in. This file must FAIL to compile. If it ever starts
//! compiling, the boundary has been widened and the guarantee is gone.
use banana_core::features::PostEntryFacts;
use banana_core::filter::{Condition, EntryFilter};

fn main() {
    let future = PostEntryFacts {
        distinct_buyers_1m: 12,
        every_early_buy_taxed: false,
        curve_progress_bps: 2_500,
    };
    let filter = EntryFilter::all_of([Condition::RequireTwitter]);

    // Must not compile: post-entry facts are not point-in-time features.
    let _ = filter.evaluate(&future);
}
