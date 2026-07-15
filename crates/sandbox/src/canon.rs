//! Path canonicalization: where symlink escapes die.
//!
//! `policy` normalizes `.` and `..` lexically, which is all a contract
//! crate can do without I/O. That half defeats `../../etc/passwd`. It does not
//! defeat `src/innocent -> /etc`, because knowing where a symlink points means
//! asking the filesystem, and this is the crate that is allowed to ask.
//!
//! **A write's target usually does not exist yet**, which is why this is not
//! just `fs::canonicalize`: that fails on a missing path, and failing every
//! create would make the agent useless. So the deepest *existing* ancestor is
//! resolved for real, and the not-yet-existing remainder is appended and
//! normalized lexically.
//!
//! **"The remainder cannot be a symlink, because it does not exist" is false,
//! and it was the assumption behind a full escape.** A *dangling* symlink exists
//! as a link and does not exist as a path: `fs::canonicalize` fails on it, so it
//! fell into the fallback and came back verbatim. A repo shipping
//! `link -> ~/.bashrc`, or any `git`/`cargo` run planting one, turned an
//! in-root-looking write into an arbitrary-path write -- the path read as
//! in-repo in the prompt and was covered by any remembered `write:/repo/**`
//! grant, while the write followed the link and created the victim file. So the
//! result is checked with `symlink_metadata`, which reports on the link itself
//! rather than its target, and a final symlink component is refused.

use std::io;
use std::path::{Component, Path, PathBuf};

use policy::normalize;

// ========================================================================
// Functions
// ========================================================================

/// Resolve `path` to an absolute, symlink-free path, whether or not it exists.
///
/// Non-UTF-8 paths are rejected here rather than encoded around. That is a real
/// limitation and a deliberate one: the journal is a text artifact, and one that
/// cannot be read as text is not the artifact the plan wants.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    if path.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {}", path.display()),
        ));
    }

    // The whole path exists: the OS resolves every link for us.
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Ok(strip_verbatim(resolved));
    }

    // It does not. Find the deepest ancestor that does, resolve that, and
    // re-attach the rest.
    let mut remainder: Vec<Component<'_>> = Vec::new();
    let mut current = path;
    loop {
        let Some(parent) = current.parent() else {
            // Reached the root without finding anything real. Fall back to the
            // lexical form: it is still absolute-or-not exactly as given, and
            // the caller's root check decides.
            return Ok(normalize(path));
        };
        let Some(name) = current.file_name() else {
            return Ok(normalize(path));
        };
        remainder.push(Component::Normal(name));

        if let Ok(resolved) = std::fs::canonicalize(parent) {
            let mut out = strip_verbatim(resolved);
            for component in remainder.iter().rev() {
                out.push(component.as_os_str());
            }
            // The remainder was taken verbatim, so a `..` inside it is still
            // lexical and still has to go.
            let out = normalize(&out);
            refuse_dangling_symlink(&out)?;
            return Ok(out);
        }
        current = parent;
    }
}

/// Drop Windows' `\\?\` verbatim prefix from an ordinary drive path.
///
/// **`fs::canonicalize` on Windows returns a verbatim path**, so a canonical
/// root reads as `\\?\C:\repo` and every canonical path under it as
/// `\\?\C:\repo\src\main.rs`. Left alone that is *consistent*, and consistency
/// is most of what containment needs -- but it leaks into everything a person
/// reads: the permission prompt, the journal, and the policy the user wrote by
/// hand with `C:\repo` in it. A path rule a user writes would not match a path
/// the harness canonicalized, and the mismatch is invisible in the two strings
/// until you count the backslashes.
///
/// Only the plain drive form is simplified. A verbatim UNC path (`\\?\UNC\...`)
/// and a path that is only legal in verbatim form (a component ending in a dot
/// or a space) keep the prefix, because there it is load-bearing rather than
/// noise.
#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };
    let Prefix::VerbatimDisk(letter) = prefix.kind() else {
        return path;
    };
    let rest = components.as_path();
    // A component that only a verbatim path may express must keep its prefix.
    let expressible = rest.components().all(|component| match component {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|name| !name.ends_with('.') && !name.ends_with(' ')),
        _ => true,
    });
    if !expressible {
        return path;
    }
    let mut simplified = PathBuf::from(format!("{}:\\", letter as char));
    simplified.push(rest);
    simplified
}

/// A no-op off Windows: `fs::canonicalize` has no prefix to drop there.
#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// Refuse a path whose final component is a symlink we could not resolve.
///
/// Reached only from the fallback, and only for a link whose target does not
/// exist: anything resolvable was already handled by `fs::canonicalize` above.
/// Such a path passes the root check as written and then redirects the write
/// wherever the link points, so it cannot be handed back as though it named
/// itself.
///
/// Refusing is the whole answer here rather than resolving the link, because
/// resolving it would re-open the check/use gap this module exists to close: the
/// target is a name the agent controls and can change between this call and the
/// open. The broker's caller sees a `Denied`, which is the honest report.
fn refuse_dangling_symlink(path: &Path) -> io::Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        // Nothing there at all, which is the ordinary "creating a new file"
        // case. `symlink_metadata` does not follow, so a link would have
        // answered here.
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a symlink whose target does not exist; refusing to \
                 resolve it",
                path.display()
            ),
        ));
    }
    Ok(())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with `real/` and `link -> /tmp` style escapes, under a temp root.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/foo.rs"), "fn main() {}").expect("foo.rs");
        dir
    }

    #[test]
    fn an_existing_path_resolves() {
        let dir = fixture();
        let resolved = canonicalize(&dir.path().join("src/foo.rs")).expect("resolves");
        assert!(resolved.ends_with("src/foo.rs"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn a_dotdot_in_an_existing_path_resolves() {
        let dir = fixture();
        let resolved = canonicalize(&dir.path().join("src/../src/foo.rs")).expect("resolves");
        assert_eq!(
            resolved,
            canonicalize(&dir.path().join("src/foo.rs")).unwrap()
        );
    }

    #[test]
    fn a_path_that_does_not_exist_yet_still_resolves() {
        // A write creates its target, so failing here would fail every create.
        let dir = fixture();
        let resolved = canonicalize(&dir.path().join("src/new.rs")).expect("resolves");
        assert!(resolved.ends_with("src/new.rs"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn a_dotdot_in_the_nonexistent_remainder_is_still_resolved() {
        // The remainder is attached verbatim, so its `..` has to be normalized
        // or it escapes after the ancestor was resolved.
        let dir = fixture();
        let resolved = canonicalize(&dir.path().join("src/nope/../new.rs")).expect("resolves");
        assert!(
            resolved.ends_with("src/new.rs"),
            "got {}",
            resolved.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_is_resolved_to_its_target() {
        // The escape lexical normalization cannot see: the path stays under the
        // root as written and points outside it once resolved.
        let dir = fixture();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "shh").expect("secret");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).expect("symlink");

        let resolved = canonicalize(&dir.path().join("escape/secret")).expect("resolves");
        let real_outside = std::fs::canonicalize(outside.path()).expect("canonical");
        assert!(
            resolved.starts_with(&real_outside),
            "symlink was not resolved: {}",
            resolved.display()
        );
        assert!(
            !resolved.starts_with(std::fs::canonicalize(dir.path()).unwrap()),
            "resolved path still looks like it is inside the root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_path_that_does_not_exist_yet_is_still_resolved() {
        // The dangerous version: the agent asks to *create* a file through a
        // symlinked directory. The file does not exist, so `fs::canonicalize`
        // fails and a naive fallback would return the lexical path, which is
        // still under the root and is a lie.
        let dir = fixture();
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).expect("symlink");

        let resolved = canonicalize(&dir.path().join("escape/new.txt")).expect("resolves");
        let real_outside = std::fs::canonicalize(outside.path()).expect("canonical");
        assert!(
            resolved.starts_with(&real_outside),
            "a create through a symlink was not resolved: {}",
            resolved.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_refused_rather_than_taken_verbatim() {
        // The escape the "a symlink cannot be in the remainder" argument missed.
        // The link exists, its target does not, so `fs::canonicalize` fails and
        // the fallback used to return `/repo/link` -- inside the root, past every
        // check, and the write then followed the link and created the victim.
        let dir = fixture();
        let outside = tempfile::tempdir().expect("outside");
        let victim = outside.path().join("bashrc");
        std::os::unix::fs::symlink(&victim, dir.path().join("link")).expect("symlink");

        let error = canonicalize(&dir.path().join("link"))
            .expect_err("a dangling symlink must not resolve to itself");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(!victim.exists(), "nothing should have been created");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_in_the_middle_of_a_path_does_not_escape() {
        // The directory form: `link -> /outside` where `/outside` is gone, then
        // a write through it. The ancestor walk skips the unresolvable link and
        // lands on the real parent, so the result must stay under the root
        // rather than adopt the link's target.
        let dir = fixture();
        let outside = tempfile::tempdir().expect("outside");
        let gone = outside.path().join("gone");
        std::os::unix::fs::symlink(&gone, dir.path().join("escape")).expect("symlink");

        let resolved = canonicalize(&dir.path().join("escape/new.txt")).expect("resolves");
        assert!(
            !resolved.starts_with(&gone),
            "resolved to the dangling target: {}",
            resolved.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_ordinary_new_file_still_resolves() {
        // The check must not cost the common case: a path with nothing at the
        // end is a create, not an attack.
        let dir = fixture();
        let resolved = canonicalize(&dir.path().join("src/brand_new.rs")).expect("resolves");
        assert!(resolved.ends_with("src/brand_new.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_rejected() {
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe"));
        assert!(canonicalize(&path).is_err());
    }
}
