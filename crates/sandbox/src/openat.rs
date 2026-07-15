//! Containment the kernel enforces at the moment of the open.
//!
//! **`canonicalize` answers a question about the past.** [`crate::canon`]
//! resolves a path, the evaluator decides about the resolved path, and then the
//! broker opens it -- three steps, with a window between the second and the
//! third. The agent controls what is in that window: `cargo` is on the built-in
//! allow list and runs arbitrary `build.rs`, so a directory component approved
//! as an ordinary directory can be replaced by a symlink before the open
//! happens, and the open follows it. No amount of care in the *check* closes
//! that, because the check and the use are separate syscalls and the filesystem
//! is shared mutable state.
//!
//! `openat2` closes it by construction. Given a directory descriptor for the
//! policy root and `RESOLVE_BENEATH`, the kernel refuses any resolution that
//! leaves that directory -- through `..`, through an absolute symlink, through a
//! symlink swapped in a microsecond ago. The descriptor is the root: it was
//! opened once, and renaming the directory afterwards does not move it.
//!
//! **What this does and does not buy.** It guarantees the opened file is beneath
//! the root. It does *not* make the policy decision atomic: if one in-root path
//! is swapped for another in-root path mid-window, the file opened is not the
//! file evaluated. That residue is bounded by the root, which is the property
//! worth having, and closing it further needs the evaluation itself to run on a
//! descriptor rather than a name.
//!
//! Linux 5.6 and later. Older kernels report `ENOSYS` and the caller falls back
//! to the check-then-open path, which is where every platform started.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::broker::OpenMode;

// ========================================================================
// Constants
// ========================================================================

/// Refuse a resolution that escapes the directory descriptor. This is the whole
/// point of the module.
const RESOLVE_BENEATH: u64 = 0x08;

/// Refuse `/proc/self/fd`-style jumps, which are not symlinks and would
/// otherwise slip past `RESOLVE_BENEATH`'s reasoning about them.
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;

/// Mode for a file the broker creates. The user's umask narrows it, exactly as
/// it does for `OpenOptions::create`.
const CREATE_MODE: u64 = 0o666;

// ========================================================================
// Data Structures
// ========================================================================

/// The kernel's `struct open_how`, from `linux/openat2.h`.
///
/// Declared here rather than taken from `libc`, whose version is
/// `#[non_exhaustive]` and therefore cannot be constructed outside that crate.
/// The layout is kernel UAPI and stable; the size is passed to the syscall, and
/// the kernel rejects trailing bytes it does not recognize, so a mismatch fails
/// loudly rather than silently misreading flags.
#[repr(C)]
#[derive(Default)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

// ========================================================================
// Functions
// ========================================================================

/// Open `relative` beneath `root`, refusing anything that escapes it.
///
/// `relative` must be relative; an absolute path would be refused by the kernel
/// under `RESOLVE_BENEATH` anyway, but passing one is a caller bug rather than
/// an attack and is worth failing clearly.
///
/// `Err` with [`io::ErrorKind::Unsupported`] means this kernel has no `openat2`
/// and the caller should fall back. Every other error is a genuine refusal or a
/// genuine I/O failure.
pub fn open_beneath(root: &File, relative: &Path, access: OpenMode) -> io::Result<File> {
    if relative.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "open_beneath takes a path relative to the root",
        ));
    }
    let path = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;

    // `O_NONBLOCK` for the same reason `open_regular` uses it: opening a FIFO
    // blocks waiting for a peer, so without it the hang happens before there is
    // a handle to inspect. `O_CLOEXEC` so an authorized handle does not leak
    // into an authorized child.
    let flags = match access {
        OpenMode::Read => libc::O_RDONLY,
        // Not `O_TRUNC`: `AuthorizedWrite::write` truncates when it has content
        // to put there, so an authorized write that never happens leaves the
        // file as it found it.
        OpenMode::Write => libc::O_WRONLY | libc::O_CREAT,
    } | libc::O_CLOEXEC
        | libc::O_NONBLOCK;

    let how = OpenHow {
        flags: flags as u64,
        mode: match access {
            OpenMode::Read => 0,
            OpenMode::Write => CREATE_MODE,
        },
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
    };

    // Safety: `path` is a valid NUL-terminated C string that outlives the call,
    // `how` is a correctly sized `open_how` living on this stack frame, and the
    // descriptor comes from `root`, which the caller holds open. The syscall
    // either returns a new descriptor or a negative errno; it writes nothing
    // through the pointers.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };

    if fd >= 0 {
        // Safety: the kernel just returned this descriptor and nothing else owns
        // it, so `File` takes sole ownership and will close it.
        return Ok(unsafe { File::from_raw_fd(fd as i32) });
    }

    let error = io::Error::last_os_error();
    // `ENOSYS` is "this kernel predates openat2", not "this path is refused".
    // `EPERM` is what seccomp filters (Docker's default profile blocked openat2
    // for years) return for a syscall they do not know, and it is also a real
    // refusal -- so it is *not* treated as unsupported. Falling back on a real
    // refusal would be the one mistake that matters here.
    if error.raw_os_error() == Some(libc::ENOSYS) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this kernel has no openat2",
        ));
    }
    Err(error)
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// A repo with one file, plus the descriptor a broker would hold.
    fn fixture() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").expect("main.rs");
        let root = File::open(dir.path()).expect("root descriptor");
        (dir, root)
    }

    /// Whether this kernel has `openat2` at all, so the assertions below can say
    /// "refused" rather than "unsupported" without ambiguity.
    fn supported(root: &File) -> bool {
        !matches!(
            open_beneath(root, Path::new("src/main.rs"), OpenMode::Read),
            Err(ref error) if error.kind() == io::ErrorKind::Unsupported
        )
    }

    #[test]
    fn a_path_inside_the_root_opens() {
        let (_dir, root) = fixture();
        if !supported(&root) {
            return;
        }
        let file = open_beneath(&root, Path::new("src/main.rs"), OpenMode::Read).expect("opens");
        assert!(file.metadata().expect("metadata").is_file());
    }

    #[test]
    fn a_dotdot_escape_is_refused_by_the_kernel() {
        // The lexical escape. `canonicalize` also catches this one; the point
        // here is that the *kernel* does, with no cooperation from us.
        let (_dir, root) = fixture();
        if !supported(&root) {
            return;
        }
        let error = open_beneath(&root, Path::new("../../etc/hostname"), OpenMode::Read)
            .expect_err("`..` may not leave the root");
        assert_ne!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn a_symlink_pointing_outside_is_refused_at_open_time() {
        // **The check/use gap, closed.** `canonicalize` would resolve this and
        // the evaluator would refuse it -- but only if the link was already
        // there when the check ran. Here nothing checked anything: the open
        // itself refuses, which is what makes a link planted *after* the check
        // harmless.
        let (dir, root) = fixture();
        if !supported(&root) {
            return;
        }
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "shh").expect("secret");
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("escape"))
            .expect("symlink");

        let error = open_beneath(&root, Path::new("escape"), OpenMode::Read)
            .expect_err("an absolute symlink leaves the root");
        assert_ne!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn a_symlink_staying_inside_still_resolves() {
        // The other half, and the reason `RESOLVE_NO_SYMLINKS` is not used: real
        // repositories contain symlinks, and refusing all of them would trade a
        // security property for an unusable tool.
        let (dir, root) = fixture();
        if !supported(&root) {
            return;
        }
        std::os::unix::fs::symlink("src/main.rs", dir.path().join("link.rs")).expect("symlink");

        let file = open_beneath(&root, Path::new("link.rs"), OpenMode::Read)
            .expect("an in-tree symlink is not an escape");
        assert!(file.metadata().expect("metadata").is_file());
    }

    #[test]
    fn a_write_creates_the_file_without_truncating_it() {
        // `AuthorizedWrite::write` owns truncation, so an authorized write that
        // never happens must leave the file as it found it.
        let (dir, root) = fixture();
        if !supported(&root) {
            return;
        }
        std::fs::write(dir.path().join("existing"), "original").expect("existing");

        let mut created =
            open_beneath(&root, Path::new("fresh"), OpenMode::Write).expect("creates a new file");
        created.write_all(b"x").expect("writes");
        assert!(dir.path().join("fresh").is_file());

        let _ = open_beneath(&root, Path::new("existing"), OpenMode::Write).expect("opens");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("existing")).unwrap(),
            "original",
            "opening for write must not truncate"
        );
    }
}
