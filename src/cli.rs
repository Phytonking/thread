//! Headless turns through the vendor CLIs: `claude -p`, `codex exec`, `grok -p`.
//!
//! The CLI owns the agent loop (its own tools, its own context management). We give
//! it the thread's prompt, resume its session on later turns, expose the harness
//! tools over MCP (`thread mcp`), and parse its event stream into our event log.

use crate::llm::{self, Llm};
use crate::provider::Provider;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Perms {
    /// Everything runs without asking.
    Yolo,
    /// Ask the user (claude routes prompts to the `approve` MCP tool; codex and grok
    /// cannot ask headlessly, so they run sandboxed to the working directory).
    Ask { accept_edits: bool },
    /// Read-only.
    Deny,
}

pub struct McpSpec {
    pub exe: PathBuf,
    pub env: Vec<(String, String)>,
    /// The user's servers from `/mcp add`: name -> {command, args, env} or {url}.
    pub extra: Vec<(String, Value)>,
}

pub struct TurnOpts<'a> {
    pub provider: Provider,
    pub workdir: &'a Path,
    /// Resume this CLI session.
    pub resume: Option<&'a str>,
    /// Start a new session with this id (claude, grok).
    pub new_session: Option<&'a str>,
    pub prompt: &'a str,
    /// Appended to the CLI's system prompt (claude). Other CLIs get it in the first prompt.
    pub system_append: Option<&'a str>,
    pub schema: Option<&'a Value>,
    pub model: Option<&'a str>,
    pub mcp: Option<&'a McpSpec>,
    pub perms: Perms,
    /// One-shot, no tools, nothing persisted (memory calls).
    pub ephemeral: bool,
    /// Turn off the CLI's own subagents (main must delegate through `spawn_thread`).
    pub no_subagents: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    Session(String),
    Text(String),
    /// Reasoning the model showed before acting (claude thinking, codex reasoning summaries).
    Thinking(String),
    Tool(String, String),
    ToolResult(bool, String),
}

#[derive(Debug, Default)]
pub struct Turn {
    pub text: String,
    pub session: Option<String>,
    pub structured: Option<Value>,
    pub error: Option<String>,
}

/// Claude Code tools that only read or look things up.
pub const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS", "WebSearch", "WebFetch", "ToolSearch", "TodoWrite",
    "NotebookRead", "ListMcpResourcesTool", "ReadMcpResourceTool"];

fn toml_str(s: &str) -> String { serde_json::to_string(s).unwrap() }

pub fn command(o: &TurnOpts, schema_file: Option<&Path>) -> (Command, bool) {
    let mut stdin_prompt = true;
    let mut c;
    match o.provider {
        Provider::Claude => {
            c = Command::new("claude");
            c.args(["-p", "--output-format", "stream-json", "--verbose"]);
            // Same config the user normally runs (MCP servers, plugins, skills, hooks); override with THREAD_CLAUDE_SETTINGS.
            let sources = std::env::var("THREAD_CLAUDE_SETTINGS").unwrap_or_else(|_| "user,project,local".into());
            c.args(["--setting-sources", &sources]);
            if let Some(r) = o.resume { c.args(["--resume", r]); }
            else if let Some(s) = o.new_session { c.args(["--session-id", s]); }
            if o.ephemeral { c.args(["--no-session-persistence", "--tools", ""]); }
            if let Some(m) = o.model { c.args(["--model", m]); }
            if let Some(s) = o.system_append { c.args(["--append-system-prompt", s]); }
            if let Some(s) = o.schema { c.args(["--json-schema", &s.to_string()]); }
            if let Some(m) = o.mcp {
                let env: serde_json::Map<String, Value> = m.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                let mut servers = serde_json::Map::new();
                for (name, s) in &m.extra {
                    servers.insert(name.clone(), match s["url"].as_str() {
                        Some(url) => json!({"type": "http", "url": url}),
                        None => json!({"command": s["command"], "args": s["args"], "env": s["env"]}),
                    });
                }
                servers.insert("thread".into(), json!({"command": m.exe, "args": ["mcp"], "env": env}));
                let cfg = json!({"mcpServers": servers});
                c.args(["--mcp-config", &cfg.to_string()]);
            }
            match o.perms {
                Perms::Yolo => { c.arg("--dangerously-skip-permissions"); }
                Perms::Ask { accept_edits } => {
                    c.args(["--permission-mode", if accept_edits { "acceptEdits" } else { "default" }]);
                    // Never ask about: reading and searching, the harness's own tools (spawn_thread,
                    // remember, …), servers the user added, and tools they chose `/always` for.
                    let mut allowed: Vec<String> = READ_ONLY_TOOLS.iter().map(|t| t.to_string()).collect();
                    allowed.extend(crate::provider::always_allowed());
                    if let Some(m) = o.mcp {
                        allowed.push("mcp__thread".into());
                        allowed.extend(m.extra.iter().map(|(n, _)| format!("mcp__{n}")));
                    }
                    c.arg(format!("--allowedTools={}", allowed.join(",")));
                    if o.mcp.is_some() {
                        c.args(["--permission-prompt-tool", "mcp__thread__approve"]);
                    }
                    else { c.args(["--permission-prompts", "none"]); }
                }
                Perms::Deny => { c.args(["--permission-mode", "default", "--permission-prompts", "none"]); }
            }
        }
        Provider::Codex => {
            c = Command::new("codex");
            c.arg("exec");
            if let Some(r) = o.resume { c.args(["resume", r]); }
            c.args(["--json", "--skip-git-repo-check"]);
            if o.no_subagents { c.args(["--disable", "multi_agent"]); }
            if o.ephemeral { c.arg("--ephemeral"); }
            if let Some(m) = o.model { c.args(["-m", m]); }
            if let Some(f) = schema_file { c.arg("--output-schema").arg(f); }
            match o.perms {
                Perms::Yolo => { c.arg("--dangerously-bypass-approvals-and-sandbox"); }
                Perms::Ask { .. } => { c.args(["-c", "sandbox_mode=\"workspace-write\"", "-c", "approval_policy=\"never\""]); }
                Perms::Deny => { c.args(["-c", "sandbox_mode=\"read-only\"", "-c", "approval_policy=\"never\""]); }
            }
            if let Some(m) = o.mcp {
                c.args(["-c", &format!("mcp_servers.thread.command={}", toml_str(&m.exe.display().to_string()))]);
                c.args(["-c", "mcp_servers.thread.args=[\"mcp\"]"]);
                // exec can't prompt, and MCP calls needing approval are silently rejected: pre-approve ours.
                c.args(["-c", "mcp_servers.thread.default_tools_approval_mode=\"approve\""]);
                for (k, v) in &m.env { c.args(["-c", &format!("mcp_servers.thread.env.{k}={}", toml_str(v))]); }
                for (name, s) in &m.extra {
                    c.args(["-c", &format!("mcp_servers.{name}.default_tools_approval_mode=\"approve\"")]);
                    if let Some(url) = s["url"].as_str() {
                        c.args(["-c", &format!("mcp_servers.{name}.url={}", toml_str(url))]);
                        continue;
                    }
                    c.args(["-c", &format!("mcp_servers.{name}.command={}", toml_str(s["command"].as_str().unwrap_or("")))]);
                    c.args(["-c", &format!("mcp_servers.{name}.args={}", s["args"])]);
                    for (k, v) in s["env"].as_object().into_iter().flatten() {
                        c.args(["-c", &format!("mcp_servers.{name}.env.{k}={}", toml_str(v.as_str().unwrap_or("")))]);
                    }
                }
            }
            c.arg("-"); // prompt on stdin
        }
        Provider::Grok => {
            c = Command::new("grok");
            stdin_prompt = false;
            c.args(["-p", o.prompt, "--output-format", "json", "--no-auto-update"]);
            c.arg("--cwd").arg(o.workdir);
            if let Some(r) = o.resume { c.args(["-r", r]); }
            else if let Some(s) = o.new_session { c.args(["-s", s]); }
            if let Some(m) = o.model { c.args(["-m", m]); }
            if let Some(s) = o.schema { c.args(["--json-schema", &s.to_string()]); }
            if o.ephemeral { c.args(["--no-memory", "--no-subagents", "--no-plan"]); }
            match o.perms {
                Perms::Yolo => { c.arg("--always-approve"); }
                Perms::Ask { .. } => { c.args(["--sandbox", "workspace"]); }
                Perms::Deny => { c.args(["--sandbox", "read-only"]); }
            }
        }
        Provider::Api => unreachable!("api provider does not use a CLI"),
    }
    c.current_dir(o.workdir);
    if let Some(m) = o.mcp { for (k, v) in &m.env { c.env(k, v); } }
    // own process group: Ctrl-C at the REPL must not kill a thread's turn
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    (c, stdin_prompt)
}

/// Run one headless turn, streaming parsed events to `on_event`.
pub fn run(o: &TurnOpts, on_event: &mut dyn FnMut(&Ev), cancel: &AtomicBool) -> Result<Turn> {
    let schema_file = match (o.provider, o.schema) {
        (Provider::Codex, Some(s)) => {
            let p = std::env::temp_dir().join(format!("thread-schema-{}.json", crate::provider::uuid()));
            std::fs::write(&p, s.to_string())?;
            Some(p)
        }
        _ => None,
    };
    let (mut cmd, stdin_prompt) = command(o, schema_file.as_deref());
    let bin = o.provider.binary().unwrap();
    let mut child = cmd.stdin(if stdin_prompt { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().with_context(|| format!("starting `{bin}` (is it installed and on PATH?)"))?;
    if stdin_prompt {
        let mut sin = child.stdin.take().unwrap();
        let prompt = o.prompt.to_string();
        std::thread::spawn(move || { sin.write_all(prompt.as_bytes()).ok(); });
    }
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || { let mut s = String::new(); stderr.read_to_string(&mut s).ok(); s });
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() { if let Ok(l) = line { if tx.send(l).is_err() { break; } } }
    });

    let mut turn = Turn::default();
    let mut parser = Parser::new(o.provider);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => for ev in parser.line(&line, &mut turn) { on_event(&ev); },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    child.kill().ok();
                    child.wait().ok();
                    bail!("cancelled");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait()?;
    let stderr = err_reader.join().unwrap_or_default();
    parser.finish(&mut turn);
    if let Some(f) = schema_file { std::fs::remove_file(f).ok(); }
    if o.schema.is_some() && turn.structured.is_none() {
        turn.structured = serde_json::from_str(turn.text.trim()).ok();
    }
    if !status.success() && turn.error.is_none() {
        let tail: String = stderr.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        turn.error = Some(format!("{bin} exited with {status}: {}", if tail.is_empty() { "(no stderr)" } else { &tail }));
    }
    Ok(turn)
}

pub struct Parser {
    provider: Provider,
    grok_lines: Vec<String>,
}

impl Parser {
    pub fn new(provider: Provider) -> Self { Self { provider, grok_lines: vec![] } }

    pub fn line(&mut self, line: &str, turn: &mut Turn) -> Vec<Ev> {
        if self.provider == Provider::Grok { self.grok_lines.push(line.to_string()); return vec![]; }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
        let mut evs = vec![];
        match self.provider {
            Provider::Claude => match v["type"].as_str().unwrap_or("") {
                "system" if v["subtype"] == "init" => {
                    if let Some(s) = v["session_id"].as_str() { turn.session = Some(s.into()); evs.push(Ev::Session(s.into())); }
                }
                "assistant" => for b in v["message"]["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => {
                            let t = b["text"].as_str().unwrap_or("").to_string();
                            if !t.trim().is_empty() { turn.text = t.clone(); evs.push(Ev::Text(t)); }
                        }
                        Some("tool_use") => evs.push(Ev::Tool(b["name"].as_str().unwrap_or("?").into(), b["input"].to_string())),
                        Some("thinking") => {
                            let t = b["thinking"].as_str().unwrap_or("");
                            if !t.trim().is_empty() { evs.push(Ev::Thinking(t.into())); }
                        }
                        _ => {}
                    }
                },
                "user" => for b in v["message"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                    let content = match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        other => other.to_string(),
                    };
                    evs.push(Ev::ToolResult(b["is_error"] == true, content));
                },
                "result" => {
                    if let Some(r) = v["result"].as_str() { if !r.is_empty() { turn.text = r.into(); } }
                    if !v["structured_output"].is_null() { turn.structured = Some(v["structured_output"].clone()); }
                    if let Some(s) = v["session_id"].as_str() { turn.session = Some(s.into()); }
                    if v["is_error"] == true {
                        turn.error = Some(v["result"].as_str().filter(|s| !s.is_empty()).map(String::from)
                            .unwrap_or_else(|| format!("claude: {}", v["subtype"].as_str().unwrap_or("error"))));
                    }
                }
                _ => {}
            },
            Provider::Codex => match v["type"].as_str().unwrap_or("") {
                "thread.started" => {
                    if let Some(s) = v["thread_id"].as_str() { turn.session = Some(s.into()); evs.push(Ev::Session(s.into())); }
                }
                "item.completed" => {
                    let it = &v["item"];
                    match it["type"].as_str().unwrap_or("") {
                        "agent_message" => {
                            let t = it["text"].as_str().unwrap_or("").to_string();
                            if !t.trim().is_empty() { turn.text = t.clone(); evs.push(Ev::Text(t)); }
                        }
                        "command_execution" => {
                            evs.push(Ev::Tool("shell".into(), it["command"].as_str().unwrap_or("").into()));
                            let code = it["exit_code"].as_i64().unwrap_or(0);
                            evs.push(Ev::ToolResult(code != 0 || it["status"] == "failed", it["aggregated_output"].as_str().unwrap_or("").into()));
                        }
                        "file_change" => {
                            let paths: Vec<String> = it["changes"].as_array().into_iter().flatten().filter_map(|c| c["path"].as_str().map(String::from)).collect();
                            evs.push(Ev::Tool("edit".into(), paths.join(", ")));
                            evs.push(Ev::ToolResult(it["status"] == "failed", String::new()));
                        }
                        "mcp_tool_call" => {
                            evs.push(Ev::Tool(format!("{}.{}", it["server"].as_str().unwrap_or("mcp"), it["tool"].as_str().unwrap_or("?")), it["arguments"].to_string()));
                            let failed = it["status"] == "failed" || !it["error"].is_null();
                            let out = if !it["error"].is_null() {
                                it["error"]["message"].as_str().map(String::from).unwrap_or_else(|| it["error"].to_string())
                            } else {
                                it["result"]["content"].as_array().map(|a| a.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"))
                                    .unwrap_or_else(|| it["result"].to_string())
                            };
                            evs.push(Ev::ToolResult(failed, out));
                        }
                        "reasoning" => {
                            let t = it["text"].as_str().unwrap_or("");
                            if !t.trim().is_empty() { evs.push(Ev::Thinking(t.into())); }
                        }
                        "web_search" => evs.push(Ev::Tool("web_search".into(), it["query"].as_str().unwrap_or("").into())),
                        "error" => turn.error = Some(it["message"].as_str().unwrap_or("codex error").into()),
                        _ => {}
                    }
                }
                "turn.failed" => turn.error = Some(v["error"]["message"].as_str().unwrap_or("codex turn failed").into()),
                "error" => turn.error = Some(v["message"].as_str().unwrap_or("codex error").into()),
                _ => {}
            },
            _ => {}
        }
        evs
    }

    /// Grok's `--output-format json` prints one object at the end; read it tolerantly.
    pub fn finish(&mut self, turn: &mut Turn) {
        if self.provider != Provider::Grok { return; }
        let obj = self.grok_lines.iter().rev().find_map(|l| serde_json::from_str::<Value>(l).ok().filter(|v| v.is_object()))
            .or_else(|| serde_json::from_str::<Value>(&self.grok_lines.join("\n")).ok());
        let Some(v) = obj else {
            let raw = self.grok_lines.join("\n");
            if !raw.trim().is_empty() { turn.text = raw; }
            return;
        };
        for k in ["result", "text", "response", "output", "content", "message"] {
            if let Some(s) = v[k].as_str() { turn.text = s.into(); break; }
        }
        for k in ["session_id", "sessionId", "session"] {
            if let Some(s) = v[k].as_str() { turn.session = Some(s.into()); break; }
        }
        for k in ["structured_output", "structured", "json"] {
            if v[k].is_object() { turn.structured = Some(v[k].clone()); break; }
        }
        if v["is_error"] == true || v["error"].is_string() {
            turn.error = Some(v["error"].as_str().unwrap_or(&turn.text).to_string());
        }
    }
}

/// An `Llm` backed by a one-shot CLI call, so memory consolidation can run on a subscription.
/// Supports the requests consolidation makes: a system prompt, one user message, and an
/// optional json_schema output format. No tools.
pub struct CliLlm {
    pub provider: Provider,
    pub model: Option<String>,
}

impl Llm for CliLlm {
    fn send(&self, body: Value) -> Result<Value> {
        let system = body["system"].as_str().unwrap_or("");
        let user = body["messages"].as_array().and_then(|m| m.last()).map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            c => llm::text_of(&json!({"content": c})),
        }).unwrap_or_default();
        let schema = body["output_config"]["format"]["schema"].clone();
        let schema = if schema.is_null() { None } else { Some(schema) };
        let prompt = format!("{system}\n\n{user}");
        let dir = std::env::temp_dir();
        let o = TurnOpts {
            provider: self.provider, workdir: &dir, resume: None, new_session: None, prompt: &prompt,
            system_append: None, schema: schema.as_ref(), model: self.model.as_deref(), mcp: None, perms: Perms::Deny, ephemeral: true,
            no_subagents: true,
        };
        let t = run(&o, &mut |_| {}, &AtomicBool::new(false))?;
        if let Some(e) = t.error { return Err(anyhow!("{} memory call failed: {e}", self.provider.name())); }
        let text = match (&schema, t.structured) {
            (Some(_), Some(v)) => v.to_string(),
            _ => t.text,
        };
        Ok(json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": text}], "usage": {}}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(p: Provider, lines: &[Value]) -> (Turn, Vec<Ev>) {
        let mut parser = Parser::new(p);
        let mut t = Turn::default();
        let mut evs = vec![];
        for l in lines { evs.extend(parser.line(&l.to_string(), &mut t)); }
        parser.finish(&mut t);
        (t, evs)
    }

    #[test]
    fn thinking_from_claude_and_codex() {
        let (_, evs) = parse(Provider::Claude, &[json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": "weigh the options"}, {"type": "thinking", "thinking": ""}]}})]);
        assert_eq!(evs, vec![Ev::Thinking("weigh the options".into())]);
        let (_, evs) = parse(Provider::Codex, &[json!({"type": "item.completed", "item": {"id": "i0", "type": "reasoning", "text": "**Planning**\nread the file"}})]);
        assert_eq!(evs, vec![Ev::Thinking("**Planning**\nread the file".into())]);
    }

    #[test]
    fn claude_stream() {
        let (t, evs) = parse(Provider::Claude, &[
            json!({"type": "system", "subtype": "init", "session_id": "s-1"}),
            json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": ""}, {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "a.txt", "is_error": false}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "one file"}]}}),
            json!({"type": "result", "subtype": "success", "result": "one file", "session_id": "s-1", "is_error": false, "structured_output": {"n": 1}}),
        ]);
        assert_eq!(t.session.as_deref(), Some("s-1"));
        assert_eq!(t.text, "one file");
        assert_eq!(t.structured, Some(json!({"n": 1})));
        assert!(t.error.is_none());
        assert_eq!(evs[1], Ev::Tool("Bash".into(), "{\"command\":\"ls\"}".into()));
        assert_eq!(evs[2], Ev::ToolResult(false, "a.txt".into()));
    }

    #[test]
    fn codex_stream_and_failure() {
        let (t, evs) = parse(Provider::Codex, &[
            json!({"type": "thread.started", "thread_id": "th-9"}),
            json!({"type": "item.completed", "item": {"type": "command_execution", "command": "cargo test", "aggregated_output": "1 failed", "exit_code": 101}}),
            json!({"type": "item.completed", "item": {"type": "agent_message", "text": "{\"ok\":false}"}}),
            json!({"type": "turn.completed"}),
        ]);
        assert_eq!(t.session.as_deref(), Some("th-9"));
        assert_eq!(evs[2], Ev::ToolResult(true, "1 failed".into()));
        assert_eq!(t.text, "{\"ok\":false}");
        let (t, _) = parse(Provider::Codex, &[json!({"type": "turn.failed", "error": {"message": "usage limit"}})]);
        assert_eq!(t.error.as_deref(), Some("usage limit"));
    }

    #[test]
    fn grok_final_object() {
        let (t, _) = parse(Provider::Grok, &[json!({"result": "done", "session_id": "g-2"})]);
        assert_eq!((t.text.as_str(), t.session.as_deref()), ("done", Some("g-2")));
    }

    #[test]
    fn commands_carry_resume_perms_and_mcp() {
        let dir = PathBuf::from("/repo");
        let mcp = McpSpec { exe: "/bin/thread".into(), env: vec![("THREAD_ID".into(), "fix".into())],
            extra: vec![("gh".into(), json!({"command": "gh-mcp", "args": ["--ro"], "env": {}})), ("docs".into(), json!({"url": "https://x.dev/mcp"}))] };
        let base = |p, resume, perms| TurnOpts { provider: p, workdir: &dir, resume, new_session: Some("u-1"), prompt: "hi",
            system_append: Some("role"), schema: None, model: None, mcp: Some(&mcp), perms, ephemeral: false, no_subagents: true };
        let args = |o: &TurnOpts| { let (c, _) = command(o, None); c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>() };

        let a = args(&base(Provider::Claude, None, Perms::Ask { accept_edits: true }));
        assert!(a.windows(2).any(|w| w == ["--session-id", "u-1"]));
        assert!(a.windows(2).any(|w| w == ["--permission-prompt-tool", "mcp__thread__approve"]));
        assert!(a.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        let allowed = a.iter().find(|x| x.starts_with("--allowedTools=")).unwrap();
        assert!(allowed.contains("WebSearch") && allowed.ends_with("mcp__thread,mcp__gh,mcp__docs"), "{allowed}");
        let a = args(&base(Provider::Claude, Some("s-1"), Perms::Yolo));
        assert!(a.windows(2).any(|w| w == ["--resume", "s-1"]) && a.contains(&"--dangerously-skip-permissions".to_string()));

        let a = args(&base(Provider::Codex, Some("th-9"), Perms::Ask { accept_edits: true }));
        assert_eq!(&a[..3], ["exec", "resume", "th-9"]);
        assert!(a.contains(&"sandbox_mode=\"workspace-write\"".to_string()));
        assert!(a.contains(&"mcp_servers.thread.default_tools_approval_mode=\"approve\"".to_string()));
        assert!(a.windows(2).any(|w| w == ["--disable", "multi_agent"]));
        assert!(a.contains(&"mcp_servers.thread.env.THREAD_ID=\"fix\"".to_string()));
        assert_eq!(a.last().unwrap(), "-");
        assert!(a.contains(&"mcp_servers.gh.args=[\"--ro\"]".to_string()) && a.contains(&"mcp_servers.docs.url=\"https://x.dev/mcp\"".to_string()));
        let a = args(&base(Provider::Claude, None, Perms::Deny));
        let cfg = &a[a.iter().position(|x| x == "--mcp-config").unwrap() + 1];
        assert!(cfg.contains("\"gh\"") && cfg.contains("\"type\":\"http\"") && cfg.contains("\"thread\""));

        let a = args(&base(Provider::Grok, None, Perms::Deny));
        assert!(a.windows(2).any(|w| w == ["-s", "u-1"]) && a.windows(2).any(|w| w == ["--sandbox", "read-only"]));
    }
}
