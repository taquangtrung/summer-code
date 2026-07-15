//! The handle types: proof that a policy check was passed.
//!
//! Fields are private with no public constructor. The only way to hold one is
//! to have called a [`crate::Broker`] method, which under Phase 2 will have run
//! the evaluation first. This is the enforcement; the CI lint only stops someone
//! from bypassing the crate entirely.
//!
//! **Every handle holds the thing itself, never a name for it.** `read` and
//! `write` hold an already-open [`File`], `exec` an already-spawned child,
//! `connect` an already-connected stream, `listen` an already-bound listener.
//! That is one rule with one reason: an authorization the tool redeems itself is
//! not an authorization, because the tool can redeem it for something else.
//!
//! For files the something else is a different inode. A handle holding a path
//! would be re-resolved at whatever moment the tool chose to open it, so a
//! symlink swapped into place after the policy check redirects the read or write
//! to a file the broker never approved: `cargo` is on the built-in allow list and
//! runs arbitrary `build.rs`, so the agent can arrange the swap. Opening in the
//! broker pins the handle to the inode that was checked.
//!
//! [`AuthorizedAddrs`] is the single exception, and it is narrowed to the part
//! of the rule it cannot keep: it holds resolved addresses rather than a
//! connected stream, because the HTTP client that will use them is not this
//! crate. The name is still not the tool's to redeem, which is the whole point
//! of the type; see its own docs.

use std::collections::VecDeque;
use std::fs::{DirEntry, File, ReadDir};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_util::sync::CancellationToken;

// ========================================================================
// Constants
// ========================================================================

/// How much of a child's output is kept, per stream.
///
/// A tool result is not just held in this process: it goes into the model's
/// context window and stays there for the rest of the session. So an uncapped
/// `cat` of a build log does not merely balloon the harness, it evicts the
/// conversation that gave the command meaning, and the session ends in
/// `ContextOverflow` or the OOM killer. 64 KiB is a few hundred lines of
/// compiler output: past anything an ordinary command produces, and a small
/// enough fraction of the window that a runaway one is survivable.
const CAPTURE_BYTES: usize = 64 * 1024;

/// How much of [`CAPTURE_BYTES`] is kept from the head, the rest from the tail.
///
/// Both ends carry signal and the middle rarely does: a test run puts the
/// invocation at the top and the failure count at the bottom, so a cap that kept
/// only the head would reliably drop the answer.
const CAPTURE_HEAD_BYTES: usize = CAPTURE_BYTES / 2;

/// One read from a child's pipe.
const READ_CHUNK_BYTES: usize = 8 * 1024;

// ========================================================================
// Data Structures
// ========================================================================

/// Authorization to read one file, held open since the check.
pub struct AuthorizedRead {
    file: File,
}

/// Authorization to enumerate one directory, held open since the check.
///
/// The broker opens the directory as part of the check, the same way it opens
/// a file for [`AuthorizedRead`]: the tool receives the already-open
/// enumeration rather than a name it re-resolves at a moment of its choosing.
pub struct AuthorizedDir {
    entries: ReadDir,
}

/// Authorization to write one file, held open since the check.
pub struct AuthorizedWrite {
    file: File,
}

/// An already-spawned authorized command.
///
/// **The handle runs the child; it does not lend it out.** Handing back a bare
/// `Child` is what let stdio configuration, working directory, environment,
/// cancellation, output capping and exit-status handling each diverge across the
/// call sites, so that one of them captured no output at all and another
/// reported success for a `git` that had failed. There is one way to redeem this
/// handle, [`AuthorizedCommand::run`], and it decides all six.
pub struct AuthorizedCommand {
    child: Child,
}

/// A long-lived authorized child, spoken to over its stdin and stdout.
///
/// **The shape [`AuthorizedCommand`] cannot take.** That type runs a child to
/// completion and captures both streams into capped buffers, which is right for
/// `cargo test` and impossible for a server: a language server is initialized
/// once, exchanges framed messages for the length of a session, and is asked to
/// shut down at the end. Consuming `self` to wait for exit is the whole
/// difference, so this is a separate handle rather than a flag on that one.
///
/// **stdin is piped, where `exec` nulls it.** `exec` nulls stdin deliberately,
/// so an interactive child fails at once instead of blocking until the
/// dispatcher's timeout. A service is precisely the case where that is wrong,
/// and needing the opposite answer is why the two are different methods.
///
/// The framing is not here. Bytes in, bytes out, plus enough to read a
/// header line and then an exact body: `Content-Length` is LSP's business, and a
/// handle that knew about it would be a handle that serves one protocol.
///
/// Dropping this kills the child, because the broker sets `kill_on_drop`. A
/// server outliving the session that authorized it is an orphan holding a
/// workspace lock.
pub struct AuthorizedService {
    /// Held so that dropping the handle drops the child, which kills it. Never
    /// read: the streams below are taken out of it at construction.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// What running an authorized command produced.
pub enum CommandOutcome {
    /// The child exited on its own.
    Exited(CommandOutput),
    /// Cancellation won the race. The child was killed rather than orphaned.
    Interrupted,
}

/// A finished command: what it wrote, and how it ended.
pub struct CommandOutput {
    /// The exit code, or `None` when a signal ended the child.
    pub code: Option<i32>,
    pub stderr: CapturedStream,
    pub stdout: CapturedStream,
}

/// One captured stream, and what the cap cost.
pub struct CapturedStream {
    /// What the child wrote, with the middle replaced by a marker when the cap
    /// bit. Never longer than `CAPTURE_BYTES` plus that marker.
    pub bytes: Vec<u8>,
    /// Bytes elided from the middle. Zero means `bytes` is everything.
    pub elided: usize,
}

/// Accumulates a stream, keeping the head and the newest tail once it passes the
/// cap.
///
/// Reading continues past the cap and the excess is dropped, rather than the
/// read stopping. A stopped read leaves the pipe to fill, and a child blocked
/// writing into a full pipe never exits, which would turn a capped read into a
/// hang -- the failure the cap exists to prevent, arrived at from the other side.
struct CaptureBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

/// The addresses one authorized host resolved to, looked up by the broker.
///
/// **The one handle that holds less than the thing itself, and the reason is
/// worth stating.** An HTTPS request is a TLS handshake, HTTP framing and
/// content decoding over a socket, which is a client library's job; `web_fetch`
/// therefore has `reqwest` do the transport and cannot be handed an
/// [`AuthorizedSocket`]. What it can be denied is the *lookup*. The broker
/// resolves the host while it evaluates the policy and hands back the answer,
/// the tool's client is built over a resolver that serves only these addresses,
/// and a name it was never given resolves to an error rather than to whatever
/// DNS says a moment later.
///
/// So the residue is one TCP connect to an address this handle names, instead of
/// a second name lookup whose answer nobody checked.
pub struct AuthorizedAddrs {
    addrs: Vec<SocketAddr>,
    host: String,
    port: u16,
}

/// An already-connected authorized socket.
pub struct AuthorizedSocket {
    stream: TcpStream,
}

/// An already-bound listener.
///
/// **Bound by the broker, to an address the policy chose.** A tool cannot widen
/// it to `0.0.0.0` because a tool never calls `bind`: the same shape as
/// `AuthorizedRead`, where the broker resolves the path, and for the same reason.
/// An authorization token the tool redeems itself is not an authorization,
/// because it can redeem it for something else.
pub struct AuthorizedListener {
    bound: SocketAddr,
    listener: TcpListener,
}

// ========================================================================
// AuthorizedRead Implementations
// ========================================================================

impl AuthorizedRead {
    pub(crate) fn new(file: File) -> Self {
        Self { file }
    }

    /// Read the authorized file as a string.
    pub fn read_to_string(&self) -> io::Result<String> {
        let mut content = String::new();
        (&self.file).read_to_string(&mut content)?;
        Ok(content)
    }
}

// ========================================================================
// AuthorizedDir Implementations
// ========================================================================

impl AuthorizedDir {
    pub(crate) fn new(entries: ReadDir) -> Self {
        Self { entries }
    }
}

impl Iterator for AuthorizedDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next()
    }
}

// ========================================================================
// AuthorizedWrite Implementations
// ========================================================================

impl AuthorizedWrite {
    pub(crate) fn new(file: File) -> Self {
        Self { file }
    }

    /// Overwrite the authorized file with `content`.
    ///
    /// Truncate-then-write on the open handle, which is what `fs::write` does to
    /// a path. The file is not re-opened: the broker opened it, and this writes
    /// to that inode or to none.
    ///
    /// **Not atomic, and deliberately not.** The tempting hardening is to write a
    /// sibling temp file and `rename` it over the target, which would make a
    /// Ctrl-C between truncate and write a no-op instead of an emptied file. It
    /// cannot go here: the whole point of this type is that the broker opened
    /// *this* inode and authorized *it*, and a rename authorizes a path the
    /// broker never checked, re-resolving the name at exactly the moment the
    /// symlink-swap tests exist to close. Atomic replace belongs in the broker,
    /// which is the only thing that may resolve a name, and it is not there yet.
    /// The exposure is a torn file on a crash mid-write, which git recovers and
    /// the journal records the intent of.
    pub fn write(&self, content: &[u8]) -> io::Result<()> {
        let mut file = &self.file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(content)?;
        // The result the model is told is "wrote N bytes"; without this the claim
        // is about bytes handed to the OS, not bytes the next reader will see.
        file.flush()
    }
}

// ========================================================================
// AuthorizedCommand Implementations
// ========================================================================

impl AuthorizedCommand {
    pub(crate) fn new(child: Child) -> Self {
        Self { child }
    }

    /// Run the child to completion, capturing both streams under the cap, and
    /// racing `cancel` so a long command stays interruptible.
    ///
    /// **Both pipes are drained concurrently with the wait, and that ordering is
    /// load-bearing.** Waiting for exit first and reading after deadlocks on any
    /// child that writes more than a pipe buffer: the child blocks on the write,
    /// the parent blocks on the exit, and neither moves. `try_join!` is what
    /// keeps the pipes emptying while the child is still alive.
    ///
    /// On cancellation the collect future is dropped, `child` falls out of scope,
    /// and the broker's `kill_on_drop` turns that drop into a kill. Nothing is
    /// left running behind an interrupted turn.
    pub async fn run(self, cancel: &CancellationToken) -> io::Result<CommandOutcome> {
        let mut child = self.child;
        // The broker pipes both, so these are `Some`; `capture` treats an
        // inherited stream as empty rather than asserting.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let collect = async {
            let (stdout, stderr, status) =
                tokio::try_join!(capture(stdout), capture(stderr), child.wait())?;
            Ok::<_, io::Error>(CommandOutput {
                code: status.code(),
                stderr,
                stdout,
            })
        };

        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(CommandOutcome::Interrupted),
            collected = collect => collected.map(CommandOutcome::Exited),
        }
    }
}

// ========================================================================
// AuthorizedService Implementations
// ========================================================================

impl AuthorizedService {
    /// Wrap a spawned child whose stdin and stdout are both piped.
    ///
    /// Returns `None` when either stream is missing, which cannot happen for a
    /// child the broker spawned and is not worth an error variant no caller
    /// could act on differently.
    pub(crate) fn new(mut child: Child) -> Option<Self> {
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self {
            _child: child,
            stdin,
            stdout,
        })
    }

    /// Write `bytes` to the child and flush.
    ///
    /// Flushing is not optional: a request left in the buffer is a request the
    /// server never sees, and the caller then blocks reading a reply to a
    /// message that was never sent.
    pub async fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes).await?;
        self.stdin.flush().await
    }

    /// Read one line, including its terminator. `Ok(0)` means the child closed
    /// its output, which for a server means it exited.
    pub async fn read_line(&mut self, out: &mut String) -> io::Result<usize> {
        self.stdout.read_line(out).await
    }

    /// Fill `out` exactly, for a body whose length a header already gave.
    pub async fn read_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        self.stdout.read_exact(out).await.map(|_| ())
    }
}

// ========================================================================
// CommandOutput Implementations
// ========================================================================

impl CommandOutput {
    /// Whether the child exited zero. A signal death is not success.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

// ========================================================================
// CaptureBuffer Implementations
// ========================================================================

impl CaptureBuffer {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let room = CAPTURE_HEAD_BYTES.saturating_sub(self.head.len());
        let (head, rest) = chunk.split_at(room.min(chunk.len()));
        self.head.extend_from_slice(head);
        self.tail.extend(rest);

        let excess = self
            .tail
            .len()
            .saturating_sub(CAPTURE_BYTES - CAPTURE_HEAD_BYTES);
        if excess > 0 {
            self.tail.drain(..excess);
        }
    }

    /// Splice head and tail, naming the gap where one was cut.
    ///
    /// The marker goes in here because this is the only place that knows where
    /// the boundary fell. It is text spliced into a byte stream, so it can land
    /// mid-codepoint; every reader of these bytes goes through
    /// `String::from_utf8_lossy`, which the head/tail cut already required.
    fn finish(self) -> CapturedStream {
        let elided = self.total - self.head.len() - self.tail.len();
        let mut bytes = self.head;
        if elided > 0 {
            bytes.extend_from_slice(format!("\n[... {elided} bytes elided ...]\n").as_bytes());
        }
        bytes.extend(self.tail);
        CapturedStream { bytes, elided }
    }
}

// ========================================================================
// AuthorizedAddrs Implementations
// ========================================================================

impl AuthorizedAddrs {
    pub(crate) fn new(addrs: Vec<SocketAddr>, host: String, port: u16) -> Self {
        Self { addrs, host, port }
    }

    /// Every address the host resolved to, in the order the resolver returned
    /// them. All of them, because a host with an A and an AAAA record is
    /// reachable at either and a client that gets only the first cannot fall
    /// back when it is unreachable.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// The host as it was authorized, which is the name a client will look up.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

// ========================================================================
// AuthorizedSocket Implementations
// ========================================================================

impl AuthorizedSocket {
    pub(crate) fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    pub fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    pub fn into_stream(self) -> TcpStream {
        self.stream
    }
}

// ========================================================================
// AuthorizedListener Implementations
// ========================================================================

impl AuthorizedListener {
    pub(crate) fn new(listener: TcpListener, bound: SocketAddr) -> Self {
        Self { bound, listener }
    }

    /// The address the broker actually bound.
    ///
    /// Recorded on the `Access::Listen` record, not just the port, so the audit
    /// trail can tell the safe case from the dangerous one: binding loopback is
    /// nearly always fine, binding `0.0.0.0` exposes the agent's workspace to the
    /// network.
    pub fn bound(&self) -> SocketAddr {
        self.bound
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }

    pub fn into_listener(self) -> TcpListener {
        self.listener
    }
}

// ========================================================================
// Functions
// ========================================================================

/// Drain one of the child's pipes into a [`CaptureBuffer`].
///
/// `None` means the stream was not piped, which the broker never does; it is
/// treated as empty rather than asserted so a future backend that inherits a
/// stream degrades to "no output" instead of a panic.
async fn capture(source: Option<impl AsyncRead + Unpin>) -> io::Result<CapturedStream> {
    let Some(mut source) = source else {
        return Ok(CapturedStream {
            bytes: Vec::new(),
            elided: 0,
        });
    };
    let mut buffer = CaptureBuffer::new();
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = source.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.push(&chunk[..read]);
    }
    Ok(buffer.finish())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_all(chunks: &[&[u8]]) -> CapturedStream {
        let mut buffer = CaptureBuffer::new();
        for chunk in chunks {
            buffer.push(chunk);
        }
        buffer.finish()
    }

    #[test]
    fn output_under_the_cap_is_kept_whole() {
        // The case every ordinary command is in: the cap must be invisible.
        let captured = capture_all(&[b"hello ", b"world"]);
        assert_eq!(captured.bytes, b"hello world");
        assert_eq!(captured.elided, 0);
    }

    #[test]
    fn output_spanning_the_cap_keeps_both_ends_and_names_the_gap() {
        // The failure this exists for: a test run's invocation is at the top and
        // its failure count at the bottom, so dropping either end drops the
        // answer. The marker is what stops the model reading the splice as
        // contiguous output.
        let head = vec![b'a'; CAPTURE_HEAD_BYTES];
        let middle = vec![b'm'; CAPTURE_BYTES];
        let tail = vec![b'z'; CAPTURE_BYTES - CAPTURE_HEAD_BYTES];
        let captured = capture_all(&[&head, &middle, &tail]);

        assert_eq!(captured.elided, CAPTURE_BYTES, "the middle is what goes");
        assert!(captured.bytes.starts_with(&head), "head kept");
        assert!(captured.bytes.ends_with(&tail), "tail kept");
        let rendered = String::from_utf8_lossy(&captured.bytes);
        assert!(rendered.contains("bytes elided"), "the cut is named");
    }

    #[test]
    fn the_cap_does_not_depend_on_how_the_stream_was_chunked() {
        // A pipe hands back whatever size it feels like, so the same bytes must
        // capture identically whether they arrive in one read or many.
        let bulk = vec![b'x'; CAPTURE_BYTES * 2];
        let one_shot = capture_all(&[&bulk]);
        let dribbled: Vec<&[u8]> = bulk.chunks(7).collect();
        let piecemeal = capture_all(&dribbled);

        assert_eq!(one_shot.elided, piecemeal.elided);
        assert_eq!(one_shot.bytes, piecemeal.bytes);
    }
}
