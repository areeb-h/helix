//! Kernel-held file locks — the half of mutual exclusion `create_new` cannot provide.
//!
//! **WHY THIS EXISTS.** ADR 0041 gave `create_new`, which is atomic and is the right answer
//! for a content-addressed write. As a LOCK it has one flaw, and it is the flaw that
//! matters: **a lock file does not release when its holder crashes.** The next process finds
//! a file that says "someone is writing" and cannot tell a live writer from a corpse. Every
//! remedy at that level is a guess — a PID that may have been reused, a timestamp that may
//! be a long GC pause, a heartbeat that is another thing to get wrong.
//!
//! A lock the KERNEL holds has no such ambiguity. It lives on an open file description, so
//! it is released when the descriptor closes: on `release()`, on the value being dropped, on
//! the process exiting, and on the process being killed. There is nothing to clean up and
//! nothing to guess, because the answer is not stored anywhere a crash can leave stale.
//!
//! **WHAT IT DOES NOT DO.** These are ADVISORY: they exclude other lock takers, not other
//! writers. A program that ignores the lock and writes anyway is not stopped, on any
//! operating system. That is what "advisory" means everywhere, and naming it here is
//! cheaper than discovering it during a corruption.
//!
//! Locks are also per-open-description, so two `lock_file` calls in ONE process are not
//! guaranteed to exclude each other the way two processes are — the guarantee this exists
//! for is between processes.

use std::cell::Cell;
use std::fs::File;

/// An acquired lock, alive as long as this handle is.
pub struct LockHandle {
    /// The open descriptor the kernel hangs the lock on. Dropping it releases the lock,
    /// which is the entire mechanism: nothing else has to run for the lock to go away.
    file: File,
    /// For diagnostics; a lock that cannot say what it holds is hard to debug.
    pub path: String,
    /// `release()` is idempotent, and a released handle must not release again on drop.
    released: Cell<bool>,
}

impl LockHandle {
    /// Open (creating if needed) and take an exclusive lock.
    ///
    /// `blocking` waits for the holder; otherwise `Ok(None)` means another process holds it
    /// right now — which is an ANSWER, not an error, and the reason the non-blocking form
    /// exists. A database that fails fast with "another process has this store open" is
    /// friendlier than one that hangs with no output.
    pub fn acquire(path: &str, blocking: bool) -> std::io::Result<Option<LockHandle>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if blocking {
            file.lock()?;
        } else {
            match file.try_lock() {
                Ok(()) => {}
                // Held elsewhere. Not a failure of this call.
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        Ok(Some(LockHandle { file, path: path.to_string(), released: Cell::new(false) }))
    }

    /// Release early. Idempotent, so a program that releases and then drops is fine.
    pub fn release(&self) -> std::io::Result<()> {
        if self.released.replace(true) {
            return Ok(());
        }
        self.file.unlock()
    }

    pub fn is_released(&self) -> bool {
        self.released.get()
    }
}

// NO `Drop` IMPL IS NEEDED, and writing one would be the mistake. Dropping `file` closes the
// descriptor, and closing is exactly what releases a kernel lock — the same path a crash
// takes. Adding an explicit `unlock()` on drop would make the ordinary case take a different
// route from the crash case, which is how the two stop being tested by the same code.
