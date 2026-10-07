//! A running `claude` process driven over stream-json.

use super::protocol::{self, Decision, Event, Image, PermissionRequest};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long Claude and its tools get to exit cleanly before being killed.
const STOP_GRACE: Duration = Duration::from_millis(400);

/// How Claude's processes are kept together so ending a session can stop all
/// of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Containment {
    /// A systemd user scope: a cgroup no subprocess can leave, so even tools
    /// that detach (`setsid`, daemons) are stopped with the session.
    Scope,
    /// Claude's own process group: catches everything that doesn't start a
    /// new group or session. Used where systemd isn't available.
    Group,
}

impl Containment {
    /// Scopes when the systemd user manager can make them, else groups.
    /// Checked once per run.
    fn best() -> Containment {
        static BEST: OnceLock<Containment> = OnceLock::new();
        *BEST.get_or_init(|| {
            let ok = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok { Containment::Scope } else { Containment::Group }
        })
    }
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    next_id: u64,
    /// The systemd scope Claude runs in, if any.
    scope: Option<String>,
}

impl Session {
    /// Starts Claude Code in `cwd`, optionally resuming `resume`, with `model`
    /// ("default" leaves Claude Code's default) and `effort` (`None` = the
    /// model's default). Events are delivered on the returned channel until
    /// the process exits.
    ///
    /// `rewind` = (last entry to keep, prompt of the turn to drop) resumes
    /// the conversation as it was before that turn, to regenerate it. Claude
    /// Code refuses (with an error result) if more than that turn would be
    /// lost.
    pub fn start(
        cwd: &Path,
        resume: Option<&str>,
        rewind: Option<(&str, &str)>,
        model: &str,
        effort: Option<&str>,
    ) -> io::Result<(Session, async_channel::Receiver<Event>)> {
        let mut args: Vec<String> = [
            "-p",
            "--verbose",
            "--include-partial-messages",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Permission prompts come to us as control requests on stdout.
            "--permission-prompt-tool",
            "stdio",
            // Makes Bypass selectable later; it's never on unless chosen.
            "--allow-dangerously-skip-permissions",
        ]
        .map(String::from)
        .to_vec();
        if let Some(id) = resume {
            args.push(format!("--resume={id}"));
            if let Some((keep, drop)) = rewind {
                args.push(format!("--resume-session-at={keep}"));
                args.push(format!("--resume-drops-turn={drop}"));
            }
        }
        if model != "default" {
            args.extend(["--model".into(), model.into()]);
        }
        if let Some(level) = effort {
            args.extend(["--effort".into(), level.into()]);
        }
        Session::spawn(&claude_binary(), &args, cwd, Containment::best())
    }

    /// Runs `program` as the session's Claude process.
    fn spawn(
        program: &Path,
        args: &[String],
        cwd: &Path,
        containment: Containment,
    ) -> io::Result<(Session, async_channel::Receiver<Event>)> {
        let (mut cmd, scope) = match containment {
            Containment::Scope => {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                let unit = format!("clawd-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
                let mut cmd = Command::new("systemd-run");
                // --scope execs the program in place (same PID and pipes).
                // --expand-environment=no: don't rewrite "$…" in arguments.
                cmd.args(["--user", "--scope", "--quiet", "--collect", "--expand-environment=no"])
                    .arg(format!("--unit={unit}"))
                    .arg("--")
                    .arg(program);
                (cmd, Some(format!("{unit}.scope")))
            }
            Containment::Group => (Command::new(program), None),
        };
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Also its own process group: the fallback, and it keeps terminal
            // signals aimed at Clawd away from Claude.
            .process_group(0);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (tx, rx) = async_channel::unbounded();

        std::thread::spawn({
            let tx = tx.clone();
            move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let _ = tx.send_blocking(Event::Stderr(line));
                }
            }
        });

        std::thread::spawn(move || {
            let mut dedupe = Dedupe::default();
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                for event in protocol::parse(&line) {
                    if !dedupe.keep(&event) {
                        continue;
                    }
                    if tx.send_blocking(event).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send_blocking(Event::Exited);
        });

        let mut session = Session { child, stdin, next_id: 0, scope };
        let id = session.request_id();
        session.send(&protocol::initialize(&id))?;
        Ok((session, rx))
    }

    /// `uuid`: the entry the prompt is saved as ("" lets Claude Code pick).
    pub fn prompt(&mut self, text: &str, images: &[Image], uuid: &str) -> io::Result<()> {
        self.send(&protocol::user_message(text, images, uuid))
    }

    pub fn set_effort(&mut self, level: Option<&str>) -> io::Result<()> {
        let id = self.request_id();
        self.send(&protocol::set_effort(&id, level))
    }

    pub fn answer(&mut self, req: &PermissionRequest, decision: Decision) -> io::Result<()> {
        self.send(&protocol::answer(req, decision))
    }

    pub fn set_model(&mut self, model: &str) -> io::Result<()> {
        let id = self.request_id();
        self.send(&protocol::set_model(&id, model))
    }

    pub fn set_permission_mode(&mut self, mode: &str) -> io::Result<()> {
        let id = self.request_id();
        self.send(&protocol::set_permission_mode(&id, mode))
    }

    /// Lets a tool call that isn't critical continue normally.
    pub fn gate_pass(&mut self, request_id: &str) -> io::Result<()> {
        self.send(&protocol::gate_pass(request_id))
    }

    pub fn request_plan_usage(&mut self) -> io::Result<()> {
        let id = self.request_id();
        self.send(&protocol::get_usage(&id))
    }

    pub fn request_context_usage(&mut self) -> io::Result<()> {
        let id = self.request_id();
        self.send(&protocol::get_context_usage(&id))
    }

    pub fn interrupt(&mut self) -> io::Result<()> {
        let id = self.request_id();
        self.send(&protocol::interrupt(&id))
    }

    fn request_id(&mut self) -> String {
        self.next_id += 1;
        format!("clawd-{}", self.next_id)
    }

    fn send(&mut self, msg: &serde_json::Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.stdin.write_all(&line)?;
        self.stdin.flush()
    }
}

impl Drop for Session {
    /// Stops Claude and everything it started: SIGTERM to all of it, a grace
    /// period until *every* process has exited (tools may need time to clean
    /// up), then SIGKILL for whatever is left.
    fn drop(&mut self) {
        // Claude leads its process group, so the group ID is its PID. That
        // stays reserved until Claude is reaped at the very end, so signals to
        // the group can't reach an unrelated process that reused the number.
        let group = self.child.id() as libc::pid_t;
        let cgroup = self.scope.as_deref().and_then(cgroup_dir);
        let members = || match &cgroup {
            Some(dir) => cgroup_members(dir),
            None => group_members(group),
        };

        signal_all(&members(), libc::SIGTERM);
        unsafe { libc::killpg(group, libc::SIGTERM) };
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && !members().is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }

        if let Some(dir) = &cgroup {
            // Kills every process in the cgroup at once.
            let _ = std::fs::write(dir.join("cgroup.kill"), "1");
        }
        signal_all(&members(), libc::SIGKILL);
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let _ = self.child.wait();
        // Everything in it is gone; don't leave the empty unit behind.
        if let Some(scope) = &self.scope {
            let _ = Command::new("systemctl").args(["--user", "stop", scope]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

fn signal_all(pids: &[libc::pid_t], signal: libc::c_int) {
    for &pid in pids {
        unsafe { libc::kill(pid, signal) };
    }
}

/// The cgroup directory of a systemd user scope, while it exists.
fn cgroup_dir(scope: &str) -> Option<PathBuf> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "--property=ControlGroup", "--value", scope])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// Live (non-zombie) processes in a cgroup.
fn cgroup_members(dir: &Path) -> Vec<libc::pid_t> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .filter(|&pid| !is_zombie(pid))
        .collect()
}

/// Live (non-zombie) processes in a process group.
fn group_members(group: libc::pid_t) -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return vec![] };
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|&pid| proc_stat(pid).is_some_and(|(state, pgrp)| pgrp == group && state != 'Z'))
        .collect()
}

fn is_zombie(pid: libc::pid_t) -> bool {
    proc_stat(pid).is_none_or(|(state, _)| state == 'Z')
}

/// (state, process group) from /proc/PID/stat.
fn proc_stat(pid: libc::pid_t) -> Option<(char, libc::pid_t)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses; fields resume
    // after the last ')': state ppid pgrp …
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    let pgrp = fields.nth(1)?.parse().ok()?;
    Some((state, pgrp))
}

/// Text arrives as deltas, and the finished block repeats it. The full block
/// is only used when nothing was streamed for its message (some messages,
/// like API errors, aren't streamed at all).
#[derive(Default)]
struct Dedupe {
    streamed: bool,
}

impl Dedupe {
    fn keep(&mut self, event: &Event) -> bool {
        match event {
            // A turn's end also ends its message: whatever comes next is new.
            Event::MessageStart | Event::TurnDone { .. } => self.streamed = false,
            Event::TextDelta(_) => self.streamed = true,
            Event::Text(_) => return !self.streamed,
            _ => {}
        }
        true
    }
}

/// `claude` may not be on PATH when Clawd is launched from the compositor.
fn claude_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("CLAWD_CLAUDE") {
        return p.into();
    }
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join("claude"))
        .find(|p| p.is_file());
    on_path
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/bin/claude")).filter(|p| p.is_file()))
        .unwrap_or_else(|| "claude".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in `claude` (a shell script, run as `sh script`: executing a
    /// freshly written file races with other tests' forks, "Text file busy")
    /// that starts two background processes, the way
    /// tools can: one detached into its own session (`setsid`), and one that
    /// needs ~200ms to clean up after SIGTERM. Returns (dir, detached pid
    /// file, slow pid file, clean-exit marker).
    fn fake_claude(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("clawd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (script, detached, slow, clean) = (dir.join("claude"), dir.join("detached.pid"), dir.join("slow.pid"), dir.join("clean"));
        let body = format!(
            "#!/bin/sh\n\
             setsid sleep 300 &\n\
             echo $! > '{detached}'\n\
             sh -c 'trap \"sleep 0.2; touch {clean}; exit 0\" TERM; while :; do sleep 0.05; done' &\n\
             echo $! > '{slow}'\n\
             cat > /dev/null\n",
            detached = detached.display(),
            slow = slow.display(),
            clean = clean.display(),
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        (dir, script, detached, slow, clean)
    }

    #[test]
    fn unstreamed_text_after_a_streamed_turn_is_kept() {
        let mut dedupe = Dedupe::default();
        let text = |t: &str| Event::Text(t.into());
        let done = || Event::TurnDone { is_error: false, error: None };
        let kept: Vec<bool> = [
            Event::MessageStart,
            Event::TextDelta("Hi".into()),
            text("Hi"), // repeats what was streamed
            done(),
            text("API Error: overloaded"), // next turn: never streamed
            done(),
        ]
        .iter()
        .map(|e| dedupe.keep(e))
        .collect();
        assert_eq!(kept, [true, true, false, true, true, true]);
    }

    fn read_pid(file: &Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(file).ok().and_then(|s| s.trim().parse().ok()) {
                return pid;
            }
            assert!(Instant::now() < deadline, "fake claude never wrote {}", file.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn alive(pid: i32) -> bool {
        !is_zombie(pid) && unsafe { libc::kill(pid, 0) } == 0
    }

    /// Waits a little: killed orphans are reaped by init asynchronously.
    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        !alive(pid)
    }

    #[test]
    fn scope_stops_detached_tools_after_a_full_grace_period() {
        if Containment::best() != Containment::Scope {
            eprintln!("skipping: no systemd user manager");
            return;
        }
        let (dir, script, detached, slow, clean) = fake_claude("scope");
        let (session, _rx) = Session::spawn(Path::new("/bin/sh"), &[script.display().to_string()], &dir, Containment::Scope).unwrap();
        let (detached, slow) = (read_pid(&detached), read_pid(&slow));
        assert!(alive(detached) && alive(slow));

        drop(session);
        assert!(gone(detached), "detached process {detached} survived the session");
        assert!(gone(slow), "slow process {slow} survived the session");
        assert!(clean.exists(), "slow process was killed before it finished cleaning up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_fallback_waits_for_tools_to_clean_up() {
        let (dir, script, detached, slow, clean) = fake_claude("group");
        let (session, _rx) = Session::spawn(Path::new("/bin/sh"), &[script.display().to_string()], &dir, Containment::Group).unwrap();
        let (detached, slow) = (read_pid(&detached), read_pid(&slow));

        drop(session);
        assert!(gone(slow), "slow process {slow} survived the session");
        assert!(clean.exists(), "slow process was killed before it finished cleaning up");
        // A process group can't hold a detached process (the reason scopes
        // are preferred); tidy it up ourselves.
        unsafe { libc::kill(detached, libc::SIGKILL) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Talks to the real `claude` CLI (uses your subscription), so it only
    /// runs on request: `CLAWD_MODEL=haiku cargo test -- --ignored live`.
    #[test]
    #[ignore]
    fn live_two_turns_with_permission() {
        let dir = std::env::temp_dir().join(format!("clawd-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let model = std::env::var("CLAWD_MODEL").unwrap_or_else(|_| "default".into());
        let (mut s, rx) = Session::start(&dir, None, None, &model, None).unwrap();

        let next = || rx.recv_blocking().expect("claude exited");
        let turn = |s: &mut Session, prompt: &str, allow: bool| -> (String, bool) {
            s.prompt(prompt, &[], "").unwrap();
            let (mut text, mut asked) = (String::new(), false);
            loop {
                match next() {
                    Event::TextDelta(t) | Event::Text(t) => text.push_str(&t),
                    // Every tool call passes Clawd's gate first.
                    Event::Gate(req) => s.gate_pass(&req.request_id).unwrap(),
                    Event::Permission(req) => {
                        asked = true;
                        let d = if allow { Decision::Allow } else { Decision::Deny };
                        s.answer(&req, d).unwrap();
                    }
                    Event::TurnDone { is_error, error } => {
                        assert!(!is_error, "turn failed: {error:?}");
                        return (text, asked);
                    }
                    _ => {}
                }
            }
        };

        let (_, asked) = turn(&mut s, "Use the Bash tool to run exactly: touch made-by-clawd.txt  Then reply: ok", true);
        assert!(asked, "expected a permission prompt for touch");
        assert!(dir.join("made-by-clawd.txt").exists());

        let (text, _) = turn(&mut s, "Reply with exactly the word: second", true);
        assert!(text.to_lowercase().contains("second"), "got {text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regenerating a reply: restart with the conversation rewound to just
    /// before the prompt and ask again. Claude must not see the dropped turn.
    /// `CLAWD_MODEL=haiku cargo test -- --ignored live`
    #[test]
    #[ignore]
    fn live_regenerate_drops_the_old_reply() {
        let dir = std::env::temp_dir().join(format!("clawd-regen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let model = std::env::var("CLAWD_MODEL").unwrap_or_else(|_| "default".into());
        // Runs a turn; returns (text, session id, newest entry, error).
        let turn = |s: &mut Session, rx: &async_channel::Receiver<Event>, prompt: &str, uuid: &str| {
            s.prompt(prompt, &[], uuid).unwrap();
            let (mut text, mut id, mut entry) = (String::new(), String::new(), String::new());
            loop {
                match rx.recv_blocking().expect("claude exited") {
                    Event::TextDelta(t) | Event::Text(t) => text.push_str(&t),
                    Event::Init { session_id } => id = session_id,
                    Event::Entry(e) => entry = e,
                    Event::Gate(req) | Event::Permission(req) => s.answer(&req, Decision::Deny).unwrap(),
                    Event::TurnDone { error, .. } => return (text, id, entry, error),
                    _ => {}
                }
            }
        };

        let (mut s, rx) = Session::start(&dir, None, None, &model, None).unwrap();
        let (_, id, kept, _) = turn(&mut s, &rx, "Remember the word PELICAN. Reply: ok", &protocol::new_uuid());
        let dropped = protocol::new_uuid();
        turn(&mut s, &rx, "Remember the word WALRUS. Reply: ok", &dropped);
        drop(s);

        let (mut s, rx) = Session::start(&dir, Some(&id), Some((&kept, &dropped)), &model, None).unwrap();
        let ask = "List every word I asked you to remember, in capitals, nothing else.";
        let (text, again, _, error) = turn(&mut s, &rx, ask, &protocol::new_uuid());
        assert_eq!(error, None);
        assert_eq!(again, id, "regenerating should continue the same conversation");
        assert!(text.contains("PELICAN") && !text.contains("WALRUS"), "got {text:?}");

        // A stopped turn can be regenerated too.
        drop(s);
        let (mut s, rx) = Session::start(&dir, Some(&id), None, &model, None).unwrap();
        let (_, _, kept, _) = turn(&mut s, &rx, "Reply: ok", &protocol::new_uuid());
        let stopped = protocol::new_uuid();
        let stop = |s: &mut Session, rx: &async_channel::Receiver<Event>, uuid: &str| {
            s.prompt("Count slowly from 1 to 300, one number per line.", &[], uuid).unwrap();
            let mut entry = None;
            loop {
                match rx.recv_blocking().expect("claude exited") {
                    Event::TextDelta(_) => s.interrupt().unwrap(),
                    Event::Entry(e) => entry = Some(e),
                    Event::TurnDone { .. } => return entry,
                    _ => {}
                }
            }
        };
        stop(&mut s, &rx, &stopped);
        drop(s);
        let (mut s, rx) = Session::start(&dir, Some(&id), Some((&kept, &stopped)), &model, None).unwrap();
        let (_, _, _, error) = turn(&mut s, &rx, "Reply: ok", &protocol::new_uuid());
        assert_eq!(error, None, "regenerating a stopped turn was refused");

        // And the turn after a stopped one (the stop leaves a marker entry).
        let kept = stop(&mut s, &rx, &protocol::new_uuid()).expect("no entry from the stopped turn");
        let next = protocol::new_uuid();
        turn(&mut s, &rx, "Reply: ok", &next);
        drop(s);
        let (mut s, rx) = Session::start(&dir, Some(&id), Some((&kept, &next)), &model, None).unwrap();
        let (_, _, _, error) = turn(&mut s, &rx, "Reply: ok", &protocol::new_uuid());
        assert_eq!(error, None, "regenerating the turn after a stopped one was refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The critical check holds even in Bypass mode.
    /// `CLAWD_MODEL=haiku cargo test -- --ignored live`
    #[test]
    #[ignore]
    fn live_critical_gate_in_bypass_mode() {
        let dir = std::env::temp_dir().join(format!("clawd-gate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("junkdir")).unwrap();
        std::fs::write(dir.join("junkdir/keep.txt"), "x").unwrap();
        let model = std::env::var("CLAWD_MODEL").unwrap_or_else(|_| "default".into());
        let (mut s, rx) = Session::start(&dir, None, None, &model, None).unwrap();
        s.set_permission_mode("bypassPermissions").unwrap();
        s.prompt("First run exactly: ls   Then run exactly: rm -rf ./junkdir   Then say in one sentence what happened.", &[], "").unwrap();

        let rules = crate::critical::Rules::load();
        let (mut passed, mut blocked) = (0, 0);
        loop {
            match rx.recv_blocking().expect("claude exited") {
                Event::Gate(req) => match rules.check(&req.tool_name, &req.input, &dir) {
                    None => {
                        passed += 1;
                        s.gate_pass(&req.request_id).unwrap();
                    }
                    Some(_) => {
                        blocked += 1;
                        s.answer(&req, Decision::Deny).unwrap();
                    }
                },
                Event::TurnDone { .. } => break,
                _ => {}
            }
        }
        assert!(passed >= 1, "the harmless ls should pass the gate");
        assert!(blocked >= 1, "rm -rf should have been flagged as critical");
        assert!(dir.join("junkdir/keep.txt").exists(), "the critical command ran despite being denied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `CLAWD_MODEL=haiku cargo test -- --ignored live`
    #[test]
    #[ignore]
    fn live_image_prompt() {
        use gtk4::gdk_pixbuf::{Colorspace, Pixbuf};
        let red = Pixbuf::new(Colorspace::Rgb, false, 8, 64, 64).unwrap();
        red.fill(0xdc1e1e00);
        let png = red.save_to_bufferv("png", &[]).unwrap();
        let image = crate::image::prepare(&png).unwrap().image;

        let model = std::env::var("CLAWD_MODEL").unwrap_or_else(|_| "default".into());
        let (mut s, rx) = Session::start(&std::env::temp_dir(), None, None, &model, None).unwrap();
        s.prompt("What single color fills this image? Answer with one word.", &[image], "").unwrap();
        let mut text = String::new();
        loop {
            match rx.recv_blocking().expect("claude exited") {
                Event::TextDelta(t) | Event::Text(t) => text.push_str(&t),
                // Every tool call waits on Clawd's gate; this test needs none.
                Event::Gate(req) => s.answer(&req, Decision::Deny).unwrap(),
                Event::Permission(req) => s.answer(&req, Decision::Deny).unwrap(),
                Event::TurnDone { is_error, error } => {
                    assert!(!is_error, "turn failed: {error:?}");
                    break;
                }
                _ => {}
            }
        }
        assert!(text.to_lowercase().contains("red"), "got {text:?}");
    }
}
