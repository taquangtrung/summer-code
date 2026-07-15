//! Turning a fetched HTML page into the text a model can actually read.
//!
//! **Deliberately not a parser.** A real DOM would let this ask precise
//! questions ("the `<main>` element"), and it would also drag in a parsing
//! dependency and a tree walk to serve one tool. What a model needs from a
//! documentation page is the prose in reading order, which a scanner gets right
//! for well-formed pages and gets *approximately* right for malformed ones,
//! where a strict parser gets nothing at all.
//!
//! The two things it must not do are include script bodies (which are large and
//! are never prose) and run words together across removed tags (which turns
//! `<td>foo</td><td>bar</td>` into `foobar` and invents an identifier that does
//! not exist).

use std::collections::HashMap;
use std::sync::LazyLock;

// ========================================================================
// Constants
// ========================================================================

/// Elements whose entire contents are dropped, not just their tags.
const DROPPED_ELEMENTS: &[&str] = &["head", "noscript", "script", "style", "svg", "template"];

/// Elements that end a line of prose, so text either side of them does not run
/// together into one paragraph.
const BLOCK_ELEMENTS: &[&str] = &[
    "article",
    "blockquote",
    "br",
    "div",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "li",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tr",
    "ul",
];

/// The named character references worth resolving.
///
/// Not the full HTML5 table of 2,231 names: these are the ones that appear in
/// ordinary prose and in code samples, which is what this module is for. An
/// unrecognized entity is left as written, which is readable and honest.
static ENTITIES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("amp", "&"),
        ("apos", "'"),
        ("gt", ">"),
        ("lt", "<"),
        ("mdash", "-"),
        ("nbsp", " "),
        ("ndash", "-"),
        ("quot", "\""),
        ("rsquo", "'"),
    ])
});

// ========================================================================
// Functions
// ========================================================================

/// Extract readable text from an HTML document.
pub fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 2);
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        push_text(&mut text, &rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            // An unterminated `<` is text, not a tag: this is where a strict
            // parser would give up on the whole document.
            push_text(&mut text, &rest[open..]);
            return finish(text);
        };
        let tag = &after[..close];
        rest = &after[close + 1..];

        let name = tag_name(tag);
        if DROPPED_ELEMENTS.contains(&name.as_str()) && !tag.starts_with('/') {
            rest = skip_element(rest, &name);
            text.push('\n');
        } else if BLOCK_ELEMENTS.contains(&name.as_str()) {
            text.push('\n');
        } else {
            // An inline tag still separates words: `a<b>b</b>` is two tokens.
            text.push(' ');
        }
    }
    push_text(&mut text, rest);
    finish(text)
}

/// The lowercased element name of a tag body, without `/` or attributes.
fn tag_name(tag: &str) -> String {
    tag.trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Everything after the closing tag of `name`, or the empty string if the
/// document never closes it.
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let closing = format!("</{name}");
    match rest.to_lowercase().find(&closing) {
        Some(at) => match rest[at..].find('>') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        },
        None => "",
    }
}

/// Append a run of document text, resolving entities.
fn push_text(text: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    let mut rest = chunk;
    while let Some(at) = rest.find('&') {
        text.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        // A `&` with no `;` within a short window is an ampersand in prose, not
        // a broken entity: `Q&A` and `a & b` are both common.
        match after.find(';').filter(|end| *end <= 8) {
            Some(end) => {
                text.push_str(&resolve_entity(&after[..end]));
                rest = &after[end + 1..];
            }
            None => {
                text.push('&');
                rest = after;
            }
        }
    }
    text.push_str(rest);
}

/// One entity body (between `&` and `;`) as the text it stands for.
fn resolve_entity(body: &str) -> String {
    if let Some(digits) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        return u32::from_str_radix(digits, 16)
            .ok()
            .and_then(char::from_u32)
            .map_or_else(|| format!("&{body};"), String::from);
    }
    if let Some(digits) = body.strip_prefix('#') {
        return digits
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map_or_else(|| format!("&{body};"), String::from);
    }
    ENTITIES
        .get(body)
        .map_or_else(|| format!("&{body};"), |text| (*text).to_string())
}

/// Collapse the runs of whitespace the tag stripping left behind.
///
/// Blank lines survive as paragraph breaks, because a page rendered as one
/// unbroken block is materially harder to read than the page was.
fn finish(text: String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.lines() {
        let trimmed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.is_empty() {
            blank_lines += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_lines > 0 {
                out.push('\n');
            }
        }
        blank_lines = 0;
        out.push_str(&trimmed);
    }
    out
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_and_style_bodies_are_dropped_entirely() {
        // Stripping only the tags leaves the JavaScript in the text, which is
        // both the largest part of a modern page and never the answer.
        let html =
            "<p>before</p><script>var x = 1 < 2;</script><style>p{color:red}</style><p>after</p>";
        let text = html_to_text(html);
        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
        assert!(!text.contains("var x"), "script body survived: {text}");
        assert!(!text.contains("color"), "style body survived: {text}");
    }

    #[test]
    fn adjacent_cells_do_not_run_together() {
        // The invented-identifier bug: `<td>foo</td><td>bar</td>` becoming
        // `foobar` hands the model a symbol that does not exist anywhere.
        let text = html_to_text("<table><tr><td>foo</td><td>bar</td></tr></table>");
        assert!(!text.contains("foobar"), "{text}");
        assert!(text.contains("foo"), "{text}");
        assert!(text.contains("bar"), "{text}");
    }

    #[test]
    fn entities_resolve_including_numeric_forms() {
        let text = html_to_text("<p>a &lt; b &amp;&amp; c &gt; d &#65; &#x42;</p>");
        assert_eq!(text, "a < b && c > d A B");
    }

    #[test]
    fn a_bare_ampersand_is_left_alone() {
        // `Q&A` is prose, not a malformed entity, and rewriting it to `Q&A;`
        // would corrupt the page.
        let text = html_to_text("<p>Q&A and a & b</p>");
        assert_eq!(text, "Q&A and a & b");
    }

    #[test]
    fn an_unterminated_tag_does_not_swallow_the_document() {
        // A strict parser rejects this page. The remaining prose is still worth
        // returning.
        let text = html_to_text("<p>kept</p><p>also kept <notclosed");
        assert!(text.contains("kept"), "{text}");
        assert!(text.contains("also kept"), "{text}");
    }

    #[test]
    fn an_unclosed_script_does_not_leak_its_body() {
        // Erring toward dropping the rest of the document is right here: the
        // alternative is emitting a whole minified bundle as if it were prose.
        let text = html_to_text("<p>before</p><script>var x = 1;");
        assert!(text.contains("before"), "{text}");
        assert!(!text.contains("var x"), "{text}");
    }

    #[test]
    fn paragraph_breaks_survive_but_runs_of_blank_lines_do_not() {
        let text = html_to_text("<h1>Title</h1>\n\n\n<p>Body</p>");
        assert_eq!(text, "Title\n\nBody");
    }
}
