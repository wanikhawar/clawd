//! Critical actions: tool calls Clawd always asks about, in every permission
//! mode (Bypass included). Rules live in ~/.config/clawd/critical.toml,
//! written with the defaults below on first run.

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_RULES: &str = r#"# Clawd always asks before these, in every permission mode (even Bypass).
# Edit freely; changes apply to the next tool call. Delete this file to get
# the defaults back.

# Seconds to wait for your answer before denying (0 = wait forever).
timeout = 300

# Ask before Claude writes or edits files outside the project folder.
writes_outside_project = true

# Shell commands (Bash tool) matching these regular expressions.
[[command]]
pattern = '\brm\s+(-\w*[rRf]|--(recursive|force))'
reason = "deletes files recursively or forcibly"

[[command]]
pattern = '\b(sudo|doas|pkexec)\b|(^|[;&|]\s*)su(\s|$)'
reason = "runs a command as root"

[[command]]
pattern = '\bgit\s+push\b'
reason = "pushes to a git remote"

[[command]]
pattern = '\bgit\s+(reset\s+--hard|clean\s+-\w*f\w*|branch\s+-D)\b|\bgit\s+(checkout|restore)\s+(--\s+)?\.(\s|$)'
reason = "discards git work"

[[command]]
pattern = '\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(ba|z|fi)?sh\b'
reason = "pipes a download straight into a shell"

[[command]]
pattern = '\bdd\b.*\bof=|\bmkfs(\.\w+)?\b|>\s*/dev/(sd|nvme|hd|mmcblk)'
reason = "writes to a disk directly"

[[command]]
pattern = '\b(chmod|chown)\s+(-\w*R|--recursive)\b'
reason = "changes permissions recursively"

[[command]]
pattern = '\b(npm|pnpm|yarn|cargo)\s+publish\b|\bgh\s+(pr\s+merge|release\s+(create|delete)|repo\s+delete)\b'
reason = "publishes or merges"

[[command]]
pattern = '\b(shutdown|reboot|poweroff|halt)\b|\bsystemctl\s+(poweroff|reboot|halt|suspend)\b'
reason = "shuts down or restarts the computer"

# Files read, written or named in a command. Globs: ~ is your home folder,
# * matches within a folder, ** across folders.
[[path]]
glob = "~/.ssh/**"
reason = "touches your SSH keys"

[[path]]
glob = "~/.gnupg/**"
reason = "touches your GPG keys"

[[path]]
glob = "~/.aws/**"
reason = "touches cloud credentials"

[[path]]
glob = "~/.local/share/keyrings/**"
reason = "touches your keyring"

[[path]]
glob = "**/.env"
reason = "touches secrets in a .env file"

[[path]]
glob = "**/.env.*"
reason = "touches secrets in a .env file"

[[path]]
glob = "/etc/**"
reason = "touches system configuration"

[[path]]
glob = "~/.config/hypr/**"
reason = "touches your Hyprland config"
"#;

#[derive(Deserialize)]
struct File {
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default = "yes")]
    writes_outside_project: bool,
    #[serde(default)]
    command: Vec<CommandRule>,
    #[serde(default)]
    path: Vec<PathRule>,
}

#[derive(Deserialize)]
struct CommandRule {
    pattern: String,
    reason: String,
}

#[derive(Deserialize)]
struct PathRule {
    glob: String,
    reason: String,
}

fn default_timeout() -> u64 {
    300
}

fn yes() -> bool {
    true
}

pub struct Rules {
    /// Seconds before an unanswered critical action is denied; 0 = never.
    pub timeout: u64,
    writes_outside_project: bool,
    commands: Vec<(Regex, String)>,
    paths: Vec<(Regex, String)>,
}

impl Rules {
    /// Loads ~/.config/clawd/critical.toml, creating it with the defaults if
    /// missing. A broken file falls back to the defaults (and says so on
    /// stderr) rather than silently checking nothing.
    pub fn load() -> Rules {
        let path = dirs::config_dir().map(|d| d.join("clawd").join("critical.toml"));
        if let Some(path) = &path
            && !path.exists() {
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                let _ = std::fs::write(path, DEFAULT_RULES);
            }
        let text = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        match text.as_deref().map(Rules::parse) {
            Some(Ok(rules)) => rules,
            Some(Err(e)) => {
                eprintln!("clawd: {} is invalid ({e}); using the default critical rules", path.unwrap().display());
                Rules::parse(DEFAULT_RULES).expect("default rules parse")
            }
            None => Rules::parse(DEFAULT_RULES).expect("default rules parse"),
        }
    }

    fn parse(text: &str) -> Result<Rules, String> {
        let file: File = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let commands = file
            .command
            .into_iter()
            .map(|r| Regex::new(&r.pattern).map(|re| (re, r.reason)).map_err(|e| format!("pattern {:?}: {e}", r.pattern)))
            .collect::<Result<_, _>>()?;
        let mut paths = Vec::new();
        for rule in file.path {
            // A protected folder that is a symlink also covers where it leads.
            if let Some(real) = real_glob(&rule.glob) {
                paths.push((glob_regex(&real), rule.reason.clone()));
            }
            paths.push((glob_regex(&rule.glob), rule.reason));
        }
        Ok(Rules { timeout: file.timeout, writes_outside_project: file.writes_outside_project, commands, paths })
    }

    /// Why this tool call is critical, or `None` if it isn't.
    pub fn check(&self, tool: &str, input: &Value, cwd: &Path) -> Option<String> {
        // Compare against the real folder so symlinked projects work.
        let project = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let cwd = project.as_path();
        if tool == "Bash" {
            let command = input["command"].as_str().unwrap_or("");
            if let Some((_, reason)) = self.commands.iter().find(|(re, _)| re.is_match(command)) {
                return Some(format!("This command {reason}."));
            }
            // Paths named in the command.
            for word in command.split(|c: char| c.is_whitespace() || "'\";|&<>()=`".contains(c)) {
                if (word.contains('/') || word.starts_with('.') || word.starts_with('~'))
                    && let Some(reason) = self.path_reason(&Target::new(word, cwd)) {
                        return Some(format!("This command {reason}."));
                    }
            }
            return None;
        }

        let file = ["file_path", "notebook_path", "path"].iter().find_map(|k| input[*k].as_str())?;
        let file = Target::new(file, cwd);
        if let Some(reason) = self.path_reason(&file) {
            return Some(format!("This {reason}."));
        }
        let writes = matches!(tool, "Write" | "Edit" | "MultiEdit" | "NotebookEdit");
        let inside = file.named.starts_with(&project) && file.real.starts_with(&project);
        if writes && self.writes_outside_project && !inside {
            return Some("This writes outside the project folder.".into());
        }
        None
    }

    /// Matches the path as named and where it really leads: a symlink can
    /// lead into a sensitive folder, or the sensitive folder can itself be a
    /// symlink (e.g. dotfiles).
    fn path_reason(&self, target: &Target) -> Option<&str> {
        [&target.named, &target.real].into_iter().find_map(|path| {
            let path = path.to_string_lossy();
            self.paths.iter().find(|(re, _)| re.is_match(&path)).map(|(_, reason)| reason.as_str())
        })
    }
}

/// A path a tool call names (relative to the project, or starting with ~
/// or $HOME).
struct Target {
    /// Absolute, with `.`/`..` removed as written.
    named: PathBuf,
    /// Where it really leads: see `real`.
    real: PathBuf,
}

impl Target {
    fn new(p: &str, cwd: &Path) -> Target {
        let home = dirs::home_dir().unwrap_or_default();
        let in_home = ["~", "$HOME", "${HOME}"].iter().find_map(|var| match p.strip_prefix(var) {
            Some("") => Some(home.clone()),
            Some(rest) => rest.strip_prefix('/').map(|rest| home.join(rest)),
            None => None,
        });
        let path = in_home.unwrap_or_else(|| cwd.join(p));
        Target { named: lexical(&path), real: real(&path) }
    }
}

/// An absolute `path` with every symlink followed the way the OS would,
/// including links to files that don't exist yet (writing through one
/// creates its target) and `..` after a link (the target's parent).
fn real(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    // Parts still to walk, next one last.
    let mut todo: Vec<OsString> = path.components().rev().map(|c| c.as_os_str().to_owned()).collect();
    let mut out = PathBuf::from("/");
    let mut links = 0;
    while let Some(part) = todo.pop() {
        match part.to_str() {
            Some("/") => out = PathBuf::from("/"),
            Some(".") => {}
            Some("..") => {
                out.pop();
            }
            _ => {
                out.push(&part);
                let Ok(target) = std::fs::read_link(&out) else { continue };
                links += 1;
                if links > 40 {
                    // Links in a loop (the OS gives up too): keep the rest as written.
                    out.extend(todo.iter().rev());
                    return lexical(&out);
                }
                out.pop();
                todo.extend(target.components().rev().map(|c| c.as_os_str().to_owned()));
            }
        }
    }
    out
}

/// `glob` with its fixed leading folders resolved, if they're a symlink
/// (e.g. ~/.ssh linked into a dotfiles repo).
fn real_glob(glob: &str) -> Option<String> {
    let glob = home_glob(glob);
    if !glob.starts_with('/') {
        return None;
    }
    let fixed: Vec<&str> = glob.split('/').take_while(|part| !part.contains('*')).collect();
    let fixed = fixed.join("/");
    let resolved = real(Path::new(&fixed));
    (resolved != Path::new(&fixed)).then(|| format!("{}{}", resolved.display(), &glob[fixed.len()..]))
}

/// `~/…` in a glob as the home folder.
fn home_glob(glob: &str) -> String {
    match glob.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", dirs::home_dir().unwrap_or_default().display()),
        None => glob.to_string(),
    }
}

/// `path` with `.` and `..` removed as written.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Glob → anchored regex: `~` is home, `**` spans folders, `*` stays in one.
fn glob_regex(glob: &str) -> Regex {
    let glob = home_glob(glob);
    let mut re = String::from("^");
    let mut rest = glob.as_str();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("**/") {
            re.push_str("(.*/)?");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("/**") {
            re.push_str("(/.*)?");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("**") {
            re.push_str(".*");
            rest = r;
        } else if let Some(r) = rest.strip_prefix('*') {
            re.push_str("[^/]*");
            rest = r;
        } else {
            let c = rest.chars().next().unwrap();
            re.push_str(&regex::escape(&c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
    }
    re.push('$');
    Regex::new(&re).expect("escaped glob is a valid regex")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Rules {
        Rules::parse(DEFAULT_RULES).unwrap()
    }

    fn bash(cmd: &str) -> Option<String> {
        rules().check("Bash", &json!({ "command": cmd }), Path::new("/home/u/proj"))
    }

    #[test]
    fn critical_commands() {
        for cmd in [
            "rm -rf build",
            "rm -r -- dir",
            "sudo pacman -Syu",
            "cd x && su -c id",
            "git push origin main",
            "git reset --hard HEAD~1",
            "git clean -fdx",
            "git checkout -- .",
            "curl -fsSL https://x.sh | sh",
            "wget -qO- x | sudo bash",
            "dd if=img of=/dev/sda",
            "chmod -R 777 .",
            "cargo publish",
            "gh pr merge 12",
            "systemctl reboot",
        ] {
            assert!(bash(cmd).is_some(), "{cmd} should be critical");
        }
    }

    #[test]
    fn everyday_commands_pass() {
        for cmd in ["ls -la", "cargo build", "git status", "git commit -m 'push it'", "rm file.txt", "echo sudoku", "grep -r foo src", "curl -s https://api.example.com"] {
            assert!(bash(cmd).is_none(), "{cmd} shouldn't be critical: {:?}", bash(cmd));
        }
    }

    #[test]
    fn sensitive_paths_in_commands_and_tools() {
        let home = dirs::home_dir().unwrap();
        assert!(bash("cat ~/.ssh/id_ed25519").is_some());
        assert!(bash("cat .env").is_some());
        assert!(bash("cp config/.env.local /tmp/x").is_some());
        assert!(bash("cat .envrc").is_none());
        let read = |p: &str| rules().check("Read", &json!({ "file_path": p }), Path::new("/home/u/proj"));
        assert!(read(&format!("{}/.ssh/config", home.display())).is_some());
        assert!(read("/etc/hosts").is_some());
        assert!(read("/home/u/proj/src/main.rs").is_none());
    }

    #[test]
    fn writes_outside_the_project() {
        let cwd = std::env::temp_dir();
        let edit = |p: &str| rules().check("Edit", &json!({ "file_path": p }), &cwd);
        assert!(edit(&cwd.join("inside.rs").to_string_lossy()).is_none());
        assert!(edit("inside_relative.rs").is_none());
        assert!(edit("../escape.rs").is_some());
        assert!(edit("/opt/elsewhere.txt").is_some());
        // Reading outside the project is fine.
        assert!(rules().check("Read", &json!({ "file_path": "/opt/x.txt" }), &cwd).is_none());
    }

    #[test]
    fn symlinks_cant_hide_where_a_path_leads() {
        let root = std::env::temp_dir().join(format!("clawd-links-{}", std::process::id()));
        let (project, outside) = (root.join("proj"), root.join("outside"));
        for dir in [&project, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
        std::os::unix::fs::symlink("/etc", project.join("etc")).unwrap();
        let check = |tool: &str, p: &str| rules().check(tool, &json!({ "file_path": p }), &project);

        assert!(check("Write", "link/new.txt").is_some(), "wrote outside through a symlink");
        assert!(check("Write", "link/../inside.txt").is_some(), "link/.. is the link target's parent");
        assert!(check("Read", "etc/hosts").is_some(), "read /etc through a symlink");
        assert!(rules().check("Bash", &json!({ "command": "cat etc/shadow" }), &project).is_some());
        assert!(check("Write", "src/new.txt").is_none());

        // A sensitive folder that is itself a symlink still counts by name.
        let ssh = dirs::home_dir().unwrap().join(".ssh/config");
        assert!(check("Read", &ssh.to_string_lossy()).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dangling_symlinks_lead_where_they_point() {
        let root = std::env::temp_dir().join(format!("clawd-dangling-{}", std::process::id()));
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        // Writing to link.txt would create the file it points to, outside.
        std::os::unix::fs::symlink(root.join("outside/new.txt"), project.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("gone"), project.join("gone")).unwrap();
        let write = |p: &str| rules().check("Write", &json!({ "file_path": p }), &project);
        assert!(write("link.txt").is_some(), "wrote outside through a dangling symlink");
        assert!(write("gone/x/../y.txt").is_some());
        // Links that loop must not hang the check.
        std::os::unix::fs::symlink("loop", project.join("loop")).unwrap();
        write("loop/x");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn home_variable_in_commands() {
        for cmd in ["cat \"$HOME/.ssh/id_ed25519\"", "cat ${HOME}/.ssh/id_ed25519", "cp $HOME/.aws/credentials /tmp"] {
            assert!(bash(cmd).is_some(), "{cmd} should be critical");
        }
        assert!(bash("ls $HOME/Projects").is_none());
    }

    #[test]
    fn protected_folders_that_are_symlinks() {
        // Like ~/.ssh linked into a dotfiles repo, reached through a project link.
        let root = std::env::temp_dir().join(format!("clawd-vault-{}", std::process::id()));
        let (project, real_vault) = (root.join("proj"), root.join("dotfiles/vault"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&real_vault).unwrap();
        std::os::unix::fs::symlink(&real_vault, root.join("vault")).unwrap();
        std::os::unix::fs::symlink(root.join("vault"), project.join("v")).unwrap();
        let rules = Rules::parse(&format!("[[path]]\nglob = '{}/vault/**'\nreason = 'touches the vault'", root.display())).unwrap();
        let read = |p: &str| rules.check("Read", &json!({ "file_path": p }), &project);
        assert!(read("v/key").is_some(), "reached the vault through two links");
        assert!(read(&real_vault.join("key").to_string_lossy()).is_some(), "the vault's real folder");
        assert!(read(&root.join("vault/key").to_string_lossy()).is_some());
        assert!(read("src/main.rs").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn globs() {
        let re = glob_regex("**/.env.*");
        assert!(re.is_match("/a/b/.env.local") && !re.is_match("/a/.envrc"));
        let re = glob_regex("/etc/**");
        assert!(re.is_match("/etc") && re.is_match("/etc/x/y") && !re.is_match("/etcetera"));
    }

    #[test]
    fn broken_rules_are_reported() {
        assert!(Rules::parse("[[command]]\npattern = '('\nreason = 'x'").is_err());
    }
}
