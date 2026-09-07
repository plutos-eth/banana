//! Weighted progress across the index phases.
//!
//! Spec §6.2 warns that "a progress percentage computed linearly over blocks will lie",
//! and it is right: the phases differ by orders of magnitude in cost per block. Measured
//! on 2026-09-07 for a 24-hour window (`docs/FINDINGS.md` §3):
//!
//! | phase | requests | transfer | wall clock |
//! |---|---|---|---|
//! | A launches, graduations, sweeps | ~10-45 | ~20 MB | < 1 min |
//! | B trades and snipe tax | ~430 | ~740 MB | ~9-12 min |
//! | C launch calldata, one tx per launch | ~20,000 | ~68 MB | ~7-12 min |
//! | D block-timestamp anchors | ~1,700 | ~2 MB | ~1 min |
//!
//! **Four phases, not the two §6.2 assumes** (PLAN.md D4). Phase C exists because §5.1 and
//! §5.3 need launch calldata and it lives nowhere else; §6.1 does not cost it at all, and
//! it is roughly half the wall clock. Weighting by wall clock rather than by request count
//! is deliberate: the user is watching a clock, not a request counter, and C's 20,000
//! cheap concurrent requests take about as long as B's 430 expensive serial ones.

use std::time::{Duration, Instant};

/// The four phases, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Factory-filtered logs: `TokenLaunched`, `PoolGraduated`, `LaunchSwept`.
    Launches,
    /// Topic-filtered across all curve addresses: `CurveBuy`, `CurveSell`,
    /// `SnipeTaxCharged`.
    Trades,
    /// One `eth_getTransactionByHash` per launch, for point-in-time metadata.
    Calldata,
    /// Sampled block headers, for timestamp interpolation.
    Anchors,
}

impl Phase {
    pub const ALL: [Phase; 4] = [
        Phase::Launches,
        Phase::Trades,
        Phase::Calldata,
        Phase::Anchors,
    ];

    /// The key used in `index_state`, so a resume finds its phase.
    pub fn key(self) -> &'static str {
        match self {
            Phase::Launches => "launches",
            Phase::Trades => "trades",
            Phase::Calldata => "calldata",
            Phase::Anchors => "anchors",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Launches => "launches",
            Phase::Trades => "trades",
            Phase::Calldata => "launch calldata",
            Phase::Anchors => "block anchors",
        }
    }

    /// Share of total wall clock, in per-mille. Sums to 1000.
    ///
    /// From the measured table above, using the midpoint of each range.
    pub fn weight(self) -> u32 {
        match self {
            Phase::Launches => 20,  // < 1 min of ~22
            Phase::Trades => 480,   // ~10.5 min
            Phase::Calldata => 450, // ~9.5 min
            Phase::Anchors => 50,   // ~1 min
        }
    }
}

/// What one phase has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct PhaseProgress {
    pub units_done: u64,
    pub units_total: u64,
    pub rows_written: u64,
    /// Whether `begin` has been called.
    ///
    /// Without this, a phase that has not started yet is indistinguishable from one that
    /// started with nothing to do -- both have `units_total == 0` -- and every un-started
    /// phase would count as complete, pinning the bar at 100% from the first tick.
    pub started: bool,
}

impl PhaseProgress {
    fn fraction_permille(&self) -> u32 {
        if !self.started {
            return 0;
        }
        if self.units_total == 0 {
            // Started with no work to do: genuinely complete.
            return 1_000;
        }
        let d = self.units_done.min(self.units_total);
        ((d as u128 * 1_000) / self.units_total as u128) as u32
    }
}

/// The event the UI renders as a progress bar (spec §6.2).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProgressEvent {
    pub phase: Phase,
    pub phase_label: &'static str,
    pub units_done: u64,
    pub units_total: u64,
    /// Overall completion in per-mille, weighted across all four phases.
    pub percent_x10: u32,
    pub rows_written: u64,
    pub eta_secs: Option<u64>,
}

/// Tracks all four phases and produces weighted overall progress.
#[derive(Debug)]
pub struct Progress {
    phases: [(Phase, PhaseProgress); 4],
    current: Phase,
    started: Instant,
    /// Set when a phase is skipped, so its weight is not counted as outstanding.
    skipped: [bool; 4],
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress {
    pub fn new() -> Self {
        Self {
            phases: Phase::ALL.map(|p| (p, PhaseProgress::default())),
            current: Phase::Launches,
            started: Instant::now(),
            skipped: [false; 4],
        }
    }

    fn index(p: Phase) -> usize {
        Phase::ALL.iter().position(|x| *x == p).expect("phase")
    }

    pub fn begin(&mut self, phase: Phase, units_total: u64) {
        self.current = phase;
        let i = Self::index(phase);
        self.phases[i].1.units_total = units_total;
        self.phases[i].1.started = true;
    }

    /// Mark a phase as not running at all (for example `--skip-calldata`).
    ///
    /// Its weight is removed from the total rather than counted as instantly complete, so
    /// the remaining phases still add up to 100%.
    pub fn skip(&mut self, phase: Phase) {
        self.skipped[Self::index(phase)] = true;
    }

    pub fn advance(&mut self, phase: Phase, units: u64, rows: u64) {
        let i = Self::index(phase);
        self.phases[i].1.units_done += units;
        self.phases[i].1.rows_written += rows;
    }

    pub fn set_done(&mut self, phase: Phase, units_done: u64) {
        let i = Self::index(phase);
        self.phases[i].1.units_done = units_done;
    }

    pub fn finish(&mut self, phase: Phase) {
        let i = Self::index(phase);
        self.phases[i].1.units_done = self.phases[i].1.units_total;
    }

    fn active_weight(&self) -> u32 {
        Phase::ALL
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.skipped[*i])
            .map(|(_, p)| p.weight())
            .sum()
    }

    /// Overall completion in per-mille, weighted by measured cost.
    pub fn overall_permille(&self) -> u32 {
        let total = self.active_weight();
        if total == 0 {
            return 1_000;
        }
        let done: u64 = self
            .phases
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.skipped[*i])
            .map(|(_, (phase, prog))| phase.weight() as u64 * prog.fraction_permille() as u64)
            .sum();
        ((done / total as u64) as u32).min(1_000)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Remaining time, extrapolated from overall progress so far.
    ///
    /// `None` until enough has happened for the estimate to mean anything -- an ETA
    /// computed from 0.3% done is noise, and showing it is worse than showing nothing.
    pub fn eta(&self) -> Option<Duration> {
        let done = self.overall_permille();
        if !(20..1_000).contains(&done) {
            return None;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let total = elapsed * 1_000.0 / done as f64;
        Some(Duration::from_secs_f64((total - elapsed).max(0.0)))
    }

    pub fn event(&self) -> ProgressEvent {
        let i = Self::index(self.current);
        let p = self.phases[i].1;
        ProgressEvent {
            phase: self.current,
            phase_label: self.current.label(),
            units_done: p.units_done,
            units_total: p.units_total,
            percent_x10: self.overall_permille(),
            rows_written: self.phases.iter().map(|(_, x)| x.rows_written).sum(),
            eta_secs: self.eta().map(|d| d.as_secs()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_weights_sum_to_one_thousand() {
        let total: u32 = Phase::ALL.iter().map(|p| p.weight()).sum();
        assert_eq!(total, 1_000);
    }

    /// The point of weighting at all: finishing the cheap phase must not claim a quarter
    /// of the work is done.
    #[test]
    fn finishing_the_cheap_phase_barely_moves_the_bar() {
        let mut p = Progress::new();
        p.begin(Phase::Launches, 100);
        p.finish(Phase::Launches);
        assert_eq!(
            p.overall_permille(),
            20,
            "launches are 2% of the wall clock, not 25%"
        );
    }

    #[test]
    fn finishing_the_expensive_phase_moves_it_a_lot() {
        let mut p = Progress::new();
        p.begin(Phase::Trades, 100);
        p.finish(Phase::Trades);
        assert_eq!(p.overall_permille(), 480);
    }

    #[test]
    fn progress_is_monotonic_across_a_whole_run() {
        let mut p = Progress::new();
        let mut last = 0;
        for phase in Phase::ALL {
            p.begin(phase, 1_000);
            for _ in 0..10 {
                p.advance(phase, 100, 5);
                let now = p.overall_permille();
                assert!(now >= last, "progress went backwards: {last} -> {now}");
                last = now;
            }
        }
        assert_eq!(last, 1_000, "a completed run must read 100%");
    }

    #[test]
    fn a_skipped_phase_removes_its_weight_rather_than_counting_as_done() {
        // --skip-calldata must still let the other phases reach 100%.
        let mut p = Progress::new();
        p.skip(Phase::Calldata);
        for phase in [Phase::Launches, Phase::Trades, Phase::Anchors] {
            p.begin(phase, 10);
            p.finish(phase);
        }
        assert_eq!(p.overall_permille(), 1_000);
    }

    #[test]
    fn overshooting_a_phase_cannot_push_it_past_complete() {
        let mut p = Progress::new();
        p.begin(Phase::Trades, 100);
        p.advance(Phase::Trades, 10_000, 0);
        assert_eq!(p.overall_permille(), 480, "clamped at the phase weight");
    }

    #[test]
    fn a_phase_with_no_work_counts_as_complete() {
        // An update covering zero blocks must not stall the bar at 0.
        let mut p = Progress::new();
        p.begin(Phase::Launches, 0);
        assert_eq!(p.overall_permille(), 20);
    }

    #[test]
    fn there_is_no_eta_until_the_estimate_means_something() {
        let mut p = Progress::new();
        p.begin(Phase::Trades, 1_000);
        assert_eq!(p.eta(), None, "an ETA from 0% is noise");
        p.advance(Phase::Trades, 5, 0);
        assert_eq!(p.eta(), None, "still too early");
    }

    #[test]
    fn there_is_no_eta_once_finished() {
        let mut p = Progress::new();
        for phase in Phase::ALL {
            p.begin(phase, 1);
            p.finish(phase);
        }
        assert_eq!(p.overall_permille(), 1_000);
        assert_eq!(p.eta(), None);
    }

    #[test]
    fn the_event_reports_the_current_phase_and_total_rows() {
        let mut p = Progress::new();
        p.begin(Phase::Launches, 10);
        p.advance(Phase::Launches, 10, 300);
        p.begin(Phase::Trades, 100);
        p.advance(Phase::Trades, 50, 9_000);

        let e = p.event();
        assert_eq!(e.phase, Phase::Trades);
        assert_eq!(e.phase_label, "trades");
        assert_eq!(e.units_done, 50);
        assert_eq!(e.units_total, 100);
        assert_eq!(e.rows_written, 9_300, "rows are cumulative across phases");
        assert_eq!(e.percent_x10, 20 + 240);
    }

    #[test]
    fn phase_keys_are_stable_so_a_resume_finds_its_row() {
        assert_eq!(Phase::Launches.key(), "launches");
        assert_eq!(Phase::Trades.key(), "trades");
        assert_eq!(Phase::Calldata.key(), "calldata");
        assert_eq!(Phase::Anchors.key(), "anchors");
    }
}
