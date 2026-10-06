//! What a file-changing tool call would change, as Pango markup for the
//! permission card: removed lines in red, added lines in green, a little
//! context around them.

use gtk4::glib::markup_escape_text;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::path::Path;

/// Lines of unchanged context around each change.
const CONTEXT: usize = 3;
/// Longer diffs are cut off (the card scrolls, but it isn't an editor).
const MAX_LINES: usize = 200;

/// The change `tool` would make, or `None` for tools that don't edit files.
pub fn for_tool(tool: &str, input: &Value, cwd: &Path) -> Option<String> {
    let path = cwd.join(input["file_path"].as_str()?);
    let current = std::fs::read_to_string(&path).ok();
    let body = match tool {
        "Edit" => edit(current.as_deref(), &[(str_at(input, "old_string"), str_at(input, "new_string"))]),
        "MultiEdit" => {
            let edits: Vec<_> = input["edits"]
                .as_array()?
                .iter()
                .map(|e| (str_at(e, "old_string"), str_at(e, "new_string")))
                .collect();
            edit(current.as_deref(), &edits)
        }
        "Write" => {
            let new = str_at(input, "content");
            match current.as_deref() {
                Some(old) => unified(old, new, 1),
                None => new_file(new),
            }
        }
        _ => return None,
    };
    let title = match (tool, &current) {
        ("Write", None) => "new file",
        ("Write", Some(_)) => "replace file",
        _ => "edit",
    };
    Some(format!(
        "<b>{}</b>  <span foreground=\"#9b978e\">{title}</span>\n{body}",
        markup_escape_text(&crate::projects::display(&path))
    ))
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

/// Edits shown one after another, each numbered from where it sits in the
/// current file (when it can be found there).
fn edit(current: Option<&str>, edits: &[(&str, &str)]) -> String {
    edits
        .iter()
        .map(|(old, new)| {
            let first_line = current
                .and_then(|text| text.find(old).filter(|_| !old.is_empty()))
                .map_or(1, |at| text_line(current.unwrap(), at));
            unified(old, new, first_line)
        })
        .collect::<Vec<_>>()
        .join(&hunk_gap())
}

/// 1-based line number of byte offset `at`.
fn text_line(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// A unified diff of `old` → `new`, numbering old lines from `first_line`.
fn unified(old: &str, new: &str, first_line: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for (i, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if i > 0 {
            out.push(hunk_gap());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let number = change.old_index().or(change.new_index()).map_or(0, |n| n + first_line);
                let text = change.value().trim_end_matches('\n');
                out.push(line(change.tag(), number, text));
            }
        }
    }
    if out.is_empty() {
        out.push("<span foreground=\"#9b978e\">(no changes)</span>".into());
    }
    truncate(out)
}

fn new_file(content: &str) -> String {
    let lines: Vec<String> = content.lines().enumerate().map(|(i, l)| line(ChangeTag::Insert, i + 1, l)).collect();
    if lines.is_empty() {
        return "<span foreground=\"#9b978e\">(empty file)</span>".into();
    }
    truncate(lines)
}

fn line(tag: ChangeTag, number: usize, text: &str) -> String {
    let text = markup_escape_text(text);
    match tag {
        ChangeTag::Delete => format!("<span background=\"#4d2320\" foreground=\"#ffc4b8\">{number:>4} - {text}</span>"),
        ChangeTag::Insert => format!("<span background=\"#1d4029\" foreground=\"#bff0c8\">{number:>4} + {text}</span>"),
        ChangeTag::Equal => format!("<span foreground=\"#8a867e\">{number:>4}   {text}</span>"),
    }
}

fn hunk_gap() -> String {
    "<span foreground=\"#6f6b64\">     ⋯</span>".into()
}

fn truncate(mut lines: Vec<String>) -> String {
    if lines.len() > MAX_LINES {
        let more = lines.len() - MAX_LINES;
        lines.truncate(MAX_LINES);
        lines.push(format!("<span foreground=\"#9b978e\">… {more} more lines</span>"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str, content: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clawd-diff-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(c) = content {
            std::fs::write(dir.join("f.rs"), c).unwrap();
        }
        dir
    }

    #[test]
    fn edit_shows_removed_and_added_lines_at_their_line_numbers() {
        let dir = tmp("edit", Some("a\nb\nfn main() {}\nc\n"));
        let out = for_tool("Edit", &json!({"file_path": "f.rs", "old_string": "fn main() {}", "new_string": "fn main() { go(); }"}), &dir).unwrap();
        assert!(out.contains("   3 - fn main() {}"), "{out}");
        assert!(out.contains("   3 + fn main() { go(); }"), "{out}");
        assert!(out.starts_with("<b>"));
    }

    #[test]
    fn write_new_file_and_escaping() {
        let dir = tmp("new", None);
        let out = for_tool("Write", &json!({"file_path": "f.rs", "content": "x < 1 && y\n"}), &dir).unwrap();
        assert!(out.contains("new file"));
        assert!(out.contains("   1 + x &lt; 1 &amp;&amp; y"), "{out}");
    }

    #[test]
    fn write_over_existing_file_diffs_against_it() {
        let dir = tmp("over", Some("one\ntwo\nthree\n"));
        let out = for_tool("Write", &json!({"file_path": "f.rs", "content": "one\n2\nthree\n"}), &dir).unwrap();
        assert!(out.contains("replace file"));
        assert!(out.contains("   2 - two") && out.contains("   2 + 2") && out.contains("   1   one"), "{out}");
    }

    #[test]
    fn other_tools_have_no_diff() {
        assert!(for_tool("Bash", &json!({"command": "ls"}), Path::new("/")).is_none());
    }

    #[test]
    fn long_diffs_are_cut_off() {
        let content: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let dir = tmp("long", None);
        let out = for_tool("Write", &json!({"file_path": "f.rs", "content": content}), &dir).unwrap();
        assert!(out.contains("… 300 more lines"), "{}", &out[out.len() - 200..]);
    }
}
