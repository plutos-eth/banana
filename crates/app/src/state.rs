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
/// Re-exported from `quarrel-live` rather than defined again here: the mode the UI shows
/// and the mode the executor obeys have to be one type, or they can disagree.
pub use quarrel_live::Mode;

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
    /// `None` until the startup screen has been answered. Set once, then fixed.
    mode: Mutex<Option<Mode>>,
    /// The running sniper, if one is. Not in the window: closing it must not stop a
    /// position being watched (spec section 4.2).
    engine: crate::sniper::Handle,
}

impl AppState {
    /// A process with no mode yet. The user picks one on the startup screen.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref().to_path_buf();
        // Made here rather than at the first write. A first-run user is told in the top
        // bar where their data lives; being told about a directory that does not exist
        // yet is a small lie, and every writer below would otherwise have to create it
        // separately and remember to.
        //
        // Ignored on failure: a read-only or unwritable location is a real problem, but
        // it is one the first actual write reports with a path and a reason, which beats
        // a panic before the window opens.
        let _ = std::fs::create_dir_all(&data_dir);
        let strategy = load_strategy(&data_dir).unwrap_or_default();
        Self {
            data_dir,
            history: Mutex::new(None),
            strategy: Mutex::new(strategy),
            indexing: AtomicBool::new(false),
            // No mode until the user chooses one, and no way back afterwards.
            mode: Mutex::new(None),
            engine: crate::sniper::Handle::default(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn engine(&self) -> &crate::sniper::Handle {
        &self.engine
    }

    pub fn journal_path(&self) -> PathBuf {
        self.data_dir.join("live.db")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("history.db")
    }

    pub fn strategy_path(&self) -> PathBuf {
        self.data_dir.join("strategy.json")
    }

    /// The chosen mode, or `None` while the startup screen is still showing.
    pub fn mode(&self) -> Option<Mode> {
        *self.mode.lock().expect("mode lock")
    }

    /// Choose TEST or LIVE. Once, for the life of the process.
    ///
    /// Refusing a second choice is the point: a session that could be switched to LIVE
    /// while running is a session that can start spending money it was not started to
    /// spend. Changing your mind means restarting, which costs a few seconds and removes
    /// a whole class of accident.
    ///
    /// Choosing LIVE with no key configured fails here rather than later, so the user
    /// finds out before they are watching a feed that will never fire.
    pub fn choose_mode(&self, wanted: Mode) -> Result<Mode> {
        let mut mode = self.mode.lock().expect("mode lock");
        if let Some(current) = *mode {
            return if current == wanted {
                Ok(current)
            } else {
                Err(AppError::Refused(format!(
                    "this session is already running in {}. Restart to change mode.",
                    current.label()
                )))
            };
        }
        if wanted == Mode::Live {
            // Constructing a live session is what reads the key; the error it returns
            // names what is missing.
            quarrel_live::Session::live(self.strategy().live_guards, &self.data_dir)
                .map_err(|e| AppError::Refused(e.to_string()))?;
        }
        *mode = Some(wanted);
        Ok(wanted)
    }

    /// The wallet the saved key derives, or `None` when there is not one.
    ///
    /// An address, never the key: this is what crosses the IPC boundary, so a screenshot
    /// or a screen share cannot leak the thing that spends the money.
    pub fn wallet(&self) -> Option<alloy_primitives::Address> {
        quarrel_live::keystore::address(&self.data_dir)
    }

    /// Store a key pasted into Settings, and return the address it derives.
    ///
    /// Refused while a LIVE session is running: the session already holds the old key in
    /// memory, so writing a new one would leave the file and the running wallet
    /// disagreeing about which account is being spent from.
    pub fn save_key(&self, raw: &str) -> Result<alloy_primitives::Address> {
        if self.mode() == Some(Mode::Live) || self.engine.is_running() {
            return Err(AppError::Refused(
                "this session is already running in LIVE with the current key. Restart \
                 before changing wallets."
                    .into(),
            ));
        }
        quarrel_live::keystore::save(&self.data_dir, raw)
            .map_err(|e| AppError::Refused(e.to_string()))
    }

    /// Forget the stored key.
    pub fn clear_key(&self) -> Result<()> {
        if self.mode() == Some(Mode::Live) {
            return Err(AppError::Refused(
                "this session is running in LIVE with this key. Restart before removing it.".into(),
            ));
        }
        quarrel_live::keystore::clear(&self.data_dir).map_err(|e| AppError::Refused(e.to_string()))
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
        let s = AppState::new(&d);
        assert!(!s.has_saved_strategy(), "this is what triggers onboarding");
        assert_eq!(s.strategy(), StrategyConfig::default());
    }

    #[test]
    fn a_saved_strategy_round_trips_through_the_file_the_sniper_will_read() {
        let d = temp_dir("save");
        let s = AppState::new(&d);
        let mut cfg = StrategyConfig::default();
        cfg.entry_model.max_tax_bps = 175;
        s.save_strategy(&cfg).unwrap();

        assert!(s.has_saved_strategy());
        assert_eq!(s.strategy().entry_model.max_tax_bps, 175);
        // Reopening reads the same file, with no translation step (spec §7.2).
        assert_eq!(AppState::new(&d).strategy(), cfg);
    }

    #[test]
    fn an_unparseable_strategy_falls_back_loudly_rather_than_silently() {
        let d = temp_dir("broken");
        std::fs::write(d.join("strategy.json"), "{ not json").unwrap();
        let s = AppState::new(&d);
        assert_eq!(s.strategy(), StrategyConfig::default());
        // The file is left alone: overwriting the user's config on a parse error would
        // destroy the thing they need in order to fix it.
        assert!(s.strategy_path().is_file());
    }

    #[test]
    fn a_missing_store_is_an_error_not_an_empty_database() {
        let d = temp_dir("nostore");
        let s = AppState::new(&d);
        let e = s.with_history(|_| Ok(()));
        assert!(matches!(e, Err(AppError::NoStore(_))));
        // And no file was created by asking.
        assert!(!s.db_path().exists());
    }

    #[test]
    fn a_second_index_is_refused_while_the_first_holds_the_lock() {
        let d = temp_dir("busy");
        let s = AppState::new(&d);
        let guard = s.begin_index().unwrap();
        assert!(s.is_indexing());
        assert!(matches!(s.begin_index(), Err(AppError::IndexBusy)));
        drop(guard);
        assert!(!s.is_indexing(), "the guard releases on drop");
        assert!(s.begin_index().is_ok(), "and the next one can start");
    }

    #[test]
    fn a_fresh_process_has_no_mode_until_the_user_picks_one() {
        let d = temp_dir("mode");
        let s = AppState::new(&d);
        assert_eq!(s.mode(), None, "the startup screen keys on this");
    }

    #[test]
    fn choosing_test_gives_a_session_that_holds_no_key() {
        let d = temp_dir("modetest");
        let s = AppState::new(&d);
        assert_eq!(s.choose_mode(Mode::Test).unwrap(), Mode::Test);
        assert_eq!(s.mode(), Some(Mode::Test));
        assert!(!Mode::Test.can_spend());
        assert!(Mode::Test.explain().contains("holds no key"));
    }

    /// Changing mode means restarting, which removes a whole class of accident.
    #[test]
    fn the_mode_cannot_be_changed_once_it_is_chosen() {
        let d = temp_dir("modeonce");
        let s = AppState::new(&d);
        s.choose_mode(Mode::Test).unwrap();

        // Choosing the same thing again is not an error; it is already done.
        assert_eq!(s.choose_mode(Mode::Test).unwrap(), Mode::Test);

        let e = s.choose_mode(Mode::Live).unwrap_err();
        assert!(e.to_string().contains("Restart to change mode"), "{e}");
        assert_eq!(s.mode(), Some(Mode::Test), "and it stays in TEST");
    }

    #[test]
    fn choosing_live_without_a_key_fails_at_the_choice_rather_than_later() {
        // Better here than in front of a feed that will never fire.
        //
        // The question is asked through `quarrel-live` rather than by naming the variable,
        // because `scripts/check-trust-boundary.ps1` fails on that literal anywhere but
        // that crate — and the check is deliberately blunt: one with exceptions is one you
        // can talk your way past. It caught this line when it was written the other way.
        if quarrel_live::keystore::env_key_present() {
            return; // a machine with a key configured cannot exercise this
        }
        let d = temp_dir("modelive");
        let s = AppState::new(&d);
        let e = s.choose_mode(Mode::Live).unwrap_err();
        assert!(e.to_string().contains("Add one in Settings"), "{e}");
        assert_eq!(s.mode(), None, "a failed choice leaves the mode unchosen");
    }

    /// The friendly path: a key set from inside the application, never from a file the
    /// user has to find.
    #[test]
    fn a_key_saved_in_settings_makes_live_available_and_never_leaves_the_backend() {
        let d = temp_dir("wallet");
        let s = AppState::new(&d);
        assert_eq!(s.wallet(), None);

        // The published Hardhat development account. Public, and worthless.
        let key = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let addr = s.save_key(key).unwrap();
        assert_eq!(
            addr,
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
        assert_eq!(s.wallet(), Some(addr));

        // With a wallet configured, LIVE is now a choice that can succeed.
        assert_eq!(s.choose_mode(Mode::Live).unwrap(), Mode::Live);

        // And once live, the wallet cannot be swapped underneath the running session.
        assert!(s.save_key(key).is_err());
        assert!(s.clear_key().is_err());
    }

    #[test]
    fn a_mistyped_key_is_refused_where_it_was_pasted() {
        let d = temp_dir("badkey");
        let s = AppState::new(&d);
        assert!(s.save_key("not-a-key").is_err());
        assert_eq!(s.wallet(), None, "nothing was stored");
    }

    #[test]
    fn the_two_modes_say_what_they_mean_rather_than_only_naming_themselves() {
        assert_eq!(Mode::Test.label(), "TEST");
        assert_eq!(Mode::Live.label(), "LIVE");
        for m in [Mode::Test, Mode::Live] {
            assert!(m.explain().len() > 40, "{m:?} explains nothing");
        }
    }
}
