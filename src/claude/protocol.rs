//! Claude Code's stream-json protocol (the one the official Agent SDK speaks).
//!
//! Messages are newline-delimited JSON. Prompts go in as `user` messages;
//! replies come out as `system`, `stream_event`, `assistant`, `user` (tool
//! results) and `result` messages. Permission prompts arrive as a
//! `control_request` with subtype `can_use_tool` and are answered with a
//! `control_response` carrying `{"behavior": "allow" | "deny", ...}`.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub enum Event {
    Init { session_id: String },
    /// The models Claude Code offers, from the initialize handshake.
    Models(Vec<ModelInfo>),
    /// A new top-level assistant message started.
    MessageStart,
    TextDelta(String),
    /// Full text of a finished assistant text block. Only used when no
    /// deltas were streamed for it.
    Text(String),
    ToolUse { name: String, summary: String },
    Permission(PermissionRequest),
    /// Claude is about to run a tool (any mode, even bypass) and waits for
    /// Clawd's critical-action check. Every gate must be answered.
    Gate(PermissionRequest),
    /// Claude withdrew a pending permission prompt (e.g. after an interrupt).
    PermissionCancelled { request_id: String },
    TurnDone { is_error: bool, error: Option<String> },
    /// Slash commands and skills usable in this session.
    Commands(Vec<SlashCommand>),
    /// Claude added a task to its checklist.
    TaskCreated { id: String, subject: String },
    /// Claude changed a task (status "deleted" removes it).
    TaskUpdated { id: String, status: Option<String>, subject: Option<String>, active_form: Option<String> },
    /// A message was added to the conversation (main thread): its UUID, the
    /// point a regenerate rewinds to.
    Entry(String),
    /// How full the conversation's context window is.
    ContextUsage { percent: f64, used: u64, max: u64 },
    /// A plan limit's status changed (fetch fresh numbers with get_usage).
    RateLimitChanged,
    /// Your plan's usage windows, from `get_usage` (what /usage shows).
    PlanUsage(Vec<PlanLimit>),
    Stderr(String),
    Exited,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// What `--model` / `set_model` take, e.g. "sonnet" or "claude-opus-4-8".
    pub value: String,
    /// The concrete model id this resolves to, e.g. "claude-opus-5-5".
    pub resolved: String,
    pub display_name: String,
    pub description: String,
    /// Effort levels the model accepts ("low" … "max"); empty if it has none.
    pub effort_levels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    /// e.g. "[file]"; empty if the command takes no arguments.
    pub hint: String,
}

/// One plan usage window.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanLimit {
    /// "5-hour", "Weekly", "Weekly Opus", …
    pub label: String,
    /// 0–100.
    pub percent: f64,
    /// "normal", "warning", or worse.
    pub severity: String,
    /// ISO 8601.
    pub resets_at: Option<String>,
}

/// An image attached to a prompt.
#[derive(Debug, Clone)]
pub struct Image {
    pub media_type: &'static str,
    /// Base64-encoded bytes.
    pub data: String,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub kind: RequestKind,
    /// Set when this is a critical action, with the reason it's critical.
    pub critical: Option<String>,
    pub request_id: String,
    pub tool_name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub decision_reason: Option<String>,
    pub input: Value,
    pub tool_use_id: String,
    /// Rules Claude Code proposes for "always allow" (same as the terminal's
    /// "Yes, and don't ask again" option).
    pub suggestions: Vec<Value>,
}

/// Where a request came from, which decides how it's answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Claude Code's own permission prompt (`can_use_tool`).
    CanUseTool,
    /// Clawd's PreToolUse hook (`hook_callback`), used for critical actions.
    Gate,
}

/// The hook callback ID Clawd registers for its critical-action check.
pub const GATE_CALLBACK: &str = "clawd-gate";

impl PermissionRequest {
    /// The rule an "Always allow" answer would add, for display.
    pub fn always_rule(&self) -> Option<String> {
        self.always_updates().iter().find_map(|s| {
            let rule = s["rules"].as_array()?.first()?;
            let tool = rule["toolName"].as_str()?;
            Some(match rule["ruleContent"].as_str() {
                Some(content) => format!("{tool}({content})"),
                None => tool.to_string(),
            })
        })
    }

    /// Suggestions applied on "Always allow". Mode changes are left out: the
    /// permission mode is the user's choice, not a side effect of one answer.
    fn always_updates(&self) -> Vec<Value> {
        self.suggestions
            .iter()
            .filter(|s| s["type"] != "setMode")
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    AlwaysAllow,
    Deny,
}

/// Parses one stdout line. Lines that don't matter to Clawd yield nothing.
pub fn parse(line: &str) -> Vec<Event> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else { return vec![] };
    // Subagent traffic carries a parent tool id; only the main thread is shown.
    if !msg["parent_tool_use_id"].is_null() {
        return vec![];
    }
    match msg["type"].as_str() {
        Some("system") if msg["subtype"] == "init" => vec![Event::Init {
            session_id: str_of(&msg["session_id"]),
        }],
        Some("stream_event") => {
            let ev = &msg["event"];
            match ev["type"].as_str() {
                Some("message_start") => vec![Event::MessageStart],
                Some("content_block_delta") if ev["delta"]["type"] == "text_delta" => {
                    vec![Event::TextDelta(str_of(&ev["delta"]["text"]))]
                }
                _ => vec![],
            }
        }
        Some("assistant") => msg["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| match block["type"].as_str()? {
                "text" => Some(Event::Text(str_of(&block["text"]))),
                "tool_use" => Some(Event::ToolUse {
                    name: str_of(&block["name"]),
                    summary: tool_summary(block["name"].as_str().unwrap_or(""), &block["input"]),
                }),
                _ => None,
            })
            .chain(task_updates(&msg["message"]["content"]))
            .chain(entry(&msg))
            .collect(),
        // Tool results: TaskCreate's carries the new task's ID.
        Some("user") => {
            let result = if msg["tool_use_result"].is_null() { &msg["toolUseResult"] } else { &msg["tool_use_result"] };
            let task = match (result["task"]["id"].as_str(), result["task"]["subject"].as_str()) {
                (Some(id), Some(subject)) => Some(Event::TaskCreated { id: id.into(), subject: subject.into() }),
                _ => None,
            };
            task.into_iter().chain(entry(&msg)).collect()
        }
        Some("rate_limit_event") => vec![Event::RateLimitChanged],
        Some("control_response") if msg["response"]["response"].get("rate_limits").is_some() => {
            vec![Event::PlanUsage(plan_limits(&msg["response"]["response"]["rate_limits"]))]
        }
        Some("control_response") if msg["response"]["response"]["maxTokens"].is_number() => {
            let r = &msg["response"]["response"];
            vec![Event::ContextUsage {
                percent: r["percentage"].as_f64().unwrap_or(0.0),
                used: r["totalTokens"].as_u64().unwrap_or(0),
                max: r["maxTokens"].as_u64().unwrap_or(0),
            }]
        }
        Some("control_request") if msg["request"]["subtype"] == "can_use_tool" => {
            let r = &msg["request"];
            vec![Event::Permission(PermissionRequest {
                kind: RequestKind::CanUseTool,
                critical: None,
                request_id: str_of(&msg["request_id"]),
                tool_name: str_of(&r["tool_name"]),
                display_name: r["display_name"].as_str().or(r["tool_name"].as_str()).unwrap_or("").to_string(),
                description: r["description"].as_str().map(str::to_string),
                decision_reason: r["decision_reason"].as_str().map(str::to_string),
                input: r["input"].clone(),
                tool_use_id: str_of(&r["tool_use_id"]),
                suggestions: r["permission_suggestions"].as_array().cloned().unwrap_or_default(),
            })]
        }
        Some("control_request")
            if msg["request"]["subtype"] == "hook_callback" && msg["request"]["callback_id"] == GATE_CALLBACK =>
        {
            let input = &msg["request"]["input"];
            let tool = str_of(&input["tool_name"]);
            vec![Event::Gate(PermissionRequest {
                kind: RequestKind::Gate,
                critical: None,
                request_id: str_of(&msg["request_id"]),
                display_name: tool.clone(),
                tool_name: tool,
                description: input["tool_input"]["description"].as_str().map(str::to_string),
                decision_reason: None,
                input: input["tool_input"].clone(),
                tool_use_id: str_of(&input["tool_use_id"]),
                suggestions: vec![],
            })]
        }
        Some("control_cancel_request") => vec![Event::PermissionCancelled {
            request_id: str_of(&msg["request_id"]),
        }],
        Some("control_response") if msg["response"]["response"]["models"].is_array() => {
            let models = msg["response"]["response"]["models"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| ModelInfo {
                    value: str_of(&m["value"]),
                    resolved: m["resolvedModel"].as_str().or(m["value"].as_str()).unwrap_or("").to_string(),
                    display_name: str_of(&m["displayName"]),
                    description: str_of(&m["description"]),
                    effort_levels: m["supportedEffortLevels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect(),
                })
                .filter(|m| !m.value.is_empty())
                .collect();
            let commands = msg["response"]["response"]["commands"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some(SlashCommand {
                        name: c["name"].as_str()?.to_string(),
                        description: str_of(&c["description"]),
                        hint: str_of(&c["argumentHint"]),
                    })
                })
                .collect();
            vec![Event::Models(models), Event::Commands(commands)]
        }
        Some("control_response") if msg["response"]["subtype"] == "error" => {
            vec![Event::Stderr(format!("control error: {}", msg["response"]["error"]))]
        }
        Some("result") => {
            let is_error = msg["is_error"].as_bool().unwrap_or(false);
            let error = is_error.then(|| {
                msg["result"]
                    .as_str()
                    .or(msg["errors"][0].as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| str_of(&msg["subtype"]))
            });
            vec![Event::TurnDone { is_error, error }]
        }
        _ => vec![],
    }
}

/// Usage windows from a get_usage response. Prefers its `limits` list (with
/// severities); falls back to the named windows.
fn plan_limits(limits: &Value) -> Vec<PlanLimit> {
    let label = |kind: &str| match kind {
        "session" | "five_hour" => "5-hour".to_string(),
        "weekly_all" | "seven_day" => "Weekly".to_string(),
        other => {
            let words: Vec<String> = other
                .split('_')
                .map(|w| match w {
                    "seven" | "day" => String::new(),
                    "weekly" => "Weekly".into(),
                    w => {
                        let mut c = w.chars();
                        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
                    }
                })
                .filter(|w| !w.is_empty())
                .collect();
            if other.starts_with("seven_day") { format!("Weekly {}", words.join(" ")) } else { words.join(" ") }
        }
    };
    if let Some(list) = limits["limits"].as_array() {
        return list
            .iter()
            .filter_map(|l| {
                Some(PlanLimit {
                    label: label(l["kind"].as_str()?),
                    percent: l["percent"].as_f64()?,
                    severity: l["severity"].as_str().unwrap_or("normal").to_string(),
                    resets_at: l["resets_at"].as_str().map(str::to_string),
                })
            })
            .collect();
    }
    ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"]
        .iter()
        .filter_map(|key| {
            let w = &limits[*key];
            let percent = w["utilization"].as_f64()?;
            Some(PlanLimit {
                label: label(key),
                percent,
                severity: if percent >= 100.0 { "critical" } else if percent >= 80.0 { "warning" } else { "normal" }.into(),
                resets_at: w["resets_at"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// The conversation entry a message was saved as, if it says.
fn entry(msg: &Value) -> Option<Event> {
    msg["uuid"].as_str().map(|id| Event::Entry(id.to_string()))
}

/// TaskUpdate calls in an assistant message, as checklist changes.
fn task_updates(content: &Value) -> Vec<Event> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "tool_use" && b["name"] == "TaskUpdate")
        .filter_map(|b| {
            let input = &b["input"];
            Some(Event::TaskUpdated {
                id: input["taskId"].as_str()?.to_string(),
                status: input["status"].as_str().map(str::to_string),
                subject: input["subject"].as_str().map(str::to_string),
                active_form: input["activeForm"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// One-line description of a tool call for the transcript and permission card.
pub fn tool_summary(tool: &str, input: &Value) -> String {
    let field = match tool {
        "Bash" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "file_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        _ => "",
    };
    match input[field].as_str() {
        Some(s) => s.to_string(),
        None if input.is_null() => String::new(),
        None => serde_json::to_string(input).unwrap_or_default(),
    }
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

// ---- Outgoing messages ----

/// The handshake. Registers Clawd's PreToolUse hook, which Claude Code calls
/// before every tool use in every permission mode (bypass included).
pub fn initialize(request_id: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "initialize",
            "hooks": {
                // Timeout in seconds: long enough for the user to answer;
                // Clawd enforces its own (configurable) answer timeout.
                "PreToolUse": [{ "matcher": "*", "hookCallbackIds": [GATE_CALLBACK], "timeout": 86400 }],
            },
        },
    })
}

/// Lets a gated tool call through to Claude Code's normal permission flow.
pub fn gate_pass(request_id: &str) -> Value {
    json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": {} } })
}

/// "default" (ask before actions), "acceptEdits", "auto", "plan", "dontAsk"
/// or "bypassPermissions".
pub fn set_permission_mode(request_id: &str, mode: &str) -> Value {
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": "set_permission_mode", "mode": mode } })
}

/// Plan usage windows only (skips the slow local transcript scan).
pub fn get_usage(request_id: &str) -> Value {
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": "get_usage", "skip_behaviors": true } })
}

pub fn get_context_usage(request_id: &str) -> Value {
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": "get_context_usage", "detail": "summary" } })
}

pub fn interrupt(request_id: &str) -> Value {
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": "interrupt" } })
}

/// Switches the model for the rest of the session. "default" resets to
/// Claude Code's default.
pub fn set_model(request_id: &str, model: &str) -> Value {
    let model = if model == "default" { Value::Null } else { json!(model) };
    json!({ "type": "control_request", "request_id": request_id, "request": { "subtype": "set_model", "model": model } })
}

/// Sets the reasoning effort for the rest of the session; `None` returns to
/// the model's default.
pub fn set_effort(request_id: &str, level: Option<&str>) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "apply_flag_settings", "settings": { "effortLevel": level } },
    })
}

/// A fresh random UUID (v4), from the kernel.
pub fn new_uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// A prompt with optional images (sent before the text, as the API recommends).
/// `uuid` names the conversation entry it's saved as, so this turn can later
/// be rewound and regenerated (empty: Claude Code picks one).
pub fn user_message(text: &str, images: &[Image], uuid: &str) -> Value {
    let mut content: Vec<Value> = images
        .iter()
        .map(|img| json!({ "type": "image", "source": { "type": "base64", "media_type": img.media_type, "data": img.data } }))
        .collect();
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    let mut msg = json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
    });
    if !uuid.is_empty() {
        msg["uuid"] = json!(uuid);
    }
    msg
}

pub fn answer(req: &PermissionRequest, decision: Decision) -> Value {
    if req.kind == RequestKind::Gate {
        // An approved critical action skips the normal prompt: it was just
        // explicitly approved.
        let (verdict, reason) = match decision {
            Decision::Deny => ("deny", "The user denied this critical action in Clawd."),
            _ => ("allow", "The user approved this critical action in Clawd."),
        };
        let output = json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": verdict,
            "permissionDecisionReason": reason,
        }});
        return json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": req.request_id, "response": output },
        });
    }
    let result = match decision {
        Decision::Allow => json!({
            "behavior": "allow",
            "updatedInput": req.input,
            "toolUseID": req.tool_use_id,
        }),
        Decision::AlwaysAllow => json!({
            "behavior": "allow",
            "updatedInput": req.input,
            "updatedPermissions": req.always_updates(),
            "toolUseID": req.tool_use_id,
        }),
        Decision::Deny => json!({
            "behavior": "deny",
            "message": "The user denied this action from Clawd.",
            "toolUseID": req.tool_use_id,
        }),
    };
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": req.request_id, "response": result },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from Claude Code 2.1.291 (trimmed).
    const ASK: &str = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"touch probe.txt","description":"Create an empty file named probe.txt"},"description":"Create an empty file named probe.txt","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch probe.txt"}],"behavior":"allow","destination":"localSettings"},{"type":"setMode","mode":"acceptEdits","destination":"session"}],"tool_use_id":"toolu_1"}}"#;

    #[test]
    fn parses_permission_request() {
        let Some(Event::Permission(req)) = parse(ASK).into_iter().next() else { panic!() };
        assert_eq!(req.tool_name, "Bash");
        assert_eq!(tool_summary(&req.tool_name, &req.input), "touch probe.txt");
        assert_eq!(req.always_rule().as_deref(), Some("Bash(touch probe.txt)"));
    }

    #[test]
    fn always_allow_drops_mode_changes() {
        let Some(Event::Permission(req)) = parse(ASK).into_iter().next() else { panic!() };
        let resp = answer(&req, Decision::AlwaysAllow);
        let updates = resp["response"]["response"]["updatedPermissions"].as_array().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["type"], "addRules");
        assert_eq!(resp["response"]["request_id"], "r1");
    }

    #[test]
    fn parses_model_list() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"clawd-1","response":{"commands":[],"models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","description":"Opus 5.5","supportedEffortLevels":["low","high"]},{"value":"haiku","resolvedModel":"claude-haiku-4-5-20251001","displayName":"Haiku 4.5","description":"Fastest"}]}}}"#;
        let Some(Event::Models(models)) = parse(line).into_iter().next() else { panic!() };
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].resolved, "claude-opus-5-5");
        assert_eq!(models[0].effort_levels, ["low", "high"]);
        assert_eq!(models[1].value, "haiku");
        assert_eq!(models[1].display_name, "Haiku 4.5");
        assert!(models[1].effort_levels.is_empty());
    }

    #[test]
    fn set_model_default_is_null() {
        assert!(set_model("x", "default")["request"]["model"].is_null());
        assert_eq!(set_model("x", "sonnet")["request"]["model"], "sonnet");
    }

    #[test]
    fn user_message_puts_images_first() {
        let img = Image { media_type: "image/png", data: "AAAA".into() };
        let msg = user_message("what is this?", &[img], "");
        let content = msg["message"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["data"], "AAAA");
        assert_eq!(content[1]["text"], "what is this?");
        assert_eq!(user_message("", &[Image { media_type: "image/png", data: "A".into() }], "")["message"]["content"].as_array().unwrap().len(), 1);
        assert!(msg.get("uuid").is_none());
    }

    #[test]
    fn prompts_carry_their_entry_uuid() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_ne!(id, new_uuid());
        assert_eq!(user_message("hi", &[], &id)["uuid"], id.as_str());
    }

    #[test]
    fn reports_conversation_entries() {
        // Trimmed from Claude Code 2.1.292.
        let reply = r#"{"type":"assistant","parent_tool_use_id":null,"uuid":"bf91ef5c","message":{"content":[{"type":"text","text":"one"}]}}"#;
        assert!(matches!(parse(reply).last(), Some(Event::Entry(id)) if id == "bf91ef5c"));
        let tool_result = r#"{"type":"user","parent_tool_use_id":null,"uuid":"c8b1df94","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        assert!(matches!(parse(tool_result).as_slice(), [Event::Entry(id)] if id == "c8b1df94"));
        // A subagent's messages aren't part of the main conversation.
        let sub = r#"{"type":"assistant","parent_tool_use_id":"t1","uuid":"x","message":{"content":[]}}"#;
        assert!(parse(sub).is_empty());
    }

    #[test]
    fn refused_resume_reports_its_reason() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["Resume rejected by --resume-drops-turn"]}"#;
        assert!(matches!(&parse(line)[0], Event::TurnDone { is_error: true, error: Some(e) } if e.starts_with("Resume rejected")));
    }

    #[test]
    fn set_effort_auto_is_null() {
        assert!(set_effort("x", None)["request"]["settings"]["effortLevel"].is_null());
        assert_eq!(set_effort("x", Some("low"))["request"]["settings"]["effortLevel"], "low");
    }

    #[test]
    fn parses_gate_and_answers_it_as_a_hook() {
        // Captured from Claude Code 2.1.291 in bypass mode (trimmed).
        let line = r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback","callback_id":"clawd-gate","input":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"touch blocked.txt","description":"Create blocked.txt file"},"tool_use_id":"toolu_9","permission_mode":"bypassPermissions"}}}"#;
        let Some(Event::Gate(req)) = parse(line).into_iter().next() else { panic!() };
        assert_eq!(req.kind, RequestKind::Gate);
        assert_eq!(req.tool_name, "Bash");
        assert_eq!(req.input["command"], "touch blocked.txt");
        let deny = answer(&req, Decision::Deny);
        assert_eq!(deny["response"]["request_id"], "h1");
        assert_eq!(deny["response"]["response"]["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(gate_pass("h1")["response"]["response"], json!({}));
        assert_eq!(initialize("i")["request"]["hooks"]["PreToolUse"][0]["hookCallbackIds"][0], GATE_CALLBACK);
    }

    #[test]
    fn parses_task_checklist_changes() {
        // Captured from Claude Code 2.1.291.
        let created = r#"{"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Task #1 created successfully: Write tests"}]},"tool_use_result":{"task":{"id":"1","subject":"Write tests"}}}"#;
        assert_eq!(
            parse(created).first().map(|e| format!("{e:?}")),
            Some(format!("{:?}", Event::TaskCreated { id: "1".into(), subject: "Write tests".into() }))
        );
        let update = r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"t3","name":"TaskUpdate","input":{"taskId":"1","status":"completed"}}]}}"#;
        let events = parse(update);
        assert!(matches!(&events[1], Event::TaskUpdated { id, status: Some(s), .. } if id == "1" && s == "completed"), "{events:?}");
    }

    #[test]
    fn parses_usage_events() {
        let usage = r#"{"type":"control_response","response":{"subtype":"success","request_id":"c","response":{"percentage":23.5,"totalTokens":47000,"maxTokens":200000,"categories":[]}}}"#;
        assert!(matches!(parse(usage)[0], Event::ContextUsage { used: 47000, max: 200000, .. }));
        let limit = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.82,"resetsAt":1791400000}}"#;
        assert!(matches!(parse(limit)[0], Event::RateLimitChanged));
    }

    #[test]
    fn parses_plan_usage() {
        // Trimmed from Claude Code 2.1.291 on a Pro plan.
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"u","response":{"subscription_type":"pro","rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":86,"resets_at":"2026-10-06T22:19:59.6+00:00"},"seven_day":{"utilization":7,"resets_at":"2026-10-13T18:59:59.6+00:00"},"limits":[{"kind":"session","group":"session","percent":86,"severity":"warning","resets_at":"2026-10-06T22:19:59.6+00:00"},{"kind":"weekly_all","group":"weekly","percent":7,"severity":"normal","resets_at":"2026-10-13T18:59:59.6+00:00"}]}}}}"#;
        let Event::PlanUsage(limits) = &parse(line)[0] else { panic!() };
        assert_eq!(limits.len(), 2);
        assert_eq!((limits[0].label.as_str(), limits[0].percent, limits[0].severity.as_str()), ("5-hour", 86.0, "warning"));
        assert_eq!((limits[1].label.as_str(), limits[1].percent), ("Weekly", 7.0));
        // Without the list, the named windows are used.
        let fallback = serde_json::json!({ "five_hour": { "utilization": 90.0 }, "seven_day_opus": { "utilization": 10.0 } });
        let limits = plan_limits(&fallback);
        assert_eq!(limits[0].severity, "warning");
        assert_eq!(limits[1].label, "Weekly Opus");
    }

    #[test]
    fn initialize_response_carries_commands() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"i","response":{"models":[],"commands":[{"name":"compact","description":"Clear history but keep a summary","argumentHint":"<instructions>"}]}}}"#;
        let Event::Commands(c) = &parse(line)[1] else { panic!() };
        assert_eq!(c[0], SlashCommand { name: "compact".into(), description: "Clear history but keep a summary".into(), hint: "<instructions>".into() });
    }

    #[test]
    fn ignores_subagent_messages() {
        let line = r#"{"type":"stream_event","parent_tool_use_id":"toolu_9","event":{"type":"message_start"}}"#;
        assert!(parse(line).is_empty());
    }
}
