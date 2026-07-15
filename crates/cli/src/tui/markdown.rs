//! Markdown and code rendering: text in, styled spans out.
//!
//! **Nothing here knows it is in a TUI.** Every function takes a `&str` and
//! returns [`Text`] or [`Span`]s; none of them touch a `Frame`, a `Rect`, or the
//! session state. That is what separates this from `view`, which is otherwise all
//! layout and widgets, and it is why these are the easiest functions in the crate
//! to test: a fenced block in, a styled line out, no terminal involved.
//!
//! **The highlighter is deliberately lexical, not a parser.** It classifies a
//! word against a keyword and type list per language and colours runs of string,
//! comment, and number. That misreads a keyword inside a string in some
//! languages, which is the trade being made knowingly: a real parse would mean a
//! grammar per language in the render path, on every frame, to decide a colour.
//! `query` links tree-sitter for the questions where being wrong actually costs
//! something; this one only has to look right.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

// ========================================================================
// Constants
// ========================================================================

/// Rendered messages kept by [`render_markdown_cached`] before it starts over.
/// A session's worth of replies, with room; past it the cache empties and
/// refills from what is still on screen, rather than growing with the session.
const RENDER_CACHE_ENTRIES: usize = 512;

/// What a tab is drawn as. Four, the width most code using tabs is written
/// to be read at.
pub(crate) const TAB_AS_SPACES: &str = "    ";

thread_local! {
    /// Rendered markdown by a hash of its source. Thread-local because the UI
    /// renders on one thread, and a lock on every frame would buy nothing.
    static RENDER_CACHE: RefCell<HashMap<u64, Text<'static>>> = RefCell::new(HashMap::new());
}

// ========================================================================
// Functions
// ========================================================================

/// [`render_markdown`], remembered by content.
///
/// **The view renders the whole transcript on every frame**, and a frame comes
/// with every streamed token, so re-parsing and re-highlighting every earlier
/// reply each time made a long session slower with every message. A finished
/// reply never changes, so its rendering is computed once.
///
/// **Only for finished text.** The reply still streaming goes through
/// [`render_markdown_live`] instead: each of its frames is a new prefix, so
/// caching them added one entry per token, filled the cache within a reply,
/// and the clear that followed threw away every finished rendering with it.
pub fn render_markdown_cached(text: &str) -> Text<'static> {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let key = hasher.finish();
    RENDER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(rendered) = cache.get(&key) {
            return rendered.clone();
        }
        if cache.len() >= RENDER_CACHE_ENTRIES {
            cache.clear();
        }
        let rendered = render_markdown_live(text);
        cache.insert(key, rendered.clone());
        rendered
    })
}

/// How many renderings the cache holds, for tests of what reaches it.
#[cfg(test)]
pub(super) fn render_cache_len() -> usize {
    RENDER_CACHE.with(|cache| cache.borrow().len())
}

/// [`render_markdown`] for text that is still changing, never cached.
pub fn render_markdown_live(text: &str) -> Text<'static> {
    // Tabs as spaces: ratatui drops control characters, so a tab-indented
    // code block lost its indentation entirely.
    render_markdown(&text.replace('\t', TAB_AS_SPACES))
}

/// Render markdown text to ratatui [`Text`] with syntax-highlighted code blocks,
/// formatted headings, bullet lists, blockquotes, inline code, and bold/italic spans.
pub fn render_markdown(text: &str) -> Text<'static> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                let lang_tag = trimmed.trim_start_matches('`').trim();
                code_lang = if lang_tag.is_empty() {
                    None
                } else {
                    Some(lang_tag.to_lowercase())
                };
                let lang_label = code_lang.as_deref().unwrap_or("code");
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("┌── {lang_label} "),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        "────────────────────────────────",
                        Style::new().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                in_code_block = false;
                code_lang = None;
                lines.push(Line::from(Span::styled(
                    "└──",
                    Style::new().fg(Color::DarkGray),
                )));
            }
            continue;
        }

        if in_code_block {
            let mut spans = vec![Span::styled("│ ", Style::new().fg(Color::DarkGray))];
            if code_lang.as_deref() == Some("diff") {
                spans.extend(highlight_diff_line(line));
            } else {
                spans.extend(highlight_code_line(line, code_lang.as_deref()));
            }
            lines.push(Line::from(spans));
            continue;
        }

        // Markdown headings
        if let Some(rest) = line.strip_prefix("# ") {
            lines.push(Line::from(Span::styled(
                format!("# {rest}"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )));
        } else if let Some(rest) = line.strip_prefix("## ") {
            lines.push(Line::from(Span::styled(
                format!("## {rest}"),
                Style::new()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if let Some(rest) = line.strip_prefix("### ") {
            lines.push(Line::from(Span::styled(
                format!("### {rest}"),
                Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            )));
        } else if let Some(rest) = line.strip_prefix("#### ") {
            lines.push(Line::from(Span::styled(
                format!("#### {rest}"),
                Style::new().fg(Color::Blue),
            )));
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let indent = line.len() - trimmed.len();
            let prefix = " ".repeat(indent);
            let content = &trimmed[2..];
            let mut spans = vec![
                Span::raw(prefix),
                Span::styled(
                    "• ",
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
            ];
            spans.extend(render_inline_markdown(content));
            lines.push(Line::from(spans));
        } else if let Some(dot_idx) = trimmed.find(". ")
            && dot_idx > 0
            && trimmed[..dot_idx].chars().all(|c| c.is_ascii_digit())
        {
            let indent = line.len() - trimmed.len();
            let prefix = " ".repeat(indent);
            let num = &trimmed[..dot_idx];
            let content = &trimmed[dot_idx + 2..];
            let mut spans = vec![
                Span::raw(prefix),
                Span::styled(
                    format!("{num}. "),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
            ];
            spans.extend(render_inline_markdown(content));
            lines.push(Line::from(spans));
        } else if let Some(rest) = line.strip_prefix("> ") {
            let mut spans = vec![Span::styled("│ ", Style::new().fg(Color::DarkGray))];
            let mut inner = render_inline_markdown(rest);
            for span in &mut inner {
                span.style = span.style.add_modifier(Modifier::ITALIC);
            }
            spans.extend(inner);
            lines.push(Line::from(spans));
        } else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            lines.push(Line::from(Span::styled(
                "────────────────────────────────────────",
                Style::new().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from(render_inline_markdown(line)));
        }
    }

    // If text ended with an unclosed code block, add a footer fence
    if in_code_block {
        lines.push(Line::from(Span::styled(
            "└──",
            Style::new().fg(Color::DarkGray),
        )));
    }

    Text::from(lines)
}

/// Render inline markdown spans (`code`, **bold**, *italic*).
pub(super) fn render_inline_markdown(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut cur = String::new();

    while i < n {
        // Inline code: `...`
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|&c| c == '`')
        {
            let code_end = i + 1 + end;
            if !cur.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut cur)));
            }
            let code: String = chars[i + 1..code_end].iter().collect();
            spans.push(Span::styled(code, Style::new().fg(Color::Cyan)));
            i = code_end + 1;
            continue;
        }

        // Bold: **...**
        if i + 1 < n
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && let Some(end) = (i + 2..n - 1).find(|&k| chars[k] == '*' && chars[k + 1] == '*')
        {
            if !cur.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut cur)));
            }
            let bold: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(
                bold,
                Style::new().add_modifier(Modifier::BOLD),
            ));
            i = end + 2;
            continue;
        }

        // Italic: *...*
        //
        // Flanked like CommonMark's: the opening `*` must be followed, and the
        // closing one preceded, by something other than whitespace. Without
        // that, "2 * 3 and 4 * 5" italicised " 3 and 4 " and dropped both
        // asterisks, and "*.rs and *.toml" lost its globs.
        if chars[i] == '*'
            && (i == 0 || chars[i - 1] != '*')
            && (i + 1 < n && chars[i + 1] != '*' && !chars[i + 1].is_whitespace())
            && let Some(end) = (i + 2..n).find(|&k| {
                chars[k] == '*'
                    && !chars[k - 1].is_whitespace()
                    && (k + 1 == n || chars[k + 1] != '*')
            })
        {
            if !cur.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut cur)));
            }
            let italic: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(
                italic,
                Style::new().add_modifier(Modifier::ITALIC),
            ));
            i = end + 1;
            continue;
        }

        cur.push(chars[i]);
        i += 1;
    }

    if !cur.is_empty() {
        spans.push(Span::raw(cur));
    }

    spans
}

/// Colorize one line of a diff: the leading `-`/`+` (or a blank prefix, for
/// context) sets the line's base color, and `[-removed-]`/`{+added+}`
/// markers -- `git diff --word-diff`'s convention, and what
/// `tools::hunk_diff` emits for an `edit` tool result -- pick out
/// the specific words that changed within it, bold and backgrounded on top of
/// the line's own color the way GitHub's diff view does. A line with no
/// markers (real `git diff` output, `/diff`'s workspace diff) still gets the
/// base red/green from its `-`/`+`, so this is a safe drop-in wherever a diff
/// line was tinted whole before.
pub(super) fn highlight_diff_line(line: &str) -> Vec<Span<'static>> {
    let (prefix, rest, base) = if let Some(rest) = line.strip_prefix('-') {
        ("-", rest, Color::Red)
    } else if let Some(rest) = line.strip_prefix('+') {
        ("+", rest, Color::Green)
    } else {
        ("", line, Color::DarkGray)
    };
    let (open, close) = match prefix {
        "-" => ("[-", "-]"),
        "+" => ("{+", "+}"),
        _ => ("", ""),
    };

    let mut spans = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(
            prefix,
            Style::new().fg(base).add_modifier(Modifier::BOLD),
        ));
    }
    if open.is_empty() {
        spans.push(Span::styled(rest.to_string(), Style::new().fg(base)));
        return spans;
    }

    let emphasized_bg = if base == Color::Red {
        Color::Rgb(64, 20, 20)
    } else {
        Color::Rgb(20, 64, 20)
    };
    let mut remaining = rest;
    while let Some(start) = remaining.find(open) {
        if start > 0 {
            spans.push(Span::styled(
                remaining[..start].to_string(),
                Style::new().fg(base),
            ));
        }
        let after_open = &remaining[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            // Unterminated marker: not something our own producers emit, but
            // a body this did not generate (a paste, a resumed journal from
            // an older build) might contain one. Show it plainly rather than
            // eating the rest of the line looking for a close that never
            // comes.
            spans.push(Span::styled(
                remaining[start..].to_string(),
                Style::new().fg(base),
            ));
            remaining = "";
            break;
        };
        spans.push(Span::styled(
            after_open[..end].to_string(),
            Style::new()
                .fg(base)
                .bg(emphasized_bg)
                .add_modifier(Modifier::BOLD),
        ));
        remaining = &after_open[end + close.len()..];
    }
    if !remaining.is_empty() {
        spans.push(Span::styled(remaining.to_string(), Style::new().fg(base)));
    }
    spans
}

/// Tokenize and syntax highlight a single line of code.
pub fn highlight_code_line(line: &str, lang: Option<&str>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;

    let is_hash_comment = matches!(
        lang,
        Some(
            "python"
                | "py"
                | "bash"
                | "sh"
                | "zsh"
                | "shell"
                | "toml"
                | "yaml"
                | "yml"
                | "dockerfile"
        )
    );
    let is_sql = matches!(lang, Some("sql"));

    while i < n {
        // Leading comments: // or /* or # or --
        if (i + 1 < n && chars[i] == '/' && chars[i + 1] == '/')
            || (is_hash_comment && chars[i] == '#')
            || (is_sql && i + 1 < n && chars[i] == '-' && chars[i + 1] == '-')
        {
            let comment: String = chars[i..].iter().collect();
            spans.push(Span::styled(
                comment,
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
            break;
        }

        // Strings: "..." or '...' or `...`
        if chars[i] == '"' || chars[i] == '\'' || chars[i] == '`' {
            let quote = chars[i];
            let mut str_content = String::new();
            str_content.push(quote);
            i += 1;
            let mut escaped = false;
            while i < n {
                let ch = chars[i];
                str_content.push(ch);
                i += 1;
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == quote {
                    break;
                }
            }
            spans.push(Span::styled(str_content, Style::new().fg(Color::Green)));
            continue;
        }

        // Numbers: digits or hex
        if chars[i].is_ascii_digit()
            || (chars[i] == '.' && i + 1 < n && chars[i + 1].is_ascii_digit())
        {
            let mut num_str = String::new();
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                num_str.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(num_str, Style::new().fg(Color::LightCyan)));
            continue;
        }

        // Identifiers and Keywords
        if chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$' || chars[i] == '@' {
            let mut ident = String::new();
            while i < n
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '$'
                    || chars[i] == '@')
            {
                ident.push(chars[i]);
                i += 1;
            }
            // Check macro call: ident!
            if i < n && chars[i] == '!' && (i + 1 == n || chars[i + 1] != '=') {
                ident.push('!');
                i += 1;
                spans.push(Span::styled(
                    ident,
                    Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                ));
                continue;
            }

            if is_code_keyword(&ident) {
                spans.push(Span::styled(
                    ident,
                    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ));
            } else if is_code_type_or_const(&ident) {
                spans.push(Span::styled(ident, Style::new().fg(Color::Yellow)));
            } else if i < n && chars[i] == '(' {
                // Function call
                spans.push(Span::styled(ident, Style::new().fg(Color::Blue)));
            } else {
                // The terminal's own foreground, not white: white text vanishes
                // on a light background, and this is most of a code block.
                spans.push(Span::raw(ident));
            }
            continue;
        }

        // Operators & punctuation
        let ch = chars[i];
        if "->=::+-*/%<>!&|^~?:;,.".contains(ch) {
            let mut op = String::new();
            while i < n && "->=::+-*/%<>!&|^~?:;,".contains(chars[i]) {
                op.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(op, Style::new().fg(Color::LightRed)));
            continue;
        }

        if "()[]{}".contains(ch) {
            spans.push(Span::styled(
                ch.to_string(),
                Style::new().fg(Color::DarkGray),
            ));
            i += 1;
            continue;
        }

        // Whitespace and other chars
        spans.push(Span::raw(ch.to_string()));
        i += 1;
    }

    spans
}

pub(super) fn is_code_keyword(word: &str) -> bool {
    matches!(
        word,
        "as" | "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "crate"
            | "default"
            | "def"
            | "defer"
            | "delete"
            | "do"
            | "dyn"
            | "else"
            | "elif"
            | "enum"
            | "except"
            | "export"
            | "extern"
            | "finally"
            | "fn"
            | "for"
            | "from"
            | "func"
            | "function"
            | "go"
            | "goto"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "include"
            | "interface"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "new"
            | "of"
            | "package"
            | "pub"
            | "raise"
            | "ref"
            | "return"
            | "select"
            | "self"
            | "Self"
            | "sizeof"
            | "static"
            | "struct"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "trait"
            | "try"
            | "type"
            | "typeof"
            | "unsafe"
            | "use"
            | "val"
            | "var"
            | "where"
            | "while"
            | "with"
            | "yield"
    )
}

pub(super) fn is_code_type_or_const(word: &str) -> bool {
    if matches!(
        word,
        "true"
            | "false"
            | "null"
            | "nil"
            | "None"
            | "Some"
            | "Ok"
            | "Err"
            | "undefined"
            | "NaN"
            | "bool"
            | "boolean"
            | "int"
            | "integer"
            | "float"
            | "double"
            | "str"
            | "string"
            | "char"
            | "byte"
            | "void"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "Vec"
            | "String"
            | "Option"
            | "Result"
            | "Box"
            | "Arc"
            | "Rc"
            | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
    ) {
        return true;
    }
    if let Some(first) = word.chars().next()
        && first.is_uppercase()
        && word.len() > 1
        && word.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return true;
    }
    false
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered text of a span run, with the styling dropped: these tests
    /// assert on which characters landed in which span, and comparing styled
    /// spans directly would couple every one of them to the exact colours.
    fn flatten(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn render_markdown_highlights_code_blocks_and_elements() {
        let md = "# Title\n\nHere is some `inline_code()` and **bold text**:\n\n```rust\n// compute sum\npub fn add(a: i32, b: i32) -> i32 {\n    let s = \"hello\";\n    a + b\n}\n```\n\n- item 1\n- item 2\n";
        let text = super::render_markdown(md);
        assert!(!text.lines.is_empty());

        // Check code block header and body
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("┌── rust"), "{rendered}");
        assert!(rendered.contains("│ pub fn add"), "{rendered}");
        assert!(rendered.contains("└──"), "{rendered}");
        assert!(rendered.contains("• item 1"), "{rendered}");
    }

    #[test]
    fn highlight_diff_line_bolds_and_backgrounds_only_the_marked_word() {
        // The point of the feature: a line where one word changed should not
        // read as a wall of uniform red/green -- the changed word itself must
        // stand out, and the markers around it must not show up literally.
        let spans = highlight_diff_line("- foo [-bar-] baz");
        assert_eq!(flatten(&spans), "- foo bar baz", "markers are stripped");

        let emphasized = spans
            .iter()
            .find(|s| s.content.as_ref() == "bar")
            .expect("the marked word is its own span");
        assert!(
            emphasized.style.bg.is_some(),
            "the marked word gets a background"
        );
        assert!(
            emphasized.style.add_modifier.contains(Modifier::BOLD),
            "the marked word is bold"
        );

        let context = spans
            .iter()
            .find(|s| s.content.as_ref() == " foo ")
            .expect("unmarked text on the line is its own span");
        assert!(
            context.style.bg.is_none(),
            "unmarked text on a changed line keeps the plain line color, no background"
        );
    }

    #[test]
    fn highlight_diff_line_colors_by_leading_sign() {
        let added = highlight_diff_line("+ hello");
        assert_eq!(flatten(&added), "+ hello");
        assert!(
            added
                .iter()
                .any(|s| s.content.as_ref() == " hello" && s.style.fg == Some(Color::Green))
        );

        let removed = highlight_diff_line("- hello");
        assert!(
            removed
                .iter()
                .any(|s| s.content.as_ref() == " hello" && s.style.fg == Some(Color::Red))
        );

        let context = highlight_diff_line("  hello");
        assert_eq!(flatten(&context), "  hello");
        assert!(context.iter().all(|s| s.style.fg == Some(Color::DarkGray)));
    }

    #[test]
    fn highlight_diff_line_does_not_panic_on_an_unterminated_marker() {
        // Not something `tools::hunk_diff` emits, but a journal
        // resumed from an older build (or a hand-edited one) could still hand
        // this to the renderer; it must degrade to plain text, not panic.
        let spans = highlight_diff_line("- foo [-bar");
        assert_eq!(flatten(&spans), "- foo [-bar");
    }

    #[test]
    fn a_lone_asterisk_in_prose_is_not_emphasis() {
        // "2 * 3 and 4 * 5" lost both asterisks to an italic run between them.
        let plain = |md: &str| -> String {
            super::render_markdown(md)
                .lines
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
                .collect()
        };
        assert_eq!(plain("2 * 3 and 4 * 5"), "2 * 3 and 4 * 5");
        assert_eq!(plain("match *.rs and *.toml"), "match *.rs and *.toml");
        let emphasis = super::render_markdown("an *important* word");
        assert!(
            emphasis.lines[0]
                .spans
                .iter()
                .any(|span| span.content == "important"
                    && span.style.add_modifier.contains(Modifier::ITALIC)),
            "real emphasis still renders"
        );
    }
}
