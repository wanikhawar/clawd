//! Just enough Markdown → Pango markup for chat replies: bold, italics,
//! inline code, fenced code blocks, headings, bullets and links. Anything
//! else is shown as plain (escaped) text.
//!
//! With a project folder, inline code naming a file that exists there (like
//! `src/main.rs` or `src/main.rs:12`) becomes a link that opens it, and bare
//! URLs become links too.

use gtk4::glib::markup_escape_text;
use gtk4::prelude::*;
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s<>"'`)\]]+[^\s<>"'`)\].,;:!?]"#).unwrap());

/// A reply split into prose and fenced code blocks (so code can get its own
/// widget with a Copy button).
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    Code { lang: String, code: String },
}

/// Splits on ``` fences. An unclosed fence (mid-stream) runs to the end.
pub fn blocks(md: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let (mut text, mut code) = (String::new(), None::<(String, String)>);
    for line in md.lines() {
        let fence = line.trim_start().strip_prefix("```");
        match (fence, code.as_mut()) {
            (Some(lang), None) => {
                if !text.trim().is_empty() {
                    out.push(Block::Text(std::mem::take(&mut text).trim().to_string()));
                }
                text.clear();
                code = Some((lang.trim().to_string(), String::new()));
            }
            (Some(_), Some(_)) => {
                let (lang, body) = code.take().unwrap();
                out.push(Block::Code { lang, code: body.trim_end_matches('\n').to_string() });
            }
            (None, Some((_, body))) => {
                body.push_str(line);
                body.push('\n');
            }
            (None, None) => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    if let Some((lang, body)) = code {
        out.push(Block::Code { lang, code: body.trim_end_matches('\n').to_string() });
    }
    if !text.trim().is_empty() {
        out.push(Block::Text(text.trim().to_string()));
    }
    out
}

/// Pango markup for `md`. `project` enables links to files that exist in it.
pub fn to_pango(md: &str, project: Option<&Path>) -> String {
    let mut lines = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            lines.push(format!("<tt>{}</tt>", markup_escape_text(line)));
            continue;
        }
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        let heading = trimmed.strip_prefix('#').map(|h| h.trim_start_matches('#'));
        lines.push(match heading {
            Some(h) if h.starts_with(' ') => format!("<b>{}</b>", inline(h.trim(), project)),
            _ => match trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
                Some(item) => format!("{indent}• {}", inline(item, project)),
                None => inline(line, project),
            },
        });
    }
    lines.join("\n")
}

/// Inline spans. Unclosed markers (common mid-stream) are left as text.
fn inline(text: &str, project: Option<&Path>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let span = [("`", "tt"), ("**", "b"), ("__", "b"), ("*", "i"), ("_", "i")]
            .iter()
            .filter_map(|&(marker, tag)| {
                let start = rest.find(marker)?;
                let after = &rest[start + marker.len()..];
                let len = after.find(marker)?;
                // `_` inside words (snake_case) isn't emphasis.
                if marker == "_" && start > 0 && rest[..start].ends_with(|c: char| c.is_alphanumeric()) {
                    return None;
                }
                (len > 0).then_some((start, marker, tag, len))
            })
            .min_by_key(|&(start, marker, ..)| (start, std::cmp::Reverse(marker.len())));

        let Some((start, marker, tag, len)) = span else {
            out.push_str(&plain(rest));
            break;
        };
        out.push_str(&plain(&rest[..start]));
        let inner = &rest[start + marker.len()..start + marker.len() + len];
        let rendered = if tag == "tt" { markup_escape_text(inner).to_string() } else { inline(inner, project) };
        let span = format!("<{tag}>{rendered}</{tag}>");
        match project.filter(|_| tag == "tt").and_then(|p| file_link(inner, p)) {
            Some(uri) => out.push_str(&format!("<a href=\"{}\">{span}</a>", markup_escape_text(&uri))),
            None => out.push_str(&span),
        }
        rest = &rest[start + 2 * marker.len() + len..];
    }
    out
}

/// Escaped text with bare URLs turned into links.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for m in URL.find_iter(text) {
        out.push_str(&markup_escape_text(&text[last..m.start()]));
        let url = markup_escape_text(m.as_str());
        out.push_str(&format!("<a href=\"{url}\">{url}</a>"));
        last = m.end();
    }
    out.push_str(&markup_escape_text(&text[last..]));
    out
}

/// A file:// URI if `code` names an existing file or folder in the project
/// (relative or absolute, optionally with a :line or :line:col suffix).
fn file_link(code: &str, project: &Path) -> Option<String> {
    let looks_like_path = code.contains('/') || code.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && ext.len() <= 5 && ext.chars().all(char::is_alphanumeric));
    if !looks_like_path || code.contains(char::is_whitespace) {
        return None;
    }
    let mut path = code;
    // Strip ":12" or ":12:5" line/column suffixes.
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':')
            && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                path = head;
            }
    }
    let full = if Path::new(path).is_absolute() { Path::new(path).to_path_buf() } else { project.join(path) };
    let full = full.canonicalize().ok()?;
    full.starts_with(project).then(|| gtk4::gio::File::for_path(&full).uri().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(s: &str) -> String {
        to_pango(s, None)
    }

    #[test]
    fn inline_spans() {
        assert_eq!(md("a **b** `c<d>` *e*"), "a <b>b</b> <tt>c&lt;d&gt;</tt> <i>e</i>");
    }

    #[test]
    fn unclosed_and_snake_case_stay_literal() {
        assert_eq!(md("**still streaming"), "**still streaming");
        assert_eq!(md("my_var_name"), "my_var_name");
    }

    #[test]
    fn line_blocks() {
        assert_eq!(md("## Title\n- one\n```\nx < 1\n```"), "<b>Title</b>\n• one\n<tt>x &lt; 1</tt>");
    }

    #[test]
    fn urls_become_links() {
        assert_eq!(
            md("see https://example.com/a?b=1&c=2."),
            "see <a href=\"https://example.com/a?b=1&amp;c=2\">https://example.com/a?b=1&amp;c=2</a>."
        );
    }

    #[test]
    fn existing_project_files_become_links() {
        let dir = std::env::temp_dir().join(format!("clawd-md-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        let out = to_pango("open `src/main.rs:12` but not `src/nope.rs` or `cargo build`", Some(&dir));
        let uri = gtk4::gio::File::for_path(dir.join("src/main.rs")).uri();
        assert!(out.contains(&format!("<a href=\"{uri}\"><tt>src/main.rs:12</tt></a>")), "{out}");
        assert!(out.contains("<tt>src/nope.rs</tt>") && !out.contains("nope.rs</tt></a>"));
        assert!(out.contains("<tt>cargo build</tt>"));
        // Outside the project: no link.
        assert!(!to_pango("`/etc/hostname`", Some(&dir)).contains("<a "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_code_blocks() {
        let parts = blocks("Here:\n```rust\nfn main() {}\n```\nDone.");
        assert_eq!(
            parts,
            vec![
                Block::Text("Here:".into()),
                Block::Code { lang: "rust".into(), code: "fn main() {}".into() },
                Block::Text("Done.".into()),
            ]
        );
        // Mid-stream: an unclosed fence is still code.
        assert_eq!(blocks("```\nlet x")[0], Block::Code { lang: String::new(), code: "let x".into() });
    }
}
