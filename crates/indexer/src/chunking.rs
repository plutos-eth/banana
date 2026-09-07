//! Adaptive block-range chunking for `eth_getLogs`.
//!
//! The endpoint returns at most **10,000 logs per query** and answers
//! `{"code":-32000,"message":"logs matched by query exceeds limit of 10000"}` when a range
//! matches more. This is not mentioned anywhere in the specification and it is not a rate
//! limit: retrying cannot fix it, waiting cannot fix it, and only splitting the range can.
//! An indexer that treats it as a transient failure appears to hang during busy periods
//! (PLAN.md F3).
//!
//! Density is also bursty. Adjacent 2,000-block samples measured 1.1 MB and 2.7 MB — a
//! 2.4x swing — so no fixed chunk size is safe in advance. The size therefore moves: it
//! halves on a refusal and grows back gradually while queries are answered, which is the
//! same additive-increase / multiplicative-decrease shape the RPC gate uses for
//! concurrency, and for the same reason.

/// A half-open-at-neither-end block range: both ends inclusive, as `eth_getLogs` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub from: u64,
    pub to: u64,
}

impl Range {
    pub fn new(from: u64, to: u64) -> Self {
        debug_assert!(from <= to, "a range must not be inverted");
        Self { from, to }
    }

    pub fn blocks(&self) -> u64 {
        self.to.saturating_sub(self.from) + 1
    }

    /// Split in two. Returns `None` for a single block, which cannot be split further.
    pub fn halve(&self) -> Option<(Range, Range)> {
        if self.blocks() < 2 {
            return None;
        }
        let mid = self.from + (self.to - self.from) / 2;
        Some((Range::new(self.from, mid), Range::new(mid + 1, self.to)))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkerConfig {
    /// Where to start. 4,000 blocks is roughly 4,000 logs at measured density, which
    /// leaves headroom under the 10,000 cap even when a busy period doubles it.
    pub initial: u64,
    pub min: u64,
    pub max: u64,
    /// Successful chunks before the size grows.
    pub grow_after: u32,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            initial: 4_000,
            // One block always fits: a single block cannot exceed the cap unless the chain
            // itself does, in which case nothing can be done and the error should surface.
            min: 1,
            max: 20_000,
            grow_after: 8,
        }
    }
}

/// Chooses the next range to request and reacts to what the endpoint says about it.
#[derive(Debug)]
pub struct Chunker {
    config: ChunkerConfig,
    size: u64,
    clean_streak: u32,
    /// Ranges deferred by a split, newest first.
    pending: Vec<Range>,
    cursor: u64,
    end: u64,
    pub splits: u32,
    pub shrinks: u32,
    pub grows: u32,
}

impl Chunker {
    pub fn new(from: u64, to: u64, config: ChunkerConfig) -> Self {
        Self {
            size: config.initial.clamp(config.min, config.max),
            config,
            clean_streak: 0,
            pending: Vec::new(),
            cursor: from,
            end: to,
            splits: 0,
            shrinks: 0,
            grows: 0,
        }
    }

    pub fn current_size(&self) -> u64 {
        self.size
    }

    /// The next range to request, or `None` when the window is fully covered.
    pub fn next_range(&mut self) -> Option<Range> {
        if let Some(r) = self.pending.pop() {
            return Some(r);
        }
        if self.cursor > self.end {
            return None;
        }
        let to = (self.cursor + self.size - 1).min(self.end);
        Some(Range::new(self.cursor, to))
    }

    /// The endpoint answered. Advance past `range` and consider growing.
    pub fn succeeded(&mut self, range: Range) {
        // A range from the pending stack does not move the cursor; the cursor already
        // passed it when the parent was split.
        if range.from == self.cursor {
            self.cursor = range.to.saturating_add(1);
        }
        self.clean_streak += 1;
        if self.clean_streak >= self.config.grow_after && self.size < self.config.max {
            self.clean_streak = 0;
            self.size = (self.size + self.size / 2).min(self.config.max);
            self.grows += 1;
        }
    }

    /// The endpoint refused `range` for matching too many logs.
    ///
    /// Halves the working size and queues both halves, so the range is still covered
    /// exactly once. Returns `false` when the range is a single block and cannot be split,
    /// which is a genuine dead end the caller must surface.
    pub fn too_many_results(&mut self, range: Range) -> bool {
        self.clean_streak = 0;
        self.size = (self.size / 2).max(self.config.min);
        self.shrinks += 1;

        let Some((a, b)) = range.halve() else {
            return false;
        };
        self.splits += 1;
        if range.from == self.cursor {
            self.cursor = range.to.saturating_add(1);
        }
        // Pushed so that `a` is popped before `b`, keeping chain order.
        self.pending.push(b);
        self.pending.push(a);
        true
    }

    /// Blocks not yet covered, for progress reporting.
    pub fn remaining(&self) -> u64 {
        let ahead = self
            .end
            .saturating_sub(self.cursor)
            .saturating_add(if self.cursor > self.end { 0 } else { 1 });
        ahead + self.pending.iter().map(|r| r.blocks()).sum::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(c: &mut Chunker) -> Vec<Range> {
        let mut out = Vec::new();
        while let Some(r) = c.next_range() {
            c.succeeded(r);
            out.push(r);
            assert!(out.len() < 10_000, "chunker did not terminate");
        }
        out
    }

    /// The property that matters most: every block is covered exactly once, whatever the
    /// endpoint does. A gap loses launches silently; an overlap is merely wasteful.
    fn assert_covers_exactly(ranges: &[Range], from: u64, to: u64) {
        let mut sorted: Vec<_> = ranges.to_vec();
        sorted.sort_by_key(|r| r.from);
        assert_eq!(sorted[0].from, from, "must start at the beginning");
        assert_eq!(sorted.last().unwrap().to, to, "must reach the end");
        for w in sorted.windows(2) {
            assert_eq!(
                w[1].from,
                w[0].to + 1,
                "gap or overlap between {:?} and {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn a_clean_run_covers_the_window_exactly() {
        let mut c = Chunker::new(1_000, 20_999, ChunkerConfig::default());
        let ranges = drain(&mut c);
        assert_covers_exactly(&ranges, 1_000, 20_999);
    }

    #[test]
    fn the_last_chunk_is_clipped_to_the_window_end() {
        let mut c = Chunker::new(0, 4_500, ChunkerConfig::default());
        let ranges = drain(&mut c);
        assert_eq!(ranges.last().unwrap().to, 4_500, "never past the target");
        assert_covers_exactly(&ranges, 0, 4_500);
    }

    /// F3: the 10,000-log cap must cause a split, not a retry.
    #[test]
    fn a_refusal_splits_the_range_and_still_covers_it_exactly() {
        let mut c = Chunker::new(0, 7_999, ChunkerConfig::default());
        let mut done = Vec::new();

        let first = c.next_range().unwrap();
        assert_eq!(first, Range::new(0, 3_999));
        assert!(c.too_many_results(first), "must be splittable");

        while let Some(r) = c.next_range() {
            c.succeeded(r);
            done.push(r);
            assert!(done.len() < 1_000);
        }
        assert_covers_exactly(&done, 0, 7_999);
        assert!(
            !done.contains(&first),
            "the refused range is never used whole"
        );
        assert_eq!(c.splits, 1);
    }

    #[test]
    fn repeated_refusals_keep_splitting_until_the_range_fits() {
        // A pathologically dense window: everything above 500 blocks is refused.
        let mut c = Chunker::new(0, 3_999, ChunkerConfig::default());
        let mut done = Vec::new();
        let mut guard = 0;

        while let Some(r) = c.next_range() {
            guard += 1;
            assert!(guard < 1_000, "did not converge");
            if r.blocks() > 500 {
                assert!(c.too_many_results(r), "must keep splitting");
            } else {
                c.succeeded(r);
                done.push(r);
            }
        }
        assert_covers_exactly(&done, 0, 3_999);
        assert!(done.iter().all(|r| r.blocks() <= 500));
    }

    #[test]
    fn a_refusal_shrinks_the_working_size_multiplicatively() {
        let mut c = Chunker::new(0, 100_000, ChunkerConfig::default());
        assert_eq!(c.current_size(), 4_000);
        let r = c.next_range().unwrap();
        c.too_many_results(r);
        assert_eq!(c.current_size(), 2_000, "halved");
        let r = c.next_range().unwrap();
        c.too_many_results(r);
        assert_eq!(c.current_size(), 1_000, "halved again");
    }

    #[test]
    fn the_size_grows_back_after_a_clean_streak() {
        let cfg = ChunkerConfig {
            grow_after: 3,
            ..Default::default()
        };
        let mut c = Chunker::new(0, 1_000_000, cfg);
        assert_eq!(c.current_size(), 4_000);
        for _ in 0..3 {
            let r = c.next_range().unwrap();
            c.succeeded(r);
        }
        assert_eq!(
            c.current_size(),
            6_000,
            "additive-ish increase after a clean streak"
        );
        assert_eq!(c.grows, 1);
    }

    #[test]
    fn the_size_never_grows_past_the_ceiling() {
        let cfg = ChunkerConfig {
            grow_after: 1,
            max: 5_000,
            ..Default::default()
        };
        let mut c = Chunker::new(0, 10_000_000, cfg);
        for _ in 0..20 {
            let r = c.next_range().unwrap();
            c.succeeded(r);
        }
        assert_eq!(c.current_size(), 5_000);
    }

    #[test]
    fn the_size_never_shrinks_below_one_block() {
        let mut c = Chunker::new(0, 1_000, ChunkerConfig::default());
        for _ in 0..40 {
            if let Some(r) = c.next_range() {
                c.too_many_results(r);
            }
        }
        assert!(
            c.current_size() >= 1,
            "a zero-block chunk would never terminate"
        );
    }

    #[test]
    fn a_single_block_that_still_refuses_is_a_dead_end_the_caller_must_see() {
        // Nothing can be split further. Silently skipping it would lose data; the caller
        // has to know.
        let mut c = Chunker::new(500, 500, ChunkerConfig::default());
        let r = c.next_range().unwrap();
        assert_eq!(r.blocks(), 1);
        assert!(
            !c.too_many_results(r),
            "must report that it cannot be split rather than looping"
        );
    }

    #[test]
    fn split_halves_are_requested_in_chain_order() {
        // Out-of-order chunks would still be correct, but in-order keeps the trade stream
        // monotonic, which the outcome replay depends on.
        let mut c = Chunker::new(0, 3_999, ChunkerConfig::default());
        let first = c.next_range().unwrap();
        c.too_many_results(first);
        let a = c.next_range().unwrap();
        c.succeeded(a);
        let b = c.next_range().unwrap();
        assert!(a.to < b.from, "{a:?} must come before {b:?}");
    }

    #[test]
    fn a_one_block_window_is_covered() {
        let mut c = Chunker::new(42, 42, ChunkerConfig::default());
        let ranges = drain(&mut c);
        assert_eq!(ranges, vec![Range::new(42, 42)]);
    }

    #[test]
    fn remaining_falls_to_zero_as_the_window_is_covered() {
        let mut c = Chunker::new(0, 9_999, ChunkerConfig::default());
        assert_eq!(c.remaining(), 10_000);
        let r = c.next_range().unwrap();
        c.succeeded(r);
        assert_eq!(c.remaining(), 6_000);
        drain(&mut c);
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn range_halving_is_exact_and_contiguous() {
        let (a, b) = Range::new(0, 9).halve().unwrap();
        assert_eq!(a, Range::new(0, 4));
        assert_eq!(b, Range::new(5, 9));
        assert_eq!(a.blocks() + b.blocks(), 10);

        let (a, b) = Range::new(0, 1).halve().unwrap();
        assert_eq!(a, Range::new(0, 0));
        assert_eq!(b, Range::new(1, 1));

        assert_eq!(Range::new(7, 7).halve(), None);
    }
}
