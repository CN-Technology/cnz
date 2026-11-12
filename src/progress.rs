//! Progress and cancellation of an operation, shared between the engine and
//! the interface: the command line today, the graphical application
//! tomorrow. The engine only keeps the counters up to date and regularly
//! checks whether it is asked to stop.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::Result;

#[derive(Default)]
pub struct Progress {
    phase: Mutex<&'static str>,
    total: AtomicU64,
    done: AtomicU64,
    cancelled: AtomicBool,
}

impl Progress {
    /// Start of a phase: its name and the amount of work (in bytes).
    pub(crate) fn start(&self, phase: &'static str, total: u64) {
        // Counters first, phase name second: the interface never sees the new phase
        // with the old one's figures.
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        *self.phase.lock().unwrap() = phase;
    }

    pub(crate) fn advance(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    /// (phase, done, total) at a given moment.
    pub fn snapshot(&self) -> (&'static str, u64, u64) {
        (*self.phase.lock().unwrap(), self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }

    /// Requests a stop. Returns true if a stop had already been requested.
    pub fn cancel(&self) -> bool {
        self.cancelled.swap(true, Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// "Interrupted" error if a stop was requested.
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(Cancelled.into());
        }
        Ok(())
    }
}

/// Operation interrupted at the user's request.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("opération interrompue")
    }
}

impl std::error::Error for Cancelled {}
