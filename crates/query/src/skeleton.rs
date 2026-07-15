//! Distilling a source file to its interface: signatures without bodies.
//!
//! What a model needs to reason about a file it is not editing is the shape:
//! what exists, what it is called, and what it takes. The bodies are the bulk of
//! the tokens and rarely the answer, so a skeleton is a large context saving on
//! any file the agent is only consulting.
//!
//! **Built from the parse, like everything else that reads code.** The version
//! this replaces counted `{` and `}` per line to find where a body ended, which
//! a brace inside a string or a char literal desynchronized for the rest of the
//! file: from that point on it emitted bodies as though they were signatures and
//! dropped signatures as though they were bodies. Tree-sitter gives the body's
//! exact extent, so removing it is a subtraction rather than a guess.

use std::path::Path;

use crate::extract::treesitter::body_spans;

// ========================================================================
// Constants
// ========================================================================

/// What replaces an elided body.
const ELIDED: &str = " { ... }";

// ========================================================================
// Functions
// ========================================================================

/// Extract an interface skeleton from `code`, parsed as `path`'s language.
///
/// A language with no grammar returns the source unchanged: an approximate
/// skeleton of a file nothing can parse is worse than the file, because the
/// reader cannot tell which lines were dropped.
pub fn extract_skeleton(path: &Path, code: &str) -> String {
    let spans = body_spans(path, code);
    if spans.is_empty() {
        return code.to_string();
    }

    // Walk the source once, copying everything outside a body and eliding each
    // body as it is reached. Spans arrive sorted and non-overlapping (an inner
    // function's body lies inside an outer one's, and `body_spans` keeps only
    // the outermost), so a single pass suffices.
    let mut out = String::with_capacity(code.len() / 2);
    let mut at = 0;
    for (start, end) in spans {
        if start < at {
            continue;
        }
        out.push_str(&code[at..start]);
        out.push_str(ELIDED);
        at = end;
    }
    out.push_str(&code[at..]);
    out
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn skeleton(path: &str, code: &str) -> String {
        extract_skeleton(Path::new(path), code)
    }

    #[test]
    fn a_body_is_elided_and_the_signature_survives() {
        let code = "fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let out = skeleton("a.rs", code);
        assert!(out.contains("fn add(a: u32, b: u32) -> u32"), "{out}");
        assert!(!out.contains("a + b"), "the body survived: {out}");
    }

    #[test]
    fn a_brace_in_a_string_does_not_desynchronize_the_rest_of_the_file() {
        // The bug this closes. Counting braces per line, the `{` inside the
        // string was never matched, so every signature after it was treated as
        // part of a body and dropped.
        let code = r#"
fn first() {
    let s = "unbalanced { brace";
}

fn second_should_survive() -> u32 {
    7
}
"#;
        let out = skeleton("a.rs", code);
        assert!(
            out.contains("fn second_should_survive() -> u32"),
            "a later signature was lost: {out}"
        );
        assert!(!out.contains("let s ="), "{out}");
    }

    #[test]
    fn type_definitions_are_kept_whole() {
        // A struct's fields are its interface. Eliding them would leave a
        // skeleton that says a type exists and nothing about what it holds.
        let code = "struct Config {\n    retries: u32,\n    name: String,\n}\n";
        let out = skeleton("a.rs", code);
        assert!(out.contains("retries: u32"), "{out}");
        assert!(out.contains("name: String"), "{out}");
    }

    #[test]
    fn python_and_typescript_bodies_are_elided_too() {
        let python = skeleton("a.py", "def go(x):\n    return x + 1\n");
        assert!(python.contains("def go(x)"), "{python}");
        assert!(!python.contains("return x + 1"), "{python}");

        let ts = skeleton(
            "a.ts",
            "function go(x: number): number {\n  return x + 1;\n}\n",
        );
        assert!(ts.contains("function go(x: number): number"), "{ts}");
        assert!(!ts.contains("return x + 1"), "{ts}");
    }

    #[test]
    fn go_c_and_cpp_bodies_are_elided_too() {
        let go = skeleton(
            "a.go",
            "func calculate(a int, b int) int {\n    return a + b\n}\n",
        );
        assert!(go.contains("func calculate(a int, b int) int"), "{go}");
        assert!(!go.contains("return a + b"), "{go}");

        let c = skeleton("a.c", "int compute(int x) {\n    return x * 2;\n}\n");
        assert!(c.contains("int compute(int x)"), "{c}");
        assert!(!c.contains("return x * 2"), "{c}");

        let cpp = skeleton(
            "a.cpp",
            "void process(const std::string& s) {\n    std::cout << s;\n}\n",
        );
        assert!(cpp.contains("void process(const std::string& s)"), "{cpp}");
        assert!(!cpp.contains("std::cout"), "{cpp}");
    }

    #[test]
    fn an_unparseable_language_returns_the_source_unchanged() {
        // A partial skeleton of a file nothing parsed is worse than the file:
        // the reader cannot tell what was dropped.
        let code = "some :: unknown ~ syntax\n";
        assert_eq!(skeleton("a.hs", code), code);
    }

    #[test]
    fn a_skeleton_is_materially_smaller_than_the_source() {
        // The whole point is the token saving; a skeleton that saves nothing is
        // a no-op nobody would notice was broken.
        let code = "
fn one() -> u32 {
    let mut total = 0;
    for i in 0..100 {
        total += i;
    }
    total
}

fn two() -> u32 {
    let mut total = 0;
    for i in 0..100 {
        total += i * 2;
    }
    total
}
";
        let out = skeleton("a.rs", code);
        assert!(
            out.len() < code.len() / 2,
            "skeleton was {} bytes of {}: {out}",
            out.len(),
            code.len()
        );
    }
}
