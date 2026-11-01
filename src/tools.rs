//! Tools the agent can call.
//! - work: Anthropic-defined `bash` and text editor (the model is trained on them)
//! - memory: `recall`, `remember`, `checkpoint`
//! - threads: main spawns and steers background threads; background threads
//!   can `notify_main` before they finish

use crate::runtime::Runtime;
use crate::{consolidate, memory, threads};
use anyhow::{Result, anyhow, bail};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const MAX_OUTPUT: usize = 30_000;
const BASH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Approval {
    /// Run everything.
    Allow,
    /// Refuse side effects (non-interactive runs without --yolo).
    Deny,
    /// Ask the user through the runtime's approval queue.
    Ask,
}

pub struct Ctx<'a> {
    pub conn: &'a Connection,
    pub run_id: String,
    pub thread_id: String,
    pub workdir: PathBuf,
    /// The thread has its own worktree; edits inside it need no approval.
    pub own_worktree: bool,
    pub approval: Approval,
    pub rt: Option<Arc<Runtime>>,
    pub cancel: Arc<AtomicBool>,
    /// Repo root (where worktrees for new threads are created).
    pub root: PathBuf,
}

pub fn definitions(is_main: bool) -> Value {
    let strict = |name: &str, desc: &str, props: Value, required: &[&str]| json!({
        "name": name, "description": desc, "strict": true,
        "input_schema": {"type": "object", "properties": props, "required": required, "additionalProperties": false},
    });
    let mut tools = vec![
        json!({"type": "bash_20250124", "name": "bash"}),
        json!({"type": "text_editor_20250728", "name": "str_replace_based_edit_tool"}),
        strict("recall", "Search long-term memory for this repo: facts, past runs, skills.",
            json!({"query": {"type": "string"}}), &["query"]),
        strict("remember", "Store a durable fact (entity, attribute, value), e.g. (\"billing-service\", \"database\", \"Postgres 16\"). Starts tentative until another run corroborates it.",
            json!({"entity": {"type": "string"}, "attribute": {"type": "string"}, "value": {"type": "string"}}), &["entity", "attribute", "value"]),
        strict("checkpoint", "Overwrite this thread's carry-forward state: where things stand and what's next.",
            json!({"state": {"type": "string"}}), &["state"]),
        strict("list_threads", "List threads with status, provider/model and state.", json!({}), &[]),
    ];
    if is_main {
        tools.extend([
            strict("spawn_thread", "Start a background thread that works on a task in parallel and reports back when done. Returns the thread's name.",
                json!({
                    "name": {"type": "string", "description": "short kebab-case name the user can type to find and switch to it, e.g. fix-login-redirect"},
                    "task": {"type": "string", "description": "complete, self-contained instructions; the thread cannot see this conversation"},
                    "worktree": {"type": "boolean", "description": "true to give it its own git worktree and branch thread/<name>; use when it will edit code"},
                    "provider": {"type": "string", "enum": ["default", "claude", "codex", "grok", "api"], "description": "which agent runs it; default runs it on your own provider"},
                    "model": {"type": "string", "description": "model for that provider: an id or alias (e.g. opus, sonnet, gpt-5.5) or a unique partial name; \"default\" inherits yours on the same provider, else the provider's default"},
                }), &["name", "task", "worktree", "provider", "model"]),
            strict("set_thread_model", "Move a thread (or yourself, as main) to another provider and/or model, from its next turn. A new provider starts a fresh session there; state and memory carry over.",
                json!({
                    "name": {"type": "string"},
                    "provider": {"type": "string", "enum": ["keep", "claude", "codex", "grok", "api"]},
                    "model": {"type": "string", "description": "an id, alias or unique partial name; \"keep\" leaves it (resets to the provider's default when the provider changes); \"default\" resets"},
                }), &["name", "provider", "model"]),
            strict("message_thread", "Send a follow-up or steering message to a thread. Delivered at its next step; wakes it if idle.",
                json!({"name": {"type": "string"}, "text": {"type": "string"}}), &["name", "text"]),
            strict("read_thread", "See a thread's status, state, and latest output.",
                json!({"name": {"type": "string"}}), &["name"]),
            strict("stop_thread", "Stop a background thread. Its conversation, worktree and branch are kept.",
                json!({"name": {"type": "string"}}), &["name"]),
            strict("remove_thread", "Delete a background thread (any status, e.g. failed or finished ones the user no longer wants listed). Stops it if running and removes its worktree; its branch and what memory learned from it are kept.",
                json!({"name": {"type": "string"}}), &["name"]),
        ]);
    } else {
        tools.push(strict("notify_main", "Send main a message now, before you finish (e.g. a blocking question or an early finding).",
            json!({"text": {"type": "string"}}), &["text"]));
    }
    json!(tools)
}

/// The harness-only tools (no bash/editor), for CLI providers that bring their own.
pub fn harness_definitions(is_main: bool) -> Vec<Value> {
    definitions(is_main).as_array().unwrap().iter().filter(|t| t["type"].is_null()).cloned().collect()
}

/// Returns (output, is_error).
pub fn run(ctx: &Ctx, name: &str, input: &Value) -> (String, bool) {
    let r = match name {
        "bash" => bash(ctx, input),
        "str_replace_based_edit_tool" => editor(ctx, input),
        "recall" => recall(ctx, input),
        "remember" => remember(ctx, input),
        "checkpoint" => checkpoint(ctx, input),
        "list_threads" => list_threads(ctx),
        "spawn_thread" => spawn_thread(ctx, input),
        "message_thread" => message_thread(ctx, input),
        "read_thread" => read_thread(ctx, input),
        "stop_thread" => stop_thread(ctx, input),
        "remove_thread" => remove_thread(ctx, input),
        "set_thread_model" => set_thread_model(ctx, input),
        "notify_main" => notify_main(ctx, input),
        _ => Err(anyhow!("unknown tool {name}")),
    };
    match r {
        Ok(s) => (crate::llm::truncate(&s, MAX_OUTPUT), false),
        Err(e) => (format!("Error: {e:#}"), true),
    }
}

fn s<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| anyhow!("missing string field '{key}'"))
}

fn approve(ctx: &Ctx, what: &str) -> Result<()> {
    match ctx.approval {
        Approval::Allow => Ok(()),
        Approval::Deny => bail!("not permitted in this mode (rerun with --yolo to allow)"),
        Approval::Ask if crate::provider::always_allowed().iter().any(|t| t == crate::provider::tool_of(what)) => Ok(()),
        Approval::Ask => {
            // Queued in the database so the REPL can answer it, whichever process asked.
            let id = threads::request_approval(ctx.conn, &ctx.thread_id, what)?;
            if threads::wait_approval(ctx.conn, id, || ctx.cancel.load(std::sync::atomic::Ordering::Relaxed))? { Ok(()) }
            else { bail!("the user denied this action") }
        }
    }
}

fn approve_write(ctx: &Ctx, path: &Path) -> Result<()> {
    if ctx.own_worktree && path.starts_with(&ctx.workdir) && !path.components().any(|c| c.as_os_str() == "..") {
        return Ok(());
    }
    approve(ctx, &format!("write {}", path.display()))
}

fn bash(ctx: &Ctx, input: &Value) -> Result<String> {
    if input["restart"].as_bool() == Some(true) {
        return Ok("shell restarted (each command runs in a fresh shell in the workdir)".into());
    }
    let cmd = s(input, "command")?;
    if !read_only(cmd) { approve(ctx, &format!("bash `{}`", crate::llm::truncate(cmd, 200)))?; }
    let mut child = Command::new("bash").arg("-c").arg(cmd).current_dir(&ctx.workdir)
        .process_group(0) // Ctrl-C at the REPL must not kill a thread's command
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let to = std::thread::spawn(move || { let mut b = Vec::new(); out.read_to_end(&mut b).ok(); b });
    let te = std::thread::spawn(move || { let mut b = Vec::new(); err.read_to_end(&mut b).ok(); b });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? { break Some(st); }
        if start.elapsed() > BASH_TIMEOUT { child.kill().ok(); child.wait().ok(); break None; }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = String::from_utf8_lossy(&to.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&te.join().unwrap_or_default()).into_owned();
    let mut res = stdout;
    if !stderr.is_empty() { res.push_str(&format!("\n[stderr]\n{stderr}")); }
    match status {
        None => bail!("timed out after {}s\n{res}", BASH_TIMEOUT.as_secs()),
        Some(st) if !st.success() => bail!("exit {}\n{res}", st.code().unwrap_or(-1)),
        Some(_) => Ok(if res.is_empty() { "(no output)".into() } else { res }),
    }
}

/// Commands that only read, so a background thread never stalls on an approval for them:
/// one plain command (no chaining, pipes, redirects or substitution) from a short list.
pub fn read_only(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || cmd.contains(['\n', ';', '&', '|', '>', '<', '`', '$']) { return false; }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    match words[0] {
        "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "grep" | "rg" | "file" | "stat" | "du" | "tree" | "which" | "echo" => true,
        "find" => !words.iter().any(|w| matches!(*w, "-exec" | "-execdir" | "-delete" | "-ok" | "-okdir" | "-fprint" | "-fprintf" | "-fls")),
        "git" => matches!(words.get(1), Some(&("status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" | "blame")))
            && !words.iter().any(|w| w.starts_with("--output")),
        _ => false,
    }
}

fn resolve(ctx: &Ctx, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { ctx.workdir.join(p) }
}

fn editor(ctx: &Ctx, input: &Value) -> Result<String> {
    let path = resolve(ctx, s(input, "path")?);
    match s(input, "command")? {
        "view" => {
            if path.is_dir() {
                let mut out = vec![];
                list_dir(&path, &path, 0, &mut out)?;
                return Ok(out.join("\n"));
            }
            let text = std::fs::read_to_string(&path)?;
            let lines: Vec<&str> = text.lines().collect();
            let (start, end) = match input["view_range"].as_array() {
                Some(r) if r.len() == 2 => {
                    let a = r[0].as_i64().unwrap_or(1).max(1) as usize;
                    let b = r[1].as_i64().unwrap_or(-1);
                    (a, if b < 0 { lines.len() } else { (b as usize).min(lines.len()) })
                }
                _ => (1, lines.len()),
            };
            Ok(lines.iter().enumerate().skip(start - 1).take(end.saturating_sub(start - 1))
                .map(|(i, l)| format!("{:>6}\t{l}", i + 1)).collect::<Vec<_>>().join("\n"))
        }
        "create" => {
            let text = s(input, "file_text")?;
            approve_write(ctx, &path)?;
            if let Some(d) = path.parent() { std::fs::create_dir_all(d)?; }
            std::fs::write(&path, text)?;
            Ok(format!("created {}", path.display()))
        }
        "str_replace" => {
            let old = s(input, "old_str")?;
            let new = input["new_str"].as_str().unwrap_or("");
            let text = std::fs::read_to_string(&path)?;
            match text.matches(old).count() {
                0 => bail!("old_str not found in {}", path.display()),
                1 => {}
                n => bail!("old_str matches {n} times; include more context to make it unique"),
            }
            approve_write(ctx, &path)?;
            std::fs::write(&path, text.replacen(old, new, 1))?;
            Ok(format!("edited {}", path.display()))
        }
        "insert" => {
            let at = input["insert_line"].as_u64().ok_or_else(|| anyhow!("missing insert_line"))? as usize;
            let new = s(input, "insert_text")?;
            let text = std::fs::read_to_string(&path)?;
            let mut lines: Vec<&str> = text.lines().collect();
            if at > lines.len() { bail!("insert_line {at} beyond end of file ({} lines)", lines.len()); }
            approve_write(ctx, &path)?;
            lines.insert(at, new);
            let mut joined = lines.join("\n");
            if text.ends_with('\n') { joined.push('\n'); }
            std::fs::write(&path, joined)?;
            Ok(format!("inserted after line {at} in {}", path.display()))
        }
        other => bail!("unsupported editor command '{other}'"),
    }
}

fn list_dir(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "target" || name == "node_modules" { continue; }
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
        if p.is_dir() {
            out.push(format!("{rel}/"));
            if depth < 1 { list_dir(root, &p, depth + 1, out)?; }
        } else {
            out.push(rel);
        }
    }
    Ok(())
}


fn recall(ctx: &Ctx, input: &Value) -> Result<String> {
    let hits = memory::recall(ctx.conn, s(input, "query")?, &[], 12)?;
    if hits.is_empty() { return Ok("nothing relevant in memory".into()); }
    memory::render_hits(ctx.conn, &hits)
}

fn remember(ctx: &Ctx, input: &Value) -> Result<String> {
    let out = consolidate::learn(ctx.conn, Some(&ctx.run_id), &ctx.thread_id, s(input, "entity")?, s(input, "attribute")?, s(input, "value")?)?;
    Ok(match out {
        memory::FactOutcome::New(_) => "stored (tentative until corroborated)".into(),
        memory::FactOutcome::Corroborated { promoted, .. } => if promoted { "corroborated and promoted to active".into() } else { "already known".into() },
        memory::FactOutcome::Superseded { old_value, .. } => format!("stored; superseded previous value \"{old_value}\" (threads that used it were notified)"),
        memory::FactOutcome::Contested { incumbent, .. } => {
            let f = memory::get_fact(ctx.conn, incumbent)?;
            format!("stored as tentative challenger; contradicts well-supported value \"{}\" — verify before relying on either", f.value)
        }
    })
}

fn checkpoint(ctx: &Ctx, input: &Value) -> Result<String> {
    threads::set_state(ctx.conn, &ctx.thread_id, s(input, "state")?)?;
    Ok("checkpointed".into())
}

fn list_threads(ctx: &Ctx) -> Result<String> {
    let ts = threads::list(ctx.conn, false)?;
    Ok(ts.iter().map(|t| format!("{} [{}] {}{} {} — {}", t.id, t.status, runs_on(ctx, t), t.branch.as_ref().map(|b| format!(" ({b})")).unwrap_or_default(),
        t.title, crate::llm::truncate(t.state.lines().next().unwrap_or(""), 160))).collect::<Vec<_>>().join("\n"))
}

/// The user's default provider (the runtime's, which honours --provider; else the saved default).
fn default_provider(ctx: &Ctx) -> Result<crate::provider::Provider> {
    match &ctx.rt { Some(rt) => Ok(rt.default_provider), None => crate::provider::resolve_default(None) }
}

/// `provider/model` a thread runs on.
fn runs_on(ctx: &Ctx, t: &threads::Thread) -> String {
    let p = if t.provider.is_empty() { default_provider(ctx).map(|p| p.name().to_string()).unwrap_or_else(|_| "default".into()) } else { t.provider.clone() };
    format!("{p}/{}", t.model.as_deref().unwrap_or("default"))
}

fn spawn_thread(ctx: &Ctx, input: &Value) -> Result<String> {
    let provider = match input["provider"].as_str().unwrap_or("default") {
        "default" | "" => String::new(),
        p => crate::provider::Provider::parse(p)?.name().to_string(),
    };
    let (name, task, wt) = (s(input, "name")?, s(input, "task")?, input["worktree"].as_bool().unwrap_or(false));
    // check the provider and model before creating anything
    // "default" runs on the spawning thread's provider (see create_thread)
    let parent = threads::get(ctx.conn, &ctx.thread_id)?;
    let p = match if provider.is_empty() { parent.provider.as_str() } else { provider.as_str() } {
        "" => default_provider(ctx)?,
        name => crate::provider::Provider::parse(name)?,
    };
    if !provider.is_empty() && !p.status().0 { bail!("{}: {}", p.name(), p.status().1); }
    let model = match input["model"].as_str().unwrap_or("default") {
        "default" | "" => None,
        m => Some(crate::provider::resolve_model(p, m)?),
    };
    let t = match &ctx.rt {
        Some(rt) => rt.spawn_thread(ctx.conn, name, task, wt, &ctx.thread_id, &provider, model.as_deref())?,
        None => crate::runtime::create_thread(ctx.conn, &ctx.root, name, task, wt, &ctx.thread_id, &provider, model.as_deref())?,
    };
    Ok(format!("started thread `{}`{}. It will report back here when done.", t.id,
        t.branch.map(|b| format!(" on branch {b} in {}", t.workdir)).unwrap_or_default()))
}

fn message_thread(ctx: &Ctx, input: &Value) -> Result<String> {
    let t = threads::find(ctx.conn, s(input, "name")?)?;
    deliver(ctx, &t.id, s(input, "text")?)?;
    Ok(format!("sent to {} ({})", t.id, t.status))
}

fn read_thread(ctx: &Ctx, input: &Value) -> Result<String> {
    let t = threads::find(ctx.conn, s(input, "name")?)?;
    let mut out = format!("{} [{}] on {}{}\ntask: {}\nstate: {}\n", t.id, t.status, runs_on(ctx, &t),
        t.branch.as_ref().map(|b| format!(" branch {b} at {}", t.workdir)).unwrap_or_default(), t.task,
        if t.state.is_empty() { "(none yet)" } else { &t.state });
    if let Some((_, _, what)) = threads::pending_approvals(ctx.conn)?.into_iter().find(|(_, th, _)| *th == t.id) {
        out.push_str(&format!("waiting for user approval: {what}\n"));
    }
    let recent = threads::recent_output(ctx.conn, &t.id, 3)?;
    if !recent.is_empty() {
        out.push_str("latest output:\n");
        for r in recent { out.push_str(&format!("---\n{}\n", crate::llm::truncate(&r, 1500))); }
    }
    Ok(out)
}

fn stop_thread(ctx: &Ctx, input: &Value) -> Result<String> {
    let t = threads::find(ctx.conn, s(input, "name")?)?;
    if t.id == threads::MAIN { bail!("main cannot be stopped"); }
    match &ctx.rt { Some(rt) => rt.stop(ctx.conn, &t.id)?, None => threads::set_status(ctx.conn, &t.id, "stopped")? }
    Ok(format!("stopped {}", t.id))
}

fn set_thread_model(ctx: &Ctx, input: &Value) -> Result<String> {
    let t = threads::find(ctx.conn, s(input, "name")?)?;
    let provider = match s(input, "provider")? { "keep" | "" => None, p => Some(p) };
    let model = match s(input, "model")? { "keep" | "" => None, m => Some(m) };
    crate::runtime::configure_thread(ctx.conn, default_provider(ctx)?, &t.id, provider, model)
}

fn remove_thread(ctx: &Ctx, input: &Value) -> Result<String> {
    let t = threads::find(ctx.conn, s(input, "name")?)?;
    crate::runtime::delete_thread(ctx.conn, &ctx.root, ctx.rt.as_deref(), &t.id)
}

/// In-process: deliver and wake. From the MCP server: write the inbox; the runtime's supervisor wakes the thread.
fn deliver(ctx: &Ctx, to: &str, text: &str) -> Result<()> {
    match &ctx.rt {
        Some(rt) => rt.deliver(ctx.conn, to, &ctx.thread_id, text),
        None => threads::send(ctx.conn, to, &ctx.thread_id, text),
    }
}

fn notify_main(ctx: &Ctx, input: &Value) -> Result<String> {
    deliver(ctx, threads::MAIN, s(input, "text")?)?;
    Ok("sent to main".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn ctx<'a>(conn: &'a Connection, dir: &Path, approval: Approval) -> Ctx<'a> {
        threads::ensure_main(conn, &dir.display().to_string()).unwrap();
        let run = threads::start_run(conn, "main").unwrap();
        Ctx { conn, run_id: run, thread_id: "main".into(), workdir: dir.to_path_buf(), own_worktree: false,
            approval, rt: None, cancel: Arc::new(AtomicBool::new(false)), root: dir.to_path_buf() }
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("thread-test-{}", db::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn editor_roundtrip_and_bash() {
        let conn = db::open_memory();
        let dir = tmpdir();
        let c = ctx(&conn, &dir, Approval::Allow);
        let (_, e) = run(&c, "str_replace_based_edit_tool", &json!({"command": "create", "path": "a.txt", "file_text": "one\ntwo\n"}));
        assert!(!e);
        let (_, e) = run(&c, "str_replace_based_edit_tool", &json!({"command": "str_replace", "path": "a.txt", "old_str": "two", "new_str": "deux"}));
        assert!(!e);
        let (_, e) = run(&c, "str_replace_based_edit_tool", &json!({"command": "insert", "path": "a.txt", "insert_line": 0, "insert_text": "zero"}));
        assert!(!e);
        let (out, _) = run(&c, "str_replace_based_edit_tool", &json!({"command": "view", "path": "a.txt"}));
        assert!(out.contains("1\tzero") && out.contains("3\tdeux"), "{out}");
        let (out, e) = run(&c, "bash", &json!({"command": "cat a.txt | wc -l"}));
        assert!(!e && out.trim() == "3", "{out}");
        let (out, e) = run(&c, "bash", &json!({"command": "exit 3"}));
        assert!(e && out.contains("exit 3"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deny_mode_blocks_side_effects_but_own_worktree_edits_pass() {
        let conn = db::open_memory();
        let dir = tmpdir();
        let mut c = ctx(&conn, &dir, Approval::Deny);
        let (out, e) = run(&c, "bash", &json!({"command": "touch x"}));
        assert!(e && out.contains("--yolo"));
        let (_, e) = run(&c, "str_replace_based_edit_tool", &json!({"command": "create", "path": "x.txt", "file_text": "x"}));
        assert!(e);
        c.own_worktree = true;
        let (out, e) = run(&c, "str_replace_based_edit_tool", &json!({"command": "create", "path": "x.txt", "file_text": "x"}));
        assert!(!e, "{out}");
        let (_, e) = run(&c, "str_replace_based_edit_tool", &json!({"command": "create", "path": "../escape.txt", "file_text": "x"}));
        assert!(e, "edits outside the worktree still need approval");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_only_commands_skip_approval() {
        for ok in ["pwd", "ls -la src", "git status", "git log --oneline -5", "grep -rn foo src", "find . -name '*.rs'"] {
            assert!(read_only(ok), "{ok}");
        }
        for no in ["rm -rf x", "ls; rm x", "cat a > b", "git push", "git log --output=x", "find . -delete", "echo $(rm x)", "ls && touch y", "touch a", "cat a | sh"] {
            assert!(!read_only(no), "{no}");
        }
    }

    #[test]
    fn main_has_orchestration_tools_background_does_not() {
        let main = definitions(true).to_string();
        let bg = definitions(false).to_string();
        assert!(main.contains("spawn_thread") && !main.contains("notify_main"));
        assert!(bg.contains("notify_main") && !bg.contains("spawn_thread"));
    }
}
