//! What is being asked. Constructed by the broker, never by a tool.

use std::path::PathBuf;

// ========================================================================
// Data Structures
// ========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Read(PathBuf),
    Write(PathBuf),
    Exec { program: String, args: Vec<String> },
    Connect { host: String, port: u16 },
    Listen { port: u16 },
}
