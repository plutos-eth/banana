//! The writer lock, held **per database file rather than per directory**.
//!
//! Spec §4.1 asks for a lock on `data/` so two instances cannot corrupt each other. Taken
//! literally that conflicts with §10, which wants a long index to run from a terminal or a
//! cron job — because the desktop app would already hold the directory and the CLI could
//! never start (PLAN.md C3).
//!
//! The two databases have disjoint writers by design: the indexer writes `history.db`, the
//! engine writes `live.db`. So each file gets its own lock, and:
//!
//! * two indexers still cannot collide,
//! * two engines still cannot collide,
//! * `banana index` from cron runs happily while the app is open and trading,
//! * the app reads `history.db` through WAL the whole time it is being written.
//!
//! The lock is advisory and held for the lifetime of the guard. A process that dies
//! releases it, because the OS closes the file.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{db} is being written by another banana process ({path})")]
    Busy { db: String, path: String },
    #[error("cannot open lock file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// An exclusive writer lock on one database.
///
/// Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    _file: File,
    path: PathBuf,
}

impl Lock {
    /// Take the writer lock for `db_path`, failing immediately if another process holds it.
    ///
    /// Fails rather than waits: a second indexer starting is a mistake to report, not a
    /// queue to join, and a caller that genuinely wants to wait can retry.
    pub fn acquire(db_path: impl AsRef<Path>) -> Result<Self, LockError> {
        let db_path = db_path.as_ref();
        let path = lock_path(db_path);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockError::Io {
                path: path.display().to_string(),
                source,
            })?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => Ok(Self { _file: file, path }),
            Ok(false) => Err(LockError::Busy {
                db: db_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| db_path.display().to_string()),
                path: path.display().to_string(),
            }),
            Err(source) => Err(LockError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Whether the lock is currently free, without taking it.
    ///
    /// Used by the app to decide whether it may write `history.db` or must open it
    /// read-only because a CLI index holds it. Inherently racy, so it informs a message to
    /// the user rather than a decision about correctness -- the actual protection is
    /// [`Lock::acquire`] failing.
    pub fn is_free(db_path: impl AsRef<Path>) -> bool {
        Lock::acquire(db_path).is_ok()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The lock file sits beside the database, not inside it, so SQLite never sees it.
fn lock_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "db".into());
    p.set_file_name(format!(".{name}.lock"));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("banana-lock-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&d).unwrap();
        d.join("history.db")
    }

    #[test]
    fn a_second_writer_is_refused_while_the_first_holds_it() {
        let db = tmp("a");
        let first = Lock::acquire(&db).expect("first writer");
        let err = Lock::acquire(&db).expect_err("second writer must be refused");
        assert!(matches!(err, LockError::Busy { .. }), "{err}");
        assert!(err.to_string().contains("another banana process"));
        drop(first);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn dropping_the_guard_releases_the_lock() {
        let db = tmp("b");
        {
            let _g = Lock::acquire(&db).unwrap();
        }
        Lock::acquire(&db).expect("released on drop");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// This is the property that resolves the §4.1 / §10 conflict: `history.db` and
    /// `live.db` are locked independently, so a CLI index and the desktop engine can run
    /// at the same time.
    #[test]
    fn history_and_live_lock_independently() {
        let dir = tmp("c");
        let dir = dir.parent().unwrap();
        let history = dir.join("history.db");
        let live = dir.join("live.db");

        let _indexer = Lock::acquire(&history).expect("cli takes history");
        let _engine = Lock::acquire(&live).expect("app takes live at the same time");

        // ...and neither can be taken twice.
        assert!(Lock::acquire(&history).is_err());
        assert!(Lock::acquire(&live).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_lock_file_sits_beside_the_database_not_inside_it() {
        let db = tmp("d");
        let g = Lock::acquire(&db).unwrap();
        assert_eq!(g.path().file_name().unwrap(), ".history.db.lock");
        assert_eq!(g.path().parent(), db.parent(), "same directory");
        assert_ne!(g.path(), db.as_path(), "must never be the database itself");
        drop(g);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn acquiring_creates_the_data_directory_if_it_is_missing() {
        let dir = std::env::temp_dir().join(format!("banana-lock-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("history.db");
        let g = Lock::acquire(&db).expect("should create the directory");
        assert!(dir.exists());
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
