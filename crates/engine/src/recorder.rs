//! The sole journal writer.
//!
//! Wraps a [`JournalSink`] so every append passes through one place and one
//! tracing span, and **feeds the [`Monitor`] on the way**.
//!
//! The monitor lives here and nowhere else. That is what makes "the monitor runs
//! on every run" structural rather than something the loop remembers to do, and
//! it is why a second copy would be wrong rather than redundant: it would see a
//! subset of the records and its properties would be checking a history that
//! never happened.

use std::io;

use journal::{JournalSink, Record};
use policy::Policy;
use tracing::{trace_span, warn};

use crate::monitor::{Monitor, Violation};

// ========================================================================
// Data Structures
// ========================================================================

pub struct Recorder {
    monitor: Monitor,
    sink: Box<dyn JournalSink>,
    violations: Vec<Violation>,
}

// ========================================================================
// Recorder Implementations
// ========================================================================

impl Recorder {
    pub fn new(sink: Box<dyn JournalSink>, policy: &Policy) -> Self {
        Self {
            monitor: Monitor::new(policy),
            sink,
            violations: Vec::new(),
        }
    }

    /// A recorder for a session whose broker reports to another journal; see
    /// [`Monitor::audited_elsewhere`].
    pub fn audited_elsewhere(sink: Box<dyn JournalSink>) -> Self {
        Self {
            monitor: Monitor::audited_elsewhere(),
            sink,
            violations: Vec::new(),
        }
    }

    /// Append, then observe.
    ///
    /// In that order: a property observing an record that failed to reach the
    /// journal would be checking a history no one else can see. The monitor's
    /// whole premise is that it reads what was written.
    pub fn append(&mut self, record: &Record) -> io::Result<()> {
        let span = trace_span!(
            "journal_append",
            record_id = %record.id,
            session = %record.session,
        );
        let _guard = span.enter();
        self.sink.append(record)?;

        for violation in self.monitor.observe(record) {
            // Surfaced, not fatal. A violation means the harness did something
            // it should not have; killing the session would destroy the evidence
            // and is not obviously safer than finishing the turn and reporting.
            warn!(
                property = violation.property,
                detail = %violation.detail,
                "runtime property violated"
            );
            self.violations.push(violation);
        }
        Ok(())
    }

    /// Push everything buffered on to the OS.
    ///
    /// **Called at turn boundaries, which is where durability is worth its
    /// cost.** A journal is only useful if it survives the crash that made you
    /// want it, and the sink is free to buffer: `JournalSink::flush` exists so
    /// that wrapping the file in a `BufWriter` is a performance decision rather
    /// than a silent data-loss one. Nothing called it, which made the seam
    /// documentation.
    ///
    /// Not per record: that would be an `fsync`-shaped cost on the hot path for
    /// a guarantee nobody asked for. A turn is the unit a user would notice
    /// losing.
    pub fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }

    /// Every violation this run has produced.
    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }
}
