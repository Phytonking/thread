//! `thread mcp`: an MCP stdio server that gives a provider CLI (Claude Code, Codex,
//! Grok Build, fx) the harness tools: memory, checkpoints, and thread orchestration,
//! plus `approve`, Claude Code's permission-prompt tool, routed to the REPL.
//!
//! The CLI launches it with THREAD_ID / THREAD_DB / THREAD_ROOT in the environment.
//! It only writes the database; the REPL's supervisor notices new mail, stopped
//! threads, and approval requests.

use crate::tools::{self, Approval, Ctx};
use crate::{db, threads};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub fn serve() -> Result<()> {
    let thread_id = std::env::var("THREAD_ID")
        .context("THREAD_ID not set (thread mcp is started by the harness)")?;
    let db_path = PathBuf::from(std::env::var("THREAD_DB").context("THREAD_DB not set")?);
    let root = PathBuf::from(std::env::var("THREAD_ROOT").unwrap_or_else(|_| ".".into()));
    let interactive = std::env::var("THREAD_INTERACTIVE").is_ok_and(|v| v == "1");
    let conn = db::open(&db_path)?;
    let thread = threads::get(&conn, &thread_id)?;
    let is_main = thread.id == threads::MAIN;

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        }; // notification
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "thread", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": list(is_main)})),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"]["arguments"].clone();
                let (text, is_error) = if name == "approve" {
                    (approve(&conn, &thread_id, interactive, &args), false)
                } else {
                    let ctx = Ctx {
                        conn: &conn,
                        run_id: threads::live_run(&conn, &thread_id)?.unwrap_or_default(),
                        thread_id: thread_id.clone(),
                        workdir: thread.workdir.clone().into(),
                        own_worktree: thread.branch.is_some(),
                        approval: Approval::Allow, // harness tools have no side effects outside the db
                        rt: None,
                        cancel: Arc::new(AtomicBool::new(false)),
                        root: root.clone(),
                    };
                    tools::run(&ctx, name, &args)
                };
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            m => Err(json!({"code": -32601, "message": format!("method not found: {m}")})),
        };
        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
    Ok(())
}

fn list(is_main: bool) -> Vec<Value> {
    let mut v: Vec<Value> = tools::harness_definitions(is_main).into_iter()
        .map(|t| json!({"name": t["name"], "description": t["description"], "inputSchema": t["input_schema"]}))
        .collect();
    v.push(json!({
        "name": "approve",
        "description": "Permission prompt handler used by the harness. Do not call directly.",
        "inputSchema": {"type": "object", "properties": {"tool_name": {"type": "string"}, "input": {"type": "object"}}, "required": ["tool_name"]},
    }));
    v
}

/// Claude Code permission-prompt contract: reply {"behavior":"allow","updatedInput":…} or {"behavior":"deny","message":…}.
fn approve(
    conn: &rusqlite::Connection,
    thread_id: &str,
    interactive: bool,
    args: &Value,
) -> String {
    let tool = args["tool_name"].as_str().unwrap_or("a tool");
    let input = args["input"].clone();
    // our own tools are pre-allowed (--allowedTools); this catches any that still get routed here
    if tool.starts_with("mcp__thread__")
        || crate::cli::READ_ONLY_TOOLS.contains(&tool)
        || crate::provider::always_allowed().iter().any(|t| t == tool)
    {
        return json!({"behavior": "allow", "updatedInput": input}).to_string();
    }
    let what = describe(tool, &input);
    let allowed = interactive
        && threads::request_approval(conn, thread_id, &what)
            .and_then(|id| {
                threads::wait_approval(conn, id, || {
                    threads::get(conn, thread_id)
                        .map(|t| t.status == "stopped")
                        .unwrap_or(true)
                })
            })
            .unwrap_or(false);
    if allowed {
        json!({"behavior": "allow", "updatedInput": input}).to_string()
    } else {
        json!({"behavior": "deny", "message": if interactive { "The user denied this action." } else { "Not permitted in non-interactive mode (rerun with --yolo)." }}).to_string()
    }
}

fn describe(tool: &str, input: &Value) -> String {
    let detail = ["command", "file_path", "path", "url", "pattern"]
        .iter()
        .find_map(|k| input[*k].as_str());
    match detail {
        Some(d) => format!("{tool} `{}`", crate::llm::truncate(d, 200)),
        None => format!("{tool} {}", crate::llm::truncate(&input.to_string(), 200)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_harness_tools_and_approve() {
        let names: Vec<String> = list(true)
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"spawn_thread".into())
                && names.contains(&"approve".into())
                && names.contains(&"remember".into())
        );
        assert!(
            !names.contains(&"bash".into()),
            "CLIs bring their own shell"
        );
        assert_eq!(
            describe("Bash", &json!({"command": "rm -rf build"})),
            "Bash `rm -rf build`"
        );
    }
}
