//! The seam where a real file or socket appears.
//!
//! The journal never opens a file. Callers that want persistence construct a
//! [`WriterSink`] over a handle they opened (the engine opens the journal file;
//! `journal` stays clear of the enforcement rule) and hand it to the
//! recorder. [`JournalSink`] is also the extension point for aggregate telemetry
//! in Phase 9.

use std::io::{self, Write};

use crate::codec::write_record;
use crate::record::Record;

// ========================================================================
// Traits
// ========================================================================

/// Append-only sink for journal records. `Send + Sync` like every seam the
/// engine holds: without it `Box<dyn JournalSink>` is `!Send`, which makes the
/// recorder and engine `!Send`, which the Phase 7 daemon discovers late.
pub trait JournalSink: Send + Sync {
    fn append(&mut self, record: &Record) -> io::Result<()>;

    /// Push anything buffered on to the OS.
    ///
    /// **The trait needs this even though the shipped sink does not**, and
    /// leaving it out is why durability had no owner. A `WriterSink` over a raw
    /// `File` needs no flush, so it was never asked for -- but the obvious next
    /// step, wrapping that file in a `BufWriter` to stop the per-record syscall,
    /// would then silently lose its tail on every crash, with nothing in the
    /// trait to call. Naming it here makes buffering a safe thing to add later
    /// rather than a trap.
    ///
    /// The default is a no-op, which is correct for an unbuffered handle.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ========================================================================
// Data Structures
// ========================================================================

/// A [`JournalSink`] over any `Write` handle: an in-memory buffer in tests, a
/// file the engine opened, a socket in Phase 9 telemetry.
pub struct WriterSink<W> {
    writer: W,
}

// ========================================================================
// WriterSink Implementations
// ========================================================================

impl<W> WriterSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W> JournalSink for WriterSink<W>
where
    W: Write + Send + Sync,
{
    fn append(&mut self, record: &Record) -> io::Result<()> {
        write_record(&mut self.writer, record).map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
