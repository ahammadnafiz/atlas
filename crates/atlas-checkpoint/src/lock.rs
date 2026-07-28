//! One writer per Workspace.
//!
//! Atlas is a multi-window app and two windows can hold the same Workspace open.
//! Two capture loops writing one SQLite file with no coordination corrupts the
//! outbox state machine — both would claim the same pending rows, and a row can
//! be marked `sent` by one process while the other is still trying to send it.
//! WAL mode makes that concurrency *possible*, which is exactly the problem;
//! it serialises writes, it does not make two independent writers agree.
//!
//! The lock is an exclusive SQLite transaction held open on a tiny sidecar
//! database for the lifetime of the store. That gets a real, OS-level,
//! automatically-released lock without adding a file-locking dependency, and
//! without the cross-platform footguns of advisory locks on the main database
//! (which WAL would happily let both processes take).
//!
//! Release is by drop, including on a crash — the OS closes the handle, SQLite
//! rolls the transaction back, and the next window to open takes over. There is
//! deliberately no stale-lock timeout, because a timeout is exactly how you get
//! two writers.

use std::path::Path;

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Holds the Workspace's writer lock. Dropping it releases the lock.
pub struct WriterLock {
    // Held for its Drop. The open exclusive transaction *is* the lock.
    _conn: Connection,
}

impl WriterLock {
    /// Take the lock, or report that another process has it.
    ///
    /// Never blocks: a second window should fall back to read-only immediately
    /// and say so, not hang waiting for the first one to close.
    pub fn acquire(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| Error::Storage(format!("{}: {e}", path.display())))?;

        // Fail fast rather than queue. A waiting second window looks identical
        // to a hung one from the outside.
        conn.busy_timeout(std::time::Duration::from_millis(0))
            .map_err(|e| Error::Storage(e.to_string()))?;

        // BEGIN EXCLUSIVE takes the write lock immediately rather than on first
        // write, which is what makes this usable as a lock at all.
        match conn.execute_batch("BEGIN EXCLUSIVE") {
            Ok(()) => Ok(Self { _conn: conn }),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if matches!(
                    err.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Err(Error::AlreadyLocked)
            }
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquire_is_refused_rather_than_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.lock");

        let first = WriterLock::acquire(&path).expect("first writer takes the lock");

        let started = std::time::Instant::now();
        let second = WriterLock::acquire(&path);
        assert!(
            matches!(second, Err(Error::AlreadyLocked)),
            "second writer should be refused, got {:?}",
            second.err()
        );
        assert!(
            started.elapsed().as_millis() < 500,
            "refusal should be immediate, took {:?}",
            started.elapsed()
        );

        drop(first);
    }

    #[test]
    fn dropping_the_lock_lets_the_next_writer_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.lock");

        drop(WriterLock::acquire(&path).unwrap());
        WriterLock::acquire(&path).expect("lock should be free after drop");
    }
}
