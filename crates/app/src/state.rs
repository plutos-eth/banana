//! What the desktop process holds, and who is allowed to write to the store.
//!
//! Two rules shape this file, both from spec §4.1 and §4.2:
//!
//! 1. **The store has one writer.** The app opens `history.db` **read-only** and takes the
//!    writer lock only for as long as it is actually indexing. That is what lets a `quarrel
//!    index` run from cron while the desktop app is open (PLAN.md C3), and it means the
//!    window can be closed mid-index without leaving a lock behind.
//! 2. **The engine is not the window.** Its state lives here, not in the UI, so closing or
//!    reloading the window is a view change and nothing more.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use quarrel_core::strategy::StrategyConfig;
use quarrel_store::{History, Lock};

/// Whether real money can move. Always visible in the UI (spec §3.2, §8 view 6).
///
/// Three states, not two, because PLAN.md C7 established that `--live` and `arm` are
/// different gates and conflating them is how "armed" gets mistaken for "live":
///
/// * `--live` is a **process launch flag**. Without it a process can never spend; it has
///   to be relaunched. That is what "never a button" means.
/// * `arm` is typed **inside** an already-live process, once, after the briefing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Launched without `--live`. Holds no key and cannot be armed at any price.
    DryRun,
    /// Launched with `--live` but not yet armed. Still cannot spend.
    LiveNotArmed,
    /// Armed. Real money moves.
    LiveArmed,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::DryRun => "DRY RUN",
            Mode::LiveNotArmed => "LIVE — NOT ARMED",
            Mode::LiveArmed => "LIVE — ARMED",
        }
    }

    /// The only state in which a transaction can be signed.
    pub fn can_spend(self) -> bool {
        matches!(self, Mode::LiveArmed)
    }

    /// What the user is told about this state, in the terms C7 asks for.
    pub fn explain(self) -> &'static str {
        match self {
            Mode::DryRun => {
                "Everything runs and nothing can be signed: this process holds no key.                  Relaunch with --live to be able to arm."
            }
            Mode::LiveNotArmed => {
                "Launched with --live, so a key is loaded, but nothing fires until you                  read the briefing and type the arm phrase."
            }
            Mode::LiveArmed => "Armed. Entries will be signed and sent, inside the session budget.",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] quarrel_store::StoreError),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Backtest(#[from] quarrel_backtest::BacktestError),
    #[error("no store at {0}; run an index first")]
    NoStore(String),
    #[error("an index is already running")]
    IndexBusy,
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

/// Tauri needs a plain string on the wire; the UI shows it verbatim.
impl serde::Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// Everything the window is a view over.
pub struct AppState {
    data_dir: PathBuf,
    /// Read-only handle, opened lazily and dropped while an index holds the writer lock.
    history: Mutex<Option<History>>,
    strategy: Mutex<StrategyConfig>,
    /// Set for the duration of an index, so a second one is refused rather than queued.
    indexing: AtomicBool,
    mode: Mutex<Mode>,
}

impl AppState {
    /// `live` comes from the `--live` launch flag and from nowhere else.
    pub fn new(data_dir: impl AsRef<Path>, live: bool) -> Self {
        let data_dir = data_dir.as_ref().to_path_buf();
        let strategy = load_strategy(&data_dir).unwrap_or_default();
        Self {
            data_dir,
            history: Mutex::new(None),
            strategy: Mutex::new(strategy),
            indexing: AtomicBool::new(false),
            // Decided at launch and never afterwards: a process without `--live` cannot
            // be talked into being one with it.
            mode: Mutex::new(if live {
                Mode::LiveNotArmed
            } else {
                Mode::DryRun
            }),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("history.db")
    }

    pub fn strategy_path(&self) -> PathBuf {
        self.data_dir.join("strategy.json")
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode lock")
    }

    /// Arm a live process. Refuses in dry run, which is the C7 rule made mechanical.
    ///
    /// Returns what the user should be shown: either the new mode, or why not.
    pub fn arm(&self, typed: &str) -> Result<Mode> {
        let mut mode = self.mode.lock().expect("mode lock");
        match *mode {
            Mode::DryRun => Err(AppError::Refused(
                "this process was launched without --live and cannot be armed. Close it                  and relaunch with --live if you mean to trade."
                    .into(),
            )),
            Mode::LiveArmed => Ok(Mode::LiveArmed),
            Mode::LiveNotArmed => {
                if !quarrel_live::phrase_arms(typed) {
                    return Err(AppError::Refused(format!(
                        "not armed: the phrase is `{}` and nothing else",
                        quarrel_live::ARM_PHRASE
                    )));
                }
                *mode = Mode::LiveArmed;
                Ok(Mode::LiveArmed)
            }
        }
    }

    /// True when a `strategy.json` already exists, which is what decides whether the
    /// first-run questionnaire is shown (spec §8, "First run").
    pub fn has_saved_strategy(&self) -> bool {
        self.strategy_path().is_file()
    }

    pub fn strategy(&self) -> StrategyConfig {
        self.strategy.lock().expect("strategy lock").clone()
    }

    /// Replace the strategy and write it to disk.
    ///
    /// The same file the Lab reads and the sniper will arm from — spec §7.2 says arming a
    /// backtested strategy is loading this file with no translation step, so there is
    /// exactly one of them.
    pub fn save_strategy(&self, config: &StrategyConfig) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let json = serde_json::to_string_pretty(config)?;
        std::fs::write(self.strategy_path(), json)?;
        *self.strategy.lock().expect("strategy lock") = config.clone();
        Ok(())
    }

    /// Borrow the read-only store, opening it if needed.
    ///
    /// Returns [`AppError::NoStore`] rather than creating an empty database: a view that
    /// silently reported zero launches would be indistinguishable from a chain with none.
    pub fn with_history<T>(&self, f: impl FnOnce(&History) -> Result<T>) -> Result<T> {
        let mut slot = self.history.lock().expect("history lock");
        if slot.is_none() {
            let path = self.db_path();
            if !path.is_file() {
                return Err(AppError::NoStore(path.display().to_string()));
            }
            *slot = Some(History::open_read_only(&path)?);
        }
        f(slot.as_ref().expect("just opened"))
    }

    /// Close the read-only handle, so an index can take the writer lock.
    pub fn release_history(&self) {
        *self.history.lock().expect("history lock") = None;
    }

    /// Claim the right to index. Released by dropping the returned guard.
    pub fn begin_index(&self) -> Result<IndexGuard<'_>> {
        if self.indexing.swap(true, Ordering::SeqCst) {
            return Err(AppError::IndexBusy);
        }
        self.release_history();
        match Lock::acquire(self.db_path()) {
            Ok(lock) => Ok(IndexGuard {
                state: self,
                _lock: lock,
            }),
            Err(e) => {
                self.indexing.store(false, Ordering::SeqCst);
                Err(AppError::Refused(format!(
                    "another quarrel process is writing to this store ({e})"
                )))
            }
        }
    }

    pub fn is_indexing(&self) -> bool {
        self.indexing.load(Ordering::SeqCst)
    }
}

/// Held for the duration of an index; releases the writer lock and the busy flag together.
///
/// A guard rather than a pair of calls, so an early return in the index path cannot leave
/// the store locked against the user's own CLI.
pub struct IndexGuard<'a> {
    state: &'a AppState,
    _lock: Lock,
}

impl Drop for IndexGuard<'_> {
    fn drop(&mut self) {
        self.state.indexing.store(false, Ordering::SeqCst);
        // The next read reopens; the writer's WAL is already checkpointed by then.
        self.state.release_history();
    }
}

fn load_strategy(data_dir: &Path) -> Option<StrategyConfig> {
    let raw = std::fs::read_to_string(data_dir.join("strategy.json")).ok()?;
    match serde_json::from_str(&raw) {
        Ok(c) => Some(c),
        Err(e) => {
            // Not fatal, and not silent: a strategy that will not parse must not be
            // replaced by the default without the user being told, or they would be
            // trading a configuration they did not choose.
            tracing::error!(error = %e, "strategy.json did not parse; using the baseline");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("quarrel-app-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_fresh_install_has_no_saved_strategy_and_uses_the_baseline() {
        let d = temp_dir("fresh");
        let s = AppState::new(&d, false);
        assert!(!s.has_saved_strategy(), "this is what triggers onboarding");
        assert_eq!(s.strategy(), StrategyConfig::default());
    }

    #[test]
    fn a_saved_strategy_round_trips_through_the_file_the_sniper_will_read() {
        let d = temp_dir("save");
        let s = AppState::new(&d, false);
        let mut cfg = StrategyConfig::default();
        cfg.entry_model.max_tax_bps = 175;
        s.save_strategy(&cfg).unwrap();

        assert!(s.has_saved_strategy());
        assert_eq!(s.strategy().entry_model.max_tax_bps, 175);
        // Reopening reads the same file, with no translation step (spec §7.2).
        assert_eq!(AppState::new(&d, false).strategy(), cfg);
    }

    #[test]
    fn an_unparseable_strategy_falls_back_loudly_rather_than_silently() {
        let d = temp_dir("broken");
        std::fs::write(d.join("strategy.json"), "{ not json").unwrap();
        let s = AppState::new(&d, false);
        assert_eq!(s.strategy(), StrategyConfig::default());
        // The file is left alone: overwriting the user's config on a parse error would
        // destroy the thing they need in order to fix it.
        assert!(s.strategy_path().is_file());
    }

    #[test]
    fn a_missing_store_is_an_error_not_an_empty_database() {
        let d = temp_dir("nostore");
        let s = AppState::new(&d, false);
        let e = s.with_history(|_| Ok(()));
        assert!(matches!(e, Err(AppError::NoStore(_))));
        // And no file was created by asking.
        assert!(!s.db_path().exists());
    }

    #[test]
    fn a_second_index_is_refused_while_the_first_holds_the_lock() {
        let d = temp_dir("busy");
        let s = AppState::new(&d, false);
        let guard = s.begin_index().unwrap();
        assert!(s.is_indexing());
        assert!(matches!(s.begin_index(), Err(AppError::IndexBusy)));
        drop(guard);
        assert!(!s.is_indexing(), "the guard releases on drop");
        assert!(s.begin_index().is_ok(), "and the next one can start");
    }

    #[test]
    fn a_process_without_the_flag_is_dry_run_and_says_so() {
        let d = temp_dir("mode");
        let s = AppState::new(&d, false);
        assert_eq!(s.mode(), Mode::DryRun);
        assert!(!s.mode().can_spend());
        assert_eq!(Mode::DryRun.label(), "DRY RUN");
        assert!(s.mode().explain().contains("holds no key"));
    }

    /// PLAN.md C7: the two gates are different, and a dry-run process cannot cross either.
    #[test]
    fn a_dry_run_process_cannot_be_armed_at_any_price() {
        let d = temp_dir("armdry");
        let s = AppState::new(&d, false);
        let e = s.arm("arm").unwrap_err();
        assert!(matches!(e, AppError::Refused(_)));
        assert!(e.to_string().contains("relaunch with --live"), "{e}");
        assert_eq!(s.mode(), Mode::DryRun, "and it stays dry run");
    }

    #[test]
    fn a_live_process_starts_unarmed_and_cannot_spend_until_the_phrase() {
        let d = temp_dir("armlive");
        let s = AppState::new(&d, true);
        assert_eq!(s.mode(), Mode::LiveNotArmed);
        assert!(!s.mode().can_spend(), "a key is loaded but nothing fires");

        for wrong in ["", "y", "yes", "ARM", "arm it"] {
            assert!(s.arm(wrong).is_err(), "{wrong:?} armed the session");
            assert_eq!(s.mode(), Mode::LiveNotArmed);
        }

        assert_eq!(s.arm("arm").unwrap(), Mode::LiveArmed);
        assert!(s.mode().can_spend());
        // Arming twice is not an error; it is already done.
        assert_eq!(s.arm("arm").unwrap(), Mode::LiveArmed);
    }

    #[test]
    fn each_mode_says_what_it_means_rather_than_only_naming_itself() {
        for m in [Mode::DryRun, Mode::LiveNotArmed, Mode::LiveArmed] {
            assert!(!m.label().is_empty());
            assert!(m.explain().len() > 40, "{m:?} explains nothing");
        }
        // "armed" must never read as "live" and vice versa (C7).
        assert_ne!(Mode::LiveNotArmed.label(), Mode::LiveArmed.label());
        assert!(Mode::LiveNotArmed.label().contains("NOT ARMED"));
    }
}
