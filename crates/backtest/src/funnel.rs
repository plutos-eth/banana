//! The funnel: where the universe went, stage by stage.
//!
//! Spec §5.5 makes this "a mandatory element of the results view, not an optional chart",
//! so it is a required field of the result type rather than something a caller opts into.
//! Every stage names how many launches entered it and how many left, and the stages are
//! ordered, so a reader can always answer "out of how many?" without doing arithmetic.
//!
//! Two stages exist that §5.5 does not list, both because measurement found they were
//! hiding something:
//!
//! * **deployer history depth** (PLAN.md C2) — only present when the strategy actually
//!   reads a deployer feature, because it is the cost of removing the window-edge bias
//!   and a strategy that never looks at the deployer should not pay it.
//! * **priced** (PLAN.md F9) — a curve whose replay does not reproduce reality is refused
//!   a reconstructed entry rather than given a wrong one. Those launches passed the
//!   filter and the sniper would have entered them, so they cannot silently disappear
//!   between "passed" and "reached target".

use serde::{Deserialize, Serialize};

/// One step of the funnel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// Stable machine-readable id, for the UI to key on.
    pub id: String,
    /// One line a user can read without the documentation.
    pub label: String,
    /// The stage this one narrows, or `None` for the raw universe.
    ///
    /// Present because the funnel is not a single chain. `reached_target` and `migrated`
    /// both narrow `priced`, and they are not nested: with a 2x five-minute target, a token
    /// can migrate without ever having doubled in the first five minutes. Reading them as a
    /// chain would say "of the tokens that reached the target, N migrated", which is a
    /// different and false claim.
    pub of: Option<String>,
    /// How many launches remain after this stage.
    pub remaining: u64,
    /// How many of `of` this stage removed.
    pub removed: u64,
}

/// The whole funnel, in order. Never empty: the first stage is always the raw universe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Funnel {
    pub stages: Vec<Stage>,
    /// The id of the last stage on the main chain, so a branch does not become the base for
    /// whatever narrows next.
    #[serde(default)]
    chain: String,
}

impl Default for Funnel {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Funnel {
    /// Start the funnel at the raw count of launches in the window.
    pub fn new(all_launches: u64) -> Self {
        Self {
            chain: "all_launches".into(),
            stages: vec![Stage {
                id: "all_launches".into(),
                label: "launches in the indexed window".into(),
                of: None,
                remaining: all_launches,
                removed: 0,
            }],
        }
    }

    /// Record a stage that narrows whatever the funnel currently stands at.
    pub fn narrow(&mut self, id: &str, label: impl Into<String>, remaining: u64) {
        let from = self
            .stages
            .last()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "all_launches".to_owned());
        self.push(id, label, &from, remaining);
    }

    /// Record a stage that narrows a **named** earlier stage rather than the last one.
    ///
    /// `remaining()` is unaffected, so a branch cannot be mistaken for the funnel's current
    /// position by whatever narrows next.
    pub fn branch(&mut self, id: &str, label: impl Into<String>, from: &str, remaining: u64) {
        let chain = std::mem::take(&mut self.chain);
        self.push(id, label, from, remaining);
        self.chain = chain;
    }

    fn push(&mut self, id: &str, label: impl Into<String>, from: &str, remaining: u64) {
        let base = self.stage(from).map(|s| s.remaining).unwrap_or(0);
        self.stages.push(Stage {
            id: id.to_owned(),
            label: label.into(),
            of: Some(from.to_owned()),
            remaining,
            removed: base.saturating_sub(remaining),
        });
        self.chain = id.to_owned();
    }

    /// How many launches survive the **chain** so far, ignoring any branches.
    pub fn remaining(&self) -> u64 {
        self.stage(&self.chain).map(|s| s.remaining).unwrap_or(0)
    }

    pub fn stage(&self, id: &str) -> Option<&Stage> {
        self.stages.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_funnel_always_starts_with_the_raw_universe() {
        let f = Funnel::new(5_091);
        assert_eq!(f.stages[0].id, "all_launches");
        assert_eq!(f.remaining(), 5_091);
    }

    #[test]
    fn each_stage_records_what_it_removed() {
        let mut f = Funnel::new(100);
        f.narrow("matured", "at least 6 h of subsequent history", 80);
        f.narrow("passed_filter", "passed every entry rule", 12);
        assert_eq!(f.stages[1].removed, 20);
        assert_eq!(f.stages[2].removed, 68);
        assert_eq!(f.remaining(), 12);
    }

    /// The arithmetic a reader does in their head must always work out.
    #[test]
    fn removals_and_the_final_count_add_up_to_the_universe() {
        let mut f = Funnel::new(100);
        f.narrow("matured", "matured", 80);
        f.narrow("passed_filter", "passed", 12);
        f.narrow("priced", "priced", 11);
        let removed: u64 = f.stages.iter().map(|s| s.removed).sum();
        assert_eq!(removed + f.remaining(), 100);
    }

    /// A branch must not become the base for whatever narrows next.
    #[test]
    fn a_branch_narrows_a_named_stage_and_leaves_the_chain_where_it_was() {
        let mut f = Funnel::new(100);
        f.narrow("priced", "priced", 40);
        f.branch("reached_target", "doubled in five minutes", "priced", 4);
        f.branch("migrated", "graduated", "priced", 9);

        assert_eq!(f.stage("reached_target").unwrap().removed, 36);
        // The claim that matters: nine migrations out of forty priced, not out of four
        // that reached the target -- which is not even a subset.
        assert_eq!(f.stage("migrated").unwrap().removed, 31);
        assert_eq!(f.stage("migrated").unwrap().of.as_deref(), Some("priced"));
        assert_eq!(f.remaining(), 40, "the chain still stands at `priced`");
    }

    /// The case that made branches necessary.
    #[test]
    fn a_branch_that_exceeds_its_sibling_is_still_stated_honestly() {
        // More tokens migrated than doubled inside five minutes. Chained, `migrated` would
        // have had to report "removed 0 of 4", implying 9 <= 4.
        let mut f = Funnel::new(100);
        f.narrow("priced", "priced", 40);
        f.branch("reached_target", "doubled in five minutes", "priced", 4);
        f.branch("migrated", "graduated", "priced", 9);
        assert!(
            f.stage("migrated").unwrap().remaining > f.stage("reached_target").unwrap().remaining
        );
    }

    #[test]
    fn a_stage_that_removes_nothing_is_still_recorded() {
        // Silence is not the same as "no launches were lost here", and a reader needs to
        // see that the stage ran.
        let mut f = Funnel::new(10);
        f.narrow("deployer_depth", "deep enough deployer history", 10);
        assert_eq!(f.stages.len(), 2);
        assert_eq!(f.stages[1].removed, 0);
    }
}
