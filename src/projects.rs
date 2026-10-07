//! Recently used project folders, from Claude Code's own session history.
//!
//! `~/.claude/projects/<encoded path>/<session>.jsonl` holds one file per
//! session. The folder names encode the path lossily ("/" and "." both become
//! "-"), so the real path is read from the `cwd` field the session records.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How far into a session file to look for its `cwd`.
const MAX_LINES: usize = 50;

/// Project folders used with Claude Code, most recent first.
pub fn recent(limit: usize) -> Vec<PathBuf> {
    let Some(root) = dirs::home_dir().map(|h| h.join(".claude/projects")) else { return vec![] };
    let Ok(entries) = fs::read_dir(&root) else { return vec![] };

    let mut found: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| newest_session(&e.path()))
        .filter_map(|(modified, file)| Some((modified, session_cwd(&file)?)))
        .filter(|(_, cwd)| is_project(cwd))
        .collect();
    found.sort_by_key(|&(modified, _)| std::cmp::Reverse(modified));

    let mut seen = Vec::new();
    for (_, cwd) in found {
        if !seen.contains(&cwd) {
            seen.push(cwd);
        }
        if seen.len() == limit {
            break;
        }
    }
    seen
}

fn newest_session(dir: &Path) -> Option<(SystemTime, PathBuf)> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(modified, _)| *modified)
}

fn session_cwd(file: &Path) -> Option<PathBuf> {
    let reader = BufReader::new(fs::File::open(file).ok()?);
    reader.lines().take(MAX_LINES).map_while(Result::ok).find_map(|line| {
        let entry: serde_json::Value = serde_json::from_str(&line).ok()?;
        entry["cwd"].as_str().map(PathBuf::from)
    })
}

/// Where chats that aren't about any project run (Claude Code needs a
/// working folder; this one keeps them resumable). Created on first use.
pub fn no_project() -> PathBuf {
    let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("clawd/chats");
    let _ = fs::create_dir_all(&dir);
    dir.canonicalize().unwrap_or(dir)
}

pub fn is_no_project(path: &Path) -> bool {
    path == no_project()
}

/// Leaves out folders that are gone, temporary, the Claude app's own
/// scratch workspaces, or Clawd's no-project folder (it has its own entry).
fn is_project(path: &Path) -> bool {
    let internal = dirs::config_dir().map(|c| c.join("Claude"));
    path.is_dir()
        && !is_no_project(path)
        && !path.starts_with(std::env::temp_dir())
        && !path.starts_with("/tmp")
        && !internal.is_some_and(|c| path.starts_with(c))
}

/// A past Claude Code conversation in a project folder.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
}

/// One entry of a past conversation, for redisplaying it.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryItem {
    User(String),
    Assistant(String),
    Tool { name: String, summary: String },
}

/// Bigger session files are only read at both ends when listing.
const FULL_READ_LIMIT: u64 = 4 * 1024 * 1024;
const PARTIAL_READ: usize = 1024 * 1024;

/// Claude Code's folder for a project's sessions: the path with every
/// character other than a letter or digit replaced by "-".
fn sessions_dir(cwd: &Path) -> Option<PathBuf> {
    let encoded: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(dirs::home_dir()?.join(".claude/projects").join(encoded))
}

/// Past conversations in `cwd`, most recent first.
pub fn sessions(cwd: &Path, limit: usize) -> Vec<SessionInfo> {
    let Some(dir) = sessions_dir(cwd) else { return vec![] };
    let Ok(entries) = fs::read_dir(dir) else { return vec![] };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort_by_key(|&(modified, _)| std::cmp::Reverse(modified));
    files
        .into_iter()
        .filter_map(|(modified, path)| {
            let id = path.file_stem()?.to_string_lossy().into_owned();
            // Sessions with nothing said in them aren't worth resuming.
            let title = title(&read_ends(&path)?)?;
            Some(SessionInfo { id, title, modified })
        })
        .take(limit)
        .collect()
}

/// The whole file if it's small, else its first and last megabyte (titles
/// are appended over time; the first prompt is near the start).
fn read_ends(path: &Path) -> Option<String> {
    let size = fs::metadata(path).ok()?.len();
    if size <= FULL_READ_LIMIT {
        return fs::read_to_string(path).ok();
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let mut head = vec![0; PARTIAL_READ];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let mut tail = Vec::new();
    file.seek(SeekFrom::End(-(PARTIAL_READ as i64))).ok()?;
    file.read_to_end(&mut tail).ok()?;
    Some(format!("{}\n{}", String::from_utf8_lossy(&head), String::from_utf8_lossy(&tail)))
}

/// Best title: a custom title, Claude Code's generated title, the latest
/// prompt, or the first thing the user said.
fn title(text: &str) -> Option<String> {
    let (mut custom, mut generated, mut last_prompt, mut first) = (None, None, None, None);
    for entry in text.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
        match entry["type"].as_str() {
            Some("custom-title") => custom = entry["customTitle"].as_str().map(str::to_string),
            Some("ai-title") => generated = entry["aiTitle"].as_str().map(str::to_string),
            Some("last-prompt") => last_prompt = entry["lastPrompt"].as_str().map(str::to_string),
            Some("user") if first.is_none() => first = user_text(&entry),
            _ => {}
        }
    }
    let title = custom.or(generated).or(last_prompt).or(first)?;
    let title = title.lines().next().unwrap_or("").trim();
    (!title.is_empty()).then(|| truncate(title, 60))
}

/// What the user typed, if this entry is a real prompt (not a tool result,
/// slash-command plumbing or other meta entry).
fn user_text(entry: &serde_json::Value) -> Option<String> {
    if entry["isMeta"] == true || entry["isSidechain"] == true {
        return None;
    }
    let content = &entry["message"]["content"];
    let text = match content.as_str() {
        Some(s) => s.to_string(),
        None => content
            .as_array()?
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let text = text.trim();
    let plumbing = ["<command-", "<local-command", "Caveat:", "<system-reminder>"];
    (!text.is_empty() && !plumbing.iter().any(|p| text.starts_with(p))).then(|| text.to_string())
}

/// The main thread of a past conversation (its last `limit` entries), and
/// the UUID of its newest entry (where a regenerate would rewind from).
///
/// A session file can hold abandoned branches (a regenerated reply stays in
/// the file), so this follows the newest entry's parent links back to the
/// start instead of reading the file top to bottom.
pub fn history(cwd: &Path, id: &str, limit: usize) -> (Vec<HistoryItem>, Option<String>) {
    let Some(path) = sessions_dir(cwd).map(|d| d.join(format!("{id}.jsonl"))) else { return (vec![], None) };
    let Ok(text) = fs::read_to_string(path) else { return (vec![], None) };
    history_of(&text, limit)
}

fn history_of(text: &str, limit: usize) -> (Vec<HistoryItem>, Option<String>) {
    let entries: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["isSidechain"] != true && e["uuid"].is_string())
        .collect();
    let by_id: std::collections::HashMap<&str, &serde_json::Value> =
        entries.iter().map(|e| (e["uuid"].as_str().unwrap_or(""), e)).collect();
    let newest = entries.iter().rev().find(|e| matches!(e["type"].as_str(), Some("user" | "assistant")));
    let mut chain = std::collections::HashSet::new();
    let mut next = newest;
    while let Some(entry) = next {
        // A damaged file could link in a circle.
        if !chain.insert(entry["uuid"].as_str().unwrap_or("")) {
            break;
        }
        // Compaction starts a new chain that still names what it continues.
        let parent = entry["parentUuid"].as_str().or(entry["logicalParentUuid"].as_str());
        next = parent.and_then(|p| by_id.get(p).copied());
    }
    // Parallel tool calls are stored as side branches off the chain; keep
    // those. A branch that starts with a prompt was abandoned (regenerated).
    let mut shown = chain.clone();
    let mut kept = Vec::new();
    for entry in &entries {
        let id = entry["uuid"].as_str().unwrap_or("");
        let off_chain = entry["parentUuid"].as_str().is_some_and(|p| shown.contains(p)) && user_text(entry).is_none();
        if chain.contains(id) || off_chain {
            shown.insert(id);
            kept.push(entry);
        }
    }

    let mut items = Vec::new();
    for entry in kept {
        match entry["type"].as_str() {
            Some("user") => items.extend(user_text(entry).map(HistoryItem::User)),
            Some("assistant") => {
                for block in entry["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or("").trim();
                            if !text.is_empty() {
                                items.push(HistoryItem::Assistant(text.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or("").to_string();
                            let summary = crate::claude::protocol::tool_summary(&name, &block["input"]);
                            items.push(HistoryItem::Tool { name, summary });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let skip = items.len().saturating_sub(limit);
    (items.split_off(skip), newest.and_then(|e| e["uuid"].as_str()).map(str::to_string))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago", "Sep 28".
pub fn ago(when: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(when).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        86400..172800 => "yesterday".into(),
        172800..604800 => format!("{} days ago", secs / 86400),
        _ => gtk4::glib::DateTime::from_unix_local(
            when.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        )
        .ok()
        .and_then(|d| d.format("%b %-d").ok())
        .map(|s| s.to_string())
        .unwrap_or_default(),
    }
}

/// `~/x/y` instead of `/home/me/x/y`, for display.
pub fn display(path: &Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The folder's own name, for the compact header chip.
pub fn short_name(path: &Path) -> String {
    match path.file_name() {
        Some(name) if dirs::home_dir().as_deref() != Some(path) => name.to_string_lossy().into_owned(),
        _ => display(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cwd_from_session_file() {
        let dir = std::env::temp_dir().join(format!("clawd-projects-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        fs::write(&file, "{\"type\":\"queue-operation\"}\nnot json\n{\"type\":\"user\",\"cwd\":\"/home/me/proj\"}\n").unwrap();
        assert_eq!(session_cwd(&file), Some(PathBuf::from("/home/me/proj")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_prefer_custom_then_generated_then_prompts() {
        let user = |t: &str| format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{t}\"}}}}");
        let first = format!("{}\n{}", user("<command-name>/login</command-name>"), user("fix the build"));
        assert_eq!(title(&first).as_deref(), Some("fix the build"));
        let with_last = format!("{first}\n{{\"type\":\"last-prompt\",\"lastPrompt\":\"and the tests\"}}");
        assert_eq!(title(&with_last).as_deref(), Some("and the tests"));
        let with_ai = format!("{with_last}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Build fixes\"}}");
        assert_eq!(title(&with_ai).as_deref(), Some("Build fixes"));
        let with_custom = format!("{{\"type\":\"custom-title\",\"customTitle\":\"Mine\"}}\n{with_ai}");
        assert_eq!(title(&with_custom).as_deref(), Some("Mine"));
        assert_eq!(title("{\"type\":\"mode\"}"), None);
    }

    #[test]
    fn history_follows_the_newest_branch() {
        // A regenerated reply: "fig" was dropped and "mango" asked again.
        let file = r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"Say one"}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"text","text":"one"}]}}
{"type":"assistant","uuid":"t1","parentUuid":"u1","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"a"}}]}}
{"type":"user","uuid":"r1","parentUuid":"t1","message":{"content":[{"type":"tool_result","content":"x"}]}}
{"type":"user","uuid":"u2","parentUuid":"a1","message":{"content":"A fruit"}}
{"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"content":[{"type":"text","text":"fig"}]}}
{"type":"assistant","uuid":"side","parentUuid":"a2","isSidechain":true,"message":{"content":[]}}
{"type":"user","uuid":"u3","parentUuid":"a1","message":{"content":"A fruit"}}
{"type":"attachment","uuid":"x3","parentUuid":"u3"}
{"type":"assistant","uuid":"a3","parentUuid":"x3","message":{"content":[{"type":"text","text":"mango"}]}}
{"type":"last-prompt","lastPrompt":"A fruit"}"#;
        let (items, newest) = history_of(file, 80);
        let user = |t: &str| HistoryItem::User(t.into());
        let said = |t: &str| HistoryItem::Assistant(t.into());
        let read = HistoryItem::Tool { name: "Read".into(), summary: "a".into() };
        assert_eq!(items, [user("Say one"), said("one"), read, user("A fruit"), said("mango")]);
        assert_eq!(newest.as_deref(), Some("a3"));
        assert_eq!(history_of("", 80), (vec![], None));
    }

    #[test]
    fn history_survives_circular_links() {
        let file = r#"{"type":"user","uuid":"u1","parentUuid":"a1","message":{"content":"hi"}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"text","text":"hello"}]}}
{"type":"user","uuid":"u2","parentUuid":"u2","message":{"content":"again"}}"#;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(history_of(file, 80));
        });
        let (items, _) = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("history_of hung on a cycle");
        assert_eq!(items, [HistoryItem::User("again".into())]);
    }

    #[test]
    fn sessions_folder_encoding_matches_claude_code() {
        let dir = sessions_dir(Path::new("/home/me/Projects/claude_assistant")).unwrap();
        assert!(dir.ends_with(".claude/projects/-home-me-Projects-claude-assistant"));
    }

    #[test]
    fn display_abbreviates_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(display(&home.join("Projects/x")), "~/Projects/x");
        assert_eq!(display(&home), "~");
        assert_eq!(short_name(&home), "~");
        assert_eq!(short_name(&home.join("Projects/x")), "x");
        assert_eq!(display(Path::new("/etc")), "/etc");
    }

    #[test]
    fn skips_temporary_folders() {
        assert!(!is_project(&no_project()));
        assert!(no_project().is_dir());
        assert!(!is_project(&std::env::temp_dir()));
        assert!(!is_project(Path::new("/definitely/not/here")));
    }
}
