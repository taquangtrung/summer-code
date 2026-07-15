//! Project environment and CLI tool discovery.
//!
//! Inspects the working directory at session startup to detect project manifests
//! (e.g. `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, `CMakeLists.txt`)
//! and probes `PATH` for installed accelerators (e.g. `cargo-nextest`, `ruff`,
//! `pnpm`, `golangci-lint`, `biome`).
//!
//! Emits an `<environment>` block in the system prompt guiding the LLM toward
//! the optimal CLI tools and flags for the current repository.

use std::collections::BTreeSet;
use std::path::Path;

// ========================================================================
// Constants
// ========================================================================

/// The languages [`project_languages`] can report, by the name skills are
/// prefixed with (`rust-testing`, `python-code-style`), and the file
/// extensions that count as each.
pub const LANGUAGES: &[(&str, &[&str])] = &[
    ("bash", &["sh", "bash"]),
    ("c", &["c", "h"]),
    ("cpp", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"]),
    ("go", &["go"]),
    ("latex", &["cls", "sty", "tex"]),
    ("markdown", &["markdown", "md"]),
    ("ocaml", &["ml", "mli"]),
    ("python", &["py", "pyi"]),
    ("rust", &["rs"]),
    ("typescript", &["cjs", "js", "jsx", "mjs", "ts", "tsx"]),
];

/// How deep [`project_languages`] looks below the root.
const LANGUAGE_SCAN_DEPTH: usize = 4;

/// The most directory entries [`project_languages`] reads before it answers
/// with what it has: startup must not wait on a walk of a huge tree.
const LANGUAGE_SCAN_ENTRIES: usize = 5_000;

/// Directories that hold build output or dependencies rather than the
/// project's own code.
const SKIPPED_DIRS: &[&str] = &["build", "dist", "node_modules", "target", "vendor"];

// ========================================================================
// Functions
// ========================================================================

/// The languages the project in `cwd` has files in, by [`LANGUAGES`] name.
///
/// A bounded walk of the top of the tree, skipping hidden directories and
/// build output: enough to know whether a project has any Python at all, which
/// is the question the skills catalog asks, without reading the whole tree.
pub fn project_languages(cwd: &Path) -> BTreeSet<&'static str> {
    let mut found = BTreeSet::new();
    let mut pending = vec![(cwd.to_path_buf(), 0)];
    let mut read = 0;
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            read += 1;
            if read > LANGUAGE_SCAN_ENTRIES {
                return found;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if depth < LANGUAGE_SCAN_DEPTH && !SKIPPED_DIRS.contains(&name.as_ref()) {
                    pending.push((path, depth + 1));
                }
            } else if let Some(extension) = path.extension().and_then(|ext| ext.to_str())
                && let Some((language, _)) = LANGUAGES
                    .iter()
                    .find(|(_, extensions)| extensions.contains(&extension))
            {
                found.insert(*language);
            }
        }
    }
    found
}

/// Detect project ecosystems and installed CLI accelerators in `cwd`.
pub fn detect_environment(cwd: &Path) -> Option<String> {
    let mut recommendations = Vec::new();

    // 1. Rust Ecosystem
    if cwd.join("Cargo.toml").exists() {
        let is_workspace = is_cargo_workspace(cwd);
        let scope = if is_workspace { " (workspace)" } else { "" };
        let mut rust_lines = vec![format!("- Ecosystem: Rust{scope}")];

        if is_workspace {
            rust_lines.push("- Fast Type Check: `cargo check --workspace`".into());
        } else {
            rust_lines.push("- Fast Type Check: `cargo check`".into());
        }

        if which("cargo-nextest") {
            rust_lines.push(
                "- Test Runner: `cargo nextest run` (accelerated) or `cargo test -p <crate>`"
                    .into(),
            );
        } else if is_workspace {
            rust_lines
                .push("- Test Runner: `cargo test -p <crate>` (scope to modified crate)".into());
        } else {
            rust_lines.push("- Test Runner: `cargo test`".into());
        }

        rust_lines.push("- Linter: `cargo clippy`".into());
        recommendations.extend(rust_lines);
    }

    // 2. JavaScript / TypeScript / Node Ecosystem
    if cwd.join("package.json").exists() {
        let (pkg_manager, _runner) = if cwd.join("pnpm-lock.yaml").exists() {
            ("pnpm", "pnpm")
        } else if cwd.join("yarn.lock").exists() {
            ("yarn", "yarn")
        } else if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
            ("bun", "bun")
        } else {
            ("npm", "npm run")
        };

        let mut js_lines = vec![format!(
            "- Ecosystem: JavaScript / TypeScript ({pkg_manager})"
        )];

        if cwd.join("tsconfig.json").exists() {
            js_lines.push("- Type Check: `npx tsc --noEmit`".into());
        }

        js_lines.push(format!("- Test Runner: `{pkg_manager} test`"));

        if cwd.join("biome.json").exists() || which("biome") {
            js_lines.push("- Linter & Formatter: `npx biome check`".into());
        } else if cwd.join(".eslintrc").exists()
            || cwd.join(".eslintrc.js").exists()
            || cwd.join(".eslintrc.json").exists()
            || cwd.join("eslint.config.js").exists()
        {
            js_lines.push("- Linter: `npx eslint .`".into());
        }

        recommendations.extend(js_lines);
    }

    // 3. Go Ecosystem
    if cwd.join("go.mod").exists() {
        let mut go_lines = vec![
            "- Ecosystem: Go".into(),
            "- Fast Check & Build: `go build ./...`".into(),
            "- Test Runner: `go test ./...` (or `go test -run <TestName> ./...`)".into(),
        ];

        if which("golangci-lint") {
            go_lines.push("- Linter: `golangci-lint run --fast`".into());
        }

        recommendations.extend(go_lines);
    }

    // 4. Python Ecosystem
    if cwd.join("pyproject.toml").exists()
        || cwd.join("requirements.txt").exists()
        || cwd.join("Pipfile").exists()
        || cwd.join("setup.py").exists()
    {
        let has_uv = which("uv");
        let prefix = if has_uv { "uv run " } else { "" };
        let mut py_lines = vec![
            format!(
                "- Ecosystem: Python{}",
                if has_uv { " (uv detected)" } else { "" }
            ),
            format!("- Test Runner: `{prefix}pytest -k <filter>`"),
        ];

        if which("ruff") {
            py_lines.push(format!("- Fast Linter: `{prefix}ruff check`"));
        }
        if which("mypy") {
            py_lines.push(format!("- Type Check: `{prefix}mypy .`"));
        }

        recommendations.extend(py_lines);
    }

    // 5. C / C++ Ecosystem
    if cwd.join("CMakeLists.txt").exists() {
        let mut c_lines = vec![
            "- Ecosystem: C/C++ (CMake)".into(),
            "- Build: `cmake --build build` (or `cmake -B build && cmake --build build`)".into(),
            "- Test: `ctest --test-dir build --output-on-failure`".into(),
        ];
        if which("ninja") {
            c_lines.push("- Build Accelerator: `ninja -C build`".into());
        }
        recommendations.extend(c_lines);
    } else if cwd.join("Makefile").exists() {
        recommendations.extend(vec![
            "- Ecosystem: C/C++ (Make)".into(),
            "- Build: `make`".into(),
            "- Test: `make test` or `make check`".into(),
        ]);
    }

    if recommendations.is_empty() {
        None
    } else {
        let mut out =
            String::from("\n\n<environment>\n# Detected Project Environment & Recommended Tools\n");
        for line in recommendations {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("</environment>");
        Some(out)
    }
}

/// Whether `cwd/Cargo.toml` declares a workspace.
fn is_cargo_workspace(cwd: &Path) -> bool {
    let cargo_path = cwd.join("Cargo.toml");
    let Ok(content) = std::fs::read_to_string(cargo_path) else {
        return false;
    };
    content.contains("[workspace]")
}

/// Check if `program` is on `PATH`.
fn which(program: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
            || candidate.with_extension("exe").is_file()
            || candidate.with_extension("cmd").is_file()
    })
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn project_languages_reads_the_tree_but_not_its_build_output() {
        // A `node_modules` full of JavaScript does not make a Rust project a
        // JavaScript one, and the skills catalog would list a language's
        // skills on the strength of a dependency.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(dir.path().join("src/deep/lib.rs"), "").unwrap();
        fs::write(dir.path().join("README.md"), "").unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        fs::write(dir.path().join("node_modules/pkg/index.js"), "").unwrap();
        fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
        fs::write(dir.path().join(".git/hooks/pre-commit.sh"), "").unwrap();
        assert_eq!(
            project_languages(dir.path()),
            BTreeSet::from(["markdown", "rust"])
        );
    }

    #[test]
    fn detects_rust_workspace_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();

        let env = detect_environment(dir.path()).expect("detected rust workspace");
        assert!(env.contains("<environment>"));
        assert!(env.contains("Rust (workspace)"));
        assert!(env.contains("cargo check --workspace"));
        assert!(env.contains("cargo clippy"));
    }

    #[test]
    fn detects_pnpm_node_project_with_typescript() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{\"name\": \"app\"}").unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 5.4").unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();

        let env = detect_environment(dir.path()).expect("detected pnpm project");
        assert!(env.contains("JavaScript / TypeScript (pnpm)"));
        assert!(env.contains("npx tsc --noEmit"));
        assert!(env.contains("pnpm test"));
    }

    #[test]
    fn detects_go_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/app\n").unwrap();

        let env = detect_environment(dir.path()).expect("detected go project");
        assert!(env.contains("Ecosystem: Go"));
        assert!(env.contains("go test ./..."));
    }

    #[test]
    fn detects_python_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"app\"\n",
        )
        .unwrap();

        let env = detect_environment(dir.path()).expect("detected python project");
        assert!(env.contains("Ecosystem: Python"));
        assert!(env.contains("pytest"));
    }

    #[test]
    fn empty_directory_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_environment(dir.path()).is_none());
    }
}
