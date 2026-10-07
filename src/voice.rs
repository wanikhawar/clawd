//! Voice: record with PipeWire, transcribe locally with whisper.cpp, and
//! optionally read short replies aloud with espeak-ng. Nothing leaves the
//! machine.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

/// whisper.cpp's English base model: small, fast, and accurate enough for
/// prompts.
const MODEL_FILE: &str = "ggml-base.en.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";
/// Recordings stop on their own after this long.
pub const MAX_SECONDS: u64 = 60;

fn find(names: &[&str]) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).flat_map(|dir| names.iter().map(move |n| dir.join(n))).find(|p| p.is_file())
}

/// The whisper.cpp command-line program (its name has changed over time).
pub fn whisper() -> Option<PathBuf> {
    find(&["whisper-cli", "whisper-cpp", "whisper.cpp"])
}

pub fn espeak() -> Option<PathBuf> {
    find(&["espeak-ng", "espeak"])
}

pub fn model_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("clawd").join("models").join(MODEL_FILE))
}

/// Downloads the speech model (blocking; ~148 MB). Writes to a temporary
/// name first, so an interrupted download is never mistaken for a model.
pub fn download_model() -> Result<PathBuf, String> {
    let path = model_path().ok_or("no data folder")?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let partial = path.with_extension("part");
    let status = Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error", "--output"])
        .arg(&partial)
        .arg(MODEL_URL)
        .status()
        .map_err(|e| format!("couldn't run curl ({e})"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&partial);
        return Err("the download failed".into());
    }
    std::fs::rename(&partial, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

pub struct Recording {
    child: Child,
    path: PathBuf,
    pub started: Instant,
}

/// Starts recording the default microphone (16 kHz mono, what whisper wants).
pub fn start_recording() -> Result<Recording, String> {
    let path = std::env::temp_dir().join(format!("clawd-voice-{}.wav", std::process::id()));
    let child = Command::new("pw-record")
        .args(["--rate", "16000", "--channels", "1", "--format", "s16"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start pw-record ({e})"))?;
    Ok(Recording { child, path, started: Instant::now() })
}

impl Recording {
    /// Stops and returns the WAV file.
    pub fn stop(mut self) -> PathBuf {
        self.end();
        self.path.clone()
    }

    /// Stops recording: SIGINT lets pw-record finish the WAV header; if it
    /// hasn't exited in a second, it's killed.
    fn end(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return; // already over
        }
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) };
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Recording {
    /// A recording nobody stopped (e.g. Clawd quit mid-sentence) must not
    /// keep the microphone open; its audio is thrown away.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.end();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Transcribes a WAV file (blocking). Returns the text, trimmed.
pub fn transcribe(wav: &Path) -> Result<String, String> {
    let whisper = whisper().ok_or("whisper-cpp isn't installed")?;
    let model = model_path().filter(|p| p.exists()).ok_or("the speech model isn't downloaded")?;
    let out = Command::new(whisper)
        .arg("--model")
        .arg(&model)
        .arg("--file")
        .arg(wav)
        .args(["--no-timestamps", "--no-prints", "--language", "en"])
        .output()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(wav);
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("transcription failed").to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        // whisper marks silence and noises like "[BLANK_AUDIO]" or "(music)".
        .filter(|l| !l.is_empty() && !(l.starts_with('[') && l.ends_with(']')) && !(l.starts_with('(') && l.ends_with(')')))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(text)
}

/// Starts reading `text` aloud; the returned child can be killed to stop.
pub fn speak(text: &str) -> Option<Child> {
    Command::new(espeak()?).args(["-v", "en-us", "-s", "170"]).arg(text).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()
}

/// Plain text for reading aloud: no code blocks, markdown symbols or URLs.
pub fn speakable(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if !in_code {
            out.push(line.trim_start_matches(['#', '-', '*', '>', ' ']).to_string());
        }
    }
    out.join(" ")
        .replace(['*', '`', '_'], "")
        .split_whitespace()
        .filter(|w| !w.starts_with("http"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in recorder; `ignore_int`: one that ignores SIGINT.
    fn recording(ignore_int: bool) -> (Recording, libc::pid_t) {
        let script = if ignore_int { "trap '' INT; while :; do sleep 0.05; done" } else { "exec sleep 30" };
        let child = Command::new("sh").args(["-c", script]).spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let path = std::env::temp_dir().join(format!("clawd-rec-test-{pid}.wav"));
        std::fs::write(&path, b"").unwrap();
        (Recording { child, path, started: Instant::now() }, pid)
    }

    fn running(pid: libc::pid_t) -> bool {
        // Reaped children are gone from /proc.
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[test]
    fn dropping_a_recording_stops_it() {
        for ignore_int in [false, true] {
            let (rec, pid) = recording(ignore_int);
            std::thread::sleep(std::time::Duration::from_millis(100));
            let path = rec.path.clone();
            drop(rec);
            assert!(!running(pid), "recorder {pid} still running (ignores SIGINT: {ignore_int})");
            assert!(!path.exists(), "unfinished recording left behind");
        }
        let (rec, pid) = recording(false);
        let path = rec.stop();
        assert!(!running(pid) && path.exists(), "a stopped recording keeps its file");
        let _ = std::fs::remove_file(path);
    }

    /// Needs whisper-cpp, the model, and a speech sample:
    /// `CLAWD_VOICE_SAMPLE=jfk.wav cargo test -- --ignored live_transcribe`
    #[test]
    #[ignore]
    fn live_transcribe() {
        let sample = std::env::var("CLAWD_VOICE_SAMPLE").expect("set CLAWD_VOICE_SAMPLE to a 16 kHz WAV");
        // transcribe() deletes its input (it's normally a recording).
        let copy = std::env::temp_dir().join(format!("clawd-voice-test-{}.wav", std::process::id()));
        std::fs::copy(sample, &copy).unwrap();
        let started = Instant::now();
        let text = transcribe(&copy).unwrap();
        eprintln!("transcribed in {:.2?}: {text}", started.elapsed());
        assert!(text.to_lowercase().contains("ask not what your country can do for you"), "{text}");
        assert!(!copy.exists());
    }

    #[test]
    fn speakable_strips_code_and_markup() {
        let md = "## Done\nI fixed **two** bugs in `main.rs`.\n```rust\nfn x() {}\n```\nSee https://example.com for more.";
        assert_eq!(speakable(md), "Done I fixed two bugs in main.rs. See for more.");
    }
}
