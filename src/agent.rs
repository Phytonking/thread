//! The conversation engine every thread runs on.
//!
//! Input arrives through the thread's inbox (the user's typed messages, follow-ups
//! from main, reports from background threads, memory notices). `step` turns the
//! pending mail into a user message and runs the tool loop until the thread is
//! idle. Mail that arrives mid-loop is injected at the next tool boundary, so you
//! can steer a running thread.
//!
//! Consolidation points:
//! - **nap**: when the prompt passes a threshold (even mid tool loop), the model
//!   distills the conversation; facts are learned, the thread state is
//!   checkpointed, and the conversation restarts from the distillate. History is
//!   never edited in place.
//! - **handoff**: when a run ends, the model writes state, outcome and facts, and
//!   the run is sealed for `consolidate`.

use crate::cli::{self, Ev, McpSpec, Perms, TurnOpts};
use crate::consolidate;
use crate::llm::{self, Llm};
use crate::provider::Provider;
use crate::runtime::Runtime;
use crate::tools::{self, Approval, Ctx};
use crate::{memory, threads};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub enum Echo<'a> {
    Text(&'a str),
    Thinking(&'a str),
    Tool(&'a str, &'a str),
    ToolErr(&'a str),
    Info(&'a str),
}

pub enum Step {
    Idle(String),
    Stopped,
}

pub struct Handoff {
    pub outcome: Option<String>,
    pub summary: String,
    /// Set when the model's handoff failed and the run was closed with a fallback summary.
    pub error: Option<String>,
}

fn main_system(root: &str) -> String {
    format!("You are `main`, the primary thread of the `thread` agent harness for the repository at {root}. The user talks to you here. \
You can do work yourself with bash and the editor, and you can run work in background threads that proceed in parallel while you and the user keep talking.

When to spawn a background thread (`spawn_thread`): self-contained work that takes more than a few steps and does not need the user turn by turn: implementing a feature, chasing a bug, running and fixing a test suite, an investigation or review. \
Give it a short kebab-case name the user can recognize and type (e.g. fix-login-redirect), and a complete task description with everything it needs, since it cannot see this conversation. \
Use `worktree: true` when it will edit code, so it works on its own branch (thread/<name>) without colliding with other threads. \
Each thread can run on its own provider (claude, codex, grok, api, or a local model: ollama, fm, local) and model: honour what the user asks for (\"have codex do it\", \"use opus for the review\"), otherwise use `default` for both. `set_thread_model` moves an existing thread. \
Do small things (a quick question, a one-line fix, reading a file) yourself. \
Background threads exist only through `spawn_thread`: never substitute another subagent, task or parallel-agent tool for it. If `spawn_thread` fails, tell the user the exact error instead of working around it.

Background threads report back when they finish; reports and other messages arrive tagged like [from fix-login-redirect]. When one arrives, tell the user what happened in a sentence or two and what decision, if any, is theirs. \
Follow up with `message_thread`, check progress with `read_thread` and `list_threads`, stop work that is no longer wanted with `stop_thread`, and delete threads the user wants gone (including failed ones) with `remove_thread`. Do not poll in a loop; reports will arrive on their own.

Memory: call `remember` for facts that should still be true next week (a decision, a config value, a user preference, an environment quirk), not for progress. \
Facts or skills marked tentative are unverified hints. Be concise.")
}

fn background_system(root: &str, name: &str, workdir: &str, branch: Option<&str>) -> String {
    let place = match branch {
        Some(b) => format!(
            "You work in your own git worktree at {workdir} on branch {b}; commit your work there when it is in a good state. Do not touch the main checkout at {root}."
        ),
        None => format!(
            "You work in the repository at {workdir}, which other threads may also be using; avoid sweeping changes outside your task."
        ),
    };
    format!("You are `{name}`, a background thread of the `thread` agent harness, spawned to carry out one task. {place}

Work autonomously; nobody is watching turn by turn. Messages from main or the user may arrive mid-work tagged like [from main] or [from user]; take them into account. \
If you hit a decision you should not make alone, stop and say so in your final message. Use `notify_main` only for something main needs before you finish. \
Use `checkpoint` at milestones. Call `remember` for durable facts learned along the way (not progress).

Your final message is your report to main: what you did, the result (tests, findings), where it lives (branch, files), and anything needing a decision. Keep it tight.")
}

const NAP_PROMPT: &str = "[harness] The context is long, so this conversation will restart from a distillate. Write it now. \
`summary`: everything needed to continue seamlessly: the task, decisions and why, what was tried and failed, current status, the latest request and what you were about to do. \
`state`: a short carry-forward for the thread. `facts`: durable facts learned (not progress).";

const HANDOFF_BG: &str = "[harness] This run is ending. Write the handoff. `state`: where things stand and the exact next step. \
`outcome`: success if the task was accomplished, partial if progress was made, failure if not. `task_type`: a short kebab-case label for the kind of task (e.g. fix-flaky-test, add-endpoint). \
`summary`: one or two sentences. `facts`: durable facts learned that are not already stored (not progress).";

const HANDOFF_MAIN: &str = "[harness] The user is stepping away. `state`: a short note on where the conversation stands and open items. `facts`: durable facts learned that are not already stored.";

pub fn fact_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "required": ["entity", "attribute", "value"],
        "properties": {"entity": {"type": "string"}, "attribute": {"type": "string"}, "value": {"type": "string"}}})
}

fn nap_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "required": ["summary", "state", "facts"], "properties": {
        "summary": {"type": "string"}, "state": {"type": "string"}, "facts": {"type": "array", "items": fact_schema()}}})
}

fn handoff_schema(main: bool) -> Value {
    if main {
        return json!({"type": "object", "additionalProperties": false, "required": ["state", "facts"], "properties": {
            "state": {"type": "string"}, "facts": {"type": "array", "items": fact_schema()}}});
    }
    json!({"type": "object", "additionalProperties": false, "required": ["state", "outcome", "task_type", "summary", "facts"], "properties": {
        "state": {"type": "string"},
        "outcome": {"type": "string", "enum": ["success", "partial", "failure"]},
        "task_type": {"type": "string"},
        "summary": {"type": "string"},
        "facts": {"type": "array", "items": fact_schema()},
    }})
}

pub struct Agent<'a> {
    conn: &'a Connection,
    llm: &'a dyn Llm,
    pub thread: threads::Thread,
    root: String,
    messages: Vec<Value>,
    prefix: String,
    pub run_id: Option<String>,
    fresh: bool,
    pub nap_threshold: u64,
    skills_offered: Vec<i64>,
    pub approval: Approval,
    rt: Option<Arc<Runtime>>,
    cancel: Arc<AtomicBool>,
    echo: Box<dyn Fn(Echo) + 'a>,
    /// `Api` runs the built-in tool loop; the others drive a vendor CLI headlessly.
    pub provider: Provider,
    pub model: Option<String>,
    pub mcp: Option<McpSpec>,
}

impl<'a> Agent<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        conn: &'a Connection,
        llm: &'a dyn Llm,
        thread_id: &str,
        root: &str,
        approval: Approval,
        rt: Option<Arc<Runtime>>,
        cancel: Arc<AtomicBool>,
        echo: Box<dyn Fn(Echo) + 'a>,
    ) -> Result<Self> {
        let thread = threads::get(conn, thread_id)?;
        let (mut messages, prefix) = threads::load_messages(conn, thread_id)?;
        // A crash between a tool call and its result leaves a dangling tool_use; close it.
        if let Some(last) = messages.last()
            && last["role"] == "assistant"
        {
            let dangling: Vec<Value> = last["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use")
                    .map(|b| json!({"type": "tool_result", "tool_use_id": b["id"], "is_error": true,
                        "content": "interrupted before this ran (the harness restarted); re-run it if still needed"}))
                    .collect();
            if !dangling.is_empty() {
                messages.push(json!({"role": "user", "content": dangling}));
            }
        }
        let run_id = threads::live_run(conn, thread_id)?;
        Ok(Self {
            conn,
            llm,
            thread,
            root: root.to_string(),
            messages,
            prefix,
            run_id,
            fresh: true,
            nap_threshold: 120_000,
            skills_offered: vec![],
            approval,
            rt,
            cancel,
            echo,
            provider: Provider::Api,
            model: None,
            mcp: None,
        })
    }

    pub fn with_cli(
        mut self,
        provider: Provider,
        model: Option<String>,
        mcp: Option<McpSpec>,
    ) -> Self {
        self.provider = provider;
        self.model = model;
        self.mcp = mcp;
        self
    }

    /// A local model in the built-in loop: its default model, and naps early enough that
    /// the distillation itself still fits a small context window.
    pub fn with_local(mut self, provider: Provider, model: Option<String>) -> Self {
        self.provider = provider;
        self.model = model;
        if let Some(window) = provider.context_window() {
            self.nap_threshold = self.nap_threshold.min(window * 6 / 10);
        }
        self
    }

    fn is_cli(&self) -> bool {
        !self.provider.in_process()
    }

    /// Every request (turns, naps, handoffs) runs on the thread's model: a side request on
    /// another model would make a local server load a second one.
    fn with_model(&self, mut body: Value) -> Value {
        if let Some(m) = self.current_model() {
            body["model"] = json!(m);
        }
        body
    }

    /// The thread's /model override (re-read each turn so changes apply live), else the default.
    fn current_model(&self) -> Option<String> {
        threads::get(self.conn, &self.thread.id)
            .ok()
            .and_then(|t| t.model)
            .or_else(|| self.model.clone())
    }

    fn is_main(&self) -> bool {
        self.thread.id == threads::MAIN
    }

    fn system(&self) -> String {
        if self.is_main() {
            main_system(&self.root)
        } else {
            background_system(
                &self.root,
                &self.thread.id,
                &self.thread.workdir,
                self.thread.branch.as_deref(),
            )
        }
    }

    pub fn has_pending(&self) -> bool {
        // For CLI threads, `prefix` holds a prompt that was in flight when the process died.
        if self.is_cli() {
            return !self.prefix.is_empty();
        }
        self.messages.last().is_some_and(|m| m["role"] == "user")
    }

    fn save(&self) -> Result<()> {
        threads::save_messages(self.conn, &self.thread.id, &self.messages, &self.prefix)
    }

    fn ensure_run(&mut self) -> Result<String> {
        if self.run_id.is_none() {
            self.run_id = Some(threads::start_run(self.conn, &self.thread.id)?);
        }
        Ok(self.run_id.clone().unwrap())
    }

    /// Where this thread stands, and for main, what the other threads are doing.
    fn situation(&self) -> Result<String> {
        let t = threads::get(self.conn, &self.thread.id)?;
        let mut b = String::new();
        if self.is_main() {
            b.push_str(&format!("<repo path=\"{}\">\n", self.root));
            if !t.state.is_empty() {
                b.push_str(&format!("Last time: {}\n", t.state));
            }
            let others: Vec<_> = threads::list(self.conn, false)?
                .into_iter()
                .filter(|o| o.id != threads::MAIN)
                .collect();
            if !others.is_empty() {
                b.push_str("Background threads:\n");
                for o in others {
                    b.push_str(&format!(
                        "- {} [{}] {}{}\n",
                        o.id,
                        o.status,
                        o.title,
                        if o.state.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", llm::truncate(&o.state, 200))
                        }
                    ));
                }
            }
            b.push_str("</repo>\n");
        } else {
            b.push_str(&format!("<thread name=\"{}\">\nTask: {}\n", t.id, t.task));
            if !t.state.is_empty() {
                b.push_str(&format!("Where things stand: {}\n", t.state));
            }
            b.push_str("</thread>\n");
        }
        Ok(b)
    }

    fn memory_block(&mut self, query: &str) -> Result<String> {
        let mut b = String::new();
        let hits = memory::recall(self.conn, query, &["fact", "episode"], 8)?;
        if !hits.is_empty() {
            b.push_str("<memory>\n");
            b.push_str(&memory::render_hits(self.conn, &hits)?);
            b.push_str("</memory>\n");
        }
        let skills = memory::recall(self.conn, query, &["skill"], 2)?;
        if !skills.is_empty() {
            b.push_str("<skills>\n");
            for h in skills {
                let s = memory::get_skill(self.conn, h.r#ref.parse()?)?;
                let tag = if s.status == "promoted" {
                    "proven"
                } else {
                    "tentative — a suggestion, verify as you go"
                };
                b.push_str(&format!(
                    "## {} (v{}, {tag})\nUse when: {}\n{}\n",
                    s.name, s.version, s.when_to_use, s.steps
                ));
                if !self.skills_offered.contains(&s.id) {
                    self.skills_offered.push(s.id);
                }
            }
            b.push_str("</skills>\n");
        }
        Ok(b)
    }

    fn format_mail(mail: &[(String, String)]) -> String {
        mail.iter()
            .map(|(from, body)| {
                if from == "user" {
                    body.clone()
                } else {
                    format!("[from {from}] {body}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Turn pending mail into the next user message. None if there is no mail.
    fn compose(&mut self) -> Result<Option<String>> {
        let mail = threads::take_mail(self.conn, &self.thread.id)?;
        if mail.is_empty() {
            return Ok(None);
        }
        let run = self.ensure_run()?;
        for (from, body) in &mail {
            threads::log_event(
                self.conn,
                &run,
                if from == "user" { "user" } else { "mail" },
                Some(from),
                false,
                body,
            )?;
        }
        let text = Self::format_mail(&mail);
        let mut content = String::new();
        if self.fresh {
            content.push_str(&self.situation()?);
            let query = format!("{} {} {text}", self.thread.task, self.thread.state);
            content.push_str(&self.memory_block(&query)?);
            if !self.skills_offered.is_empty() {
                threads::record_skills_offered(self.conn, &run, &self.skills_offered)?;
            }
            self.fresh = false;
        }
        content.push_str(&std::mem::take(&mut self.prefix));
        content.push_str(&text);
        Ok(Some(content))
    }

    fn ctx(&self, run_id: &str) -> Ctx<'_> {
        Ctx {
            conn: self.conn,
            run_id: run_id.to_string(),
            thread_id: self.thread.id.clone(),
            workdir: self.thread.workdir.clone().into(),
            own_worktree: self.thread.branch.is_some(),
            approval: self.approval,
            rt: self.rt.clone(),
            cancel: self.cancel.clone(),
            root: self.root.clone().into(),
        }
    }

    /// Process mail until idle.
    pub fn step(&mut self) -> Result<Step> {
        if self.is_cli() {
            return self.step_cli();
        }
        if let Some(content) = self.compose()? {
            self.messages
                .push(json!({"role": "user", "content": content}));
            self.save()?;
        } else if !self.has_pending() {
            return Ok(Step::Idle(String::new()));
        }
        let run = self.ensure_run()?;
        let defs = tools::definitions(self.is_main());
        let mut last_text = String::new();
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Ok(Step::Stopped);
            }
            let body = self.with_model(
                json!({"system": self.system(), "tools": defs, "messages": self.messages}),
            );
            let resp = self.llm.send(body)?;
            let ctx_tokens = llm::context_tokens(&resp);
            let blocks = resp["content"].clone();
            // Append the content unchanged (thinking blocks included): history is append-only.
            self.messages
                .push(json!({"role": "assistant", "content": blocks}));
            self.save()?;
            for t in blocks
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "thinking")
                .filter_map(|b| b["thinking"].as_str())
            {
                if t.trim().is_empty() {
                    continue;
                }
                (self.echo)(Echo::Thinking(t));
                threads::log_event(self.conn, &run, "thinking", None, false, t)?;
            }
            let text = llm::text_of(&resp);
            if !text.is_empty() {
                (self.echo)(Echo::Text(&text));
                threads::log_event(self.conn, &run, "assistant", None, false, &text)?;
                last_text = text;
            }
            match resp["stop_reason"].as_str().unwrap_or("") {
                "tool_use" => {
                    let mut results = vec![];
                    for b in blocks
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "tool_use")
                    {
                        let name = b["name"].as_str().unwrap_or("");
                        let input = &b["input"];
                        (self.echo)(Echo::Tool(name, &llm::truncate(&input.to_string(), 160)));
                        threads::log_event(
                            self.conn,
                            &run,
                            "tool_call",
                            Some(name),
                            false,
                            &input.to_string(),
                        )?;
                        let (out, is_error) = tools::run(&self.ctx(&run), name, input);
                        if is_error {
                            (self.echo)(Echo::ToolErr(&llm::truncate(&out, 300)));
                        }
                        threads::log_event(
                            self.conn,
                            &run,
                            "tool_result",
                            Some(name),
                            is_error,
                            &llm::truncate(&out, 4000),
                        )?;
                        results.push(json!({"type": "tool_result", "tool_use_id": b["id"], "content": out, "is_error": is_error}));
                    }
                    // Steering: mail that arrived during the tool calls rides along with the results.
                    let mail = threads::take_mail(self.conn, &self.thread.id)?;
                    if !mail.is_empty() {
                        for (from, body) in &mail {
                            threads::log_event(
                                self.conn,
                                &run,
                                if from == "user" { "user" } else { "mail" },
                                Some(from),
                                false,
                                body,
                            )?;
                        }
                        results.push(json!({"type": "text", "text": Self::format_mail(&mail)}));
                    }
                    self.messages
                        .push(json!({"role": "user", "content": results}));
                    self.save()?;
                    if ctx_tokens > self.nap_threshold {
                        self.nap(true)?;
                    }
                }
                "refusal" => {
                    (self.echo)(Echo::Info("the model declined this request"));
                    break;
                }
                "max_tokens" => {
                    (self.echo)(Echo::Info("response hit max_tokens"));
                    break;
                }
                _ => {
                    if ctx_tokens > self.nap_threshold {
                        self.nap(false)?;
                    }
                    // Mail that arrived while the model was answering starts the next turn.
                    match self.compose()? {
                        Some(content) => {
                            self.messages
                                .push(json!({"role": "user", "content": content}));
                            self.save()?;
                        }
                        None => break,
                    }
                }
            }
        }
        Ok(Step::Idle(last_text))
    }

    /// In-session consolidation: distill, learn, checkpoint, restart the conversation.
    pub fn nap(&mut self, mid_loop: bool) -> Result<()> {
        if self.messages.is_empty() {
            return Ok(());
        }
        (self.echo)(Echo::Info("napping: consolidating this conversation"));
        let mut msgs = self.messages.clone();
        let ask = json!({"type": "text", "text": NAP_PROMPT});
        match msgs.last_mut() {
            Some(m) if m["role"] == "user" && m["content"].is_array() => {
                m["content"].as_array_mut().unwrap().push(ask)
            }
            _ => msgs.push(json!({"role": "user", "content": [ask]})),
        }
        let resp = self.llm.send(self.with_model(json!({"system": self.system(), "tools": tools::definitions(self.is_main()), "messages": msgs,
            "output_config": {"format": {"type": "json_schema", "schema": nap_schema()}}})))?;
        let v = llm::parse_structured(&resp)?;
        let run = self.ensure_run()?;
        consolidate::learn_all(self.conn, Some(&run), &self.thread.id, &v["facts"])?;
        threads::set_state(
            self.conn,
            &self.thread.id,
            v["state"].as_str().unwrap_or(""),
        )?;
        let summary = v["summary"].as_str().unwrap_or("");
        threads::log_event(self.conn, &run, "nap", None, false, summary)?;
        self.messages.clear();
        let distillate = format!(
            "{}<earlier_in_this_conversation>\n{summary}\n</earlier_in_this_conversation>\n\n",
            self.situation()?
        );
        if mid_loop {
            self.messages.push(json!({"role": "user", "content": format!("{distillate}Continue where you left off.")}));
        } else {
            self.prefix = distillate;
        }
        self.save()
    }

    /// End the current run: handoff, learn, seal. None if there was no run.
    ///
    /// The run always ends: if the model's handoff fails twice (a request error, tool calls or
    /// non-JSON instead of the summary), the run is sealed with its last output as the summary and
    /// the failure is reported in `Handoff::error`. A run left live would never settle, and
    /// shutdown would wait on it forever.
    pub fn handoff(&mut self) -> Result<Option<Handoff>> {
        let Some(run) = self.run_id.clone() else {
            return Ok(None);
        };
        let mut err = match self.try_handoff(&run) {
            Ok(h) => return Ok(h),
            Err(e) => e,
        };
        if self.run_id.as_deref() != Some(run.as_str()) {
            return Err(err); // sealed already; the failure came after
        }
        if !self.cancel.load(Ordering::Relaxed) {
            (self.echo)(Echo::Info("handoff failed; retrying once"));
            match self.try_handoff(&run) {
                Ok(h) => return Ok(h),
                Err(e) if self.run_id.as_deref() != Some(run.as_str()) => return Err(e),
                Err(e) => err = e,
            }
        }
        self.seal_fallback(&run, &format!("{err:#}")).map(Some)
    }

    /// Close a run whose handoff failed, keeping its last output as the summary.
    fn seal_fallback(&mut self, run: &str, error: &str) -> Result<Handoff> {
        let last = threads::last_assistant_text(self.conn, run)?;
        let summary = if last.trim().is_empty() {
            "(no summary: the handoff failed)".to_string()
        } else {
            llm::truncate(last.trim(), 1000)
        };
        let task_type = if self.is_main() { "main" } else { "general" };
        threads::log_event(self.conn, run, "handoff_failed", None, true, error)?;
        threads::seal_run(self.conn, run, None, task_type, &summary)?;
        self.skills_offered.clear();
        self.run_id = None;
        self.fresh = true;
        Ok(Handoff {
            outcome: None,
            summary,
            error: Some(error.to_string()),
        })
    }

    fn try_handoff(&mut self, run: &str) -> Result<Option<Handoff>> {
        let run = run.to_string();
        if self.is_cli() {
            return self.handoff_cli(&run);
        }
        if self.messages.is_empty() && self.prefix.is_empty() {
            threads::seal_run(
                self.conn,
                &run,
                None,
                if self.is_main() { "main" } else { "general" },
                "",
            )?;
            self.run_id = None;
            return Ok(None);
        }
        let main = self.is_main();
        let mut msgs = self.messages.clone();
        let ask = if main { HANDOFF_MAIN } else { HANDOFF_BG };
        match msgs.last_mut() {
            Some(m) if m["role"] == "user" => {
                let prev = m["content"].clone();
                let mut blocks = if prev.is_array() {
                    prev.as_array().unwrap().clone()
                } else {
                    vec![json!({"type": "text", "text": prev})]
                };
                blocks.push(json!({"type": "text", "text": ask}));
                m["content"] = json!(blocks);
            }
            _ => msgs.push(json!({"role": "user", "content": format!("{}{ask}", self.prefix)})),
        }
        // A side request: the handoff exchange is not appended to the thread's conversation.
        let resp = self.llm.send(self.with_model(
            json!({"system": self.system(), "tools": tools::definitions(main), "messages": msgs,
            "output_config": {"format": {"type": "json_schema", "schema": handoff_schema(main)}}}),
        ))?;
        let v = llm::parse_structured(&resp)?;
        self.finish_handoff(&run, v)
    }

    fn finish_handoff(&mut self, run: &str, v: Value) -> Result<Option<Handoff>> {
        let main = self.is_main();
        let run = run.to_string();
        consolidate::learn_all(self.conn, Some(&run), &self.thread.id, &v["facts"])?;
        let state = v["state"].as_str().unwrap_or("");
        threads::set_state(self.conn, &self.thread.id, state)?;
        let outcome = v["outcome"].as_str().map(String::from);
        let summary = v["summary"].as_str().unwrap_or(state).to_string();
        threads::seal_run(
            self.conn,
            &run,
            outcome.as_deref(),
            if main {
                "main"
            } else {
                v["task_type"].as_str().unwrap_or("general")
            },
            &summary,
        )?;
        if let Some(o) = outcome.as_deref().filter(|o| *o != "partial") {
            for sid in &self.skills_offered {
                memory::record_skill_outcome(self.conn, *sid, o == "success")?;
            }
        }
        self.skills_offered.clear();
        self.run_id = None;
        self.fresh = true; // the next run re-reads memory
        Ok(Some(Handoff {
            outcome,
            summary,
            error: None,
        }))
    }

    fn perms(&self) -> Perms {
        match self.approval {
            Approval::Allow => Perms::Yolo,
            Approval::Deny => Perms::Deny,
            Approval::Ask => Perms::Ask {
                accept_edits: self.thread.branch.is_some(),
            },
        }
    }

    /// One headless CLI turn on this thread's session, logging and echoing its events.
    fn cli_turn(&mut self, run: &str, prompt: &str, schema: Option<&Value>) -> Result<cli::Turn> {
        let first = self.thread.session.is_none();
        let new_session = (first && matches!(self.provider, Provider::Claude | Provider::Grok))
            .then(crate::provider::uuid);
        let system = self.system();
        let system_append = (self.provider == Provider::Claude).then_some(system.as_str());
        let prompt = if first && self.provider != Provider::Claude {
            format!("<harness_instructions>\n{system}\n</harness_instructions>\n\n{prompt}")
        } else {
            prompt.to_string()
        };
        let workdir = std::path::PathBuf::from(&self.thread.workdir);
        let model = self.current_model();
        let opts = TurnOpts {
            provider: self.provider,
            workdir: &workdir,
            resume: self.thread.session.as_deref(),
            new_session: new_session.as_deref(),
            prompt: &prompt,
            system_append,
            schema,
            model: model.as_deref(),
            mcp: self.mcp.as_ref(),
            perms: self.perms(),
            ephemeral: false,
            no_subagents: self.is_main(),
        };
        let (conn, echo) = (self.conn, &self.echo);
        let mut log_err = None;
        let mut on_event = |ev: &Ev| {
            let r = match ev {
                Ev::Text(t) if schema.is_none() => {
                    echo(Echo::Text(t));
                    threads::log_event(conn, run, "assistant", None, false, t)
                }
                Ev::Thinking(t) if schema.is_none() => {
                    echo(Echo::Thinking(t));
                    threads::log_event(conn, run, "thinking", None, false, t)
                }
                Ev::Tool(name, input) => {
                    if schema.is_none() {
                        echo(Echo::Tool(name, &llm::truncate(input, 160)));
                    }
                    threads::log_event(conn, run, "tool_call", Some(name), false, input)
                }
                Ev::ToolResult(err, out) => {
                    if *err && schema.is_none() {
                        echo(Echo::ToolErr(&llm::truncate(out, 300)));
                    }
                    threads::log_event(
                        conn,
                        run,
                        "tool_result",
                        None,
                        *err,
                        &llm::truncate(out, 4000),
                    )
                }
                _ => Ok(()),
            };
            if let Err(e) = r {
                log_err = Some(e);
            }
        };
        let turn = cli::run(&opts, &mut on_event, &self.cancel)?;
        if let Some(e) = log_err {
            return Err(e);
        }
        if let Some(s) = turn.session.clone().or(new_session)
            && self.thread.session.as_deref() != Some(s.as_str())
        {
            threads::set_session(self.conn, &self.thread.id, &s)?;
            self.thread.session = Some(s);
        }
        if let Some(e) = &turn.error {
            anyhow::bail!("{}: {e}", self.provider.name());
        }
        Ok(turn)
    }

    fn step_cli(&mut self) -> Result<Step> {
        let mut last = String::new();
        loop {
            let prompt = if !self.prefix.is_empty() {
                self.prefix.clone()
            } else {
                match self.compose()? {
                    Some(p) => p,
                    None => break,
                }
            };
            // Persist the prompt until the turn completes, so a crash re-sends it.
            self.prefix = prompt.clone();
            self.save()?;
            let run = self.ensure_run()?;
            let turn = match self.cli_turn(&run, &prompt, None) {
                Err(e) if self.cancel.load(Ordering::Relaxed) => {
                    let _ = e;
                    return Ok(Step::Stopped);
                }
                r => r,
            };
            self.prefix.clear();
            self.save()?;
            last = turn?.text;
            // mail that arrived during the turn becomes the next turn
        }
        Ok(Step::Idle(last))
    }

    fn handoff_cli(&mut self, run: &str) -> Result<Option<Handoff>> {
        if self.thread.session.is_none() {
            threads::seal_run(
                self.conn,
                run,
                None,
                if self.is_main() { "main" } else { "general" },
                "",
            )?;
            self.run_id = None;
            return Ok(None);
        }
        let main = self.is_main();
        let ask = format!(
            "{} Reply with only the JSON object.",
            if main { HANDOFF_MAIN } else { HANDOFF_BG }
        );
        let turn = self.cli_turn(run, &ask, Some(&handoff_schema(main)))?;
        let v = turn.structured.ok_or_else(|| {
            anyhow::anyhow!("{} returned no structured handoff", self.provider.name())
        })?;
        self.finish_handoff(run, v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::llm::Scripted;

    fn tool_use(name: &str, input: Value) -> Value {
        json!({"stop_reason": "tool_use", "content": [{"type": "tool_use", "id": "tu1", "name": name, "input": input}], "usage": {"input_tokens": 50}})
    }

    fn agent<'a>(c: &'a Connection, llm: &'a dyn Llm, id: &str) -> Agent<'a> {
        Agent::load(
            c,
            llm,
            id,
            "/repo",
            Approval::Deny,
            None,
            Arc::new(AtomicBool::new(false)),
            Box::new(|_| {}),
        )
        .unwrap()
    }

    #[test]
    fn background_thread_runs_task_steers_naps_and_hands_off() {
        let c = db::open_memory();
        threads::ensure_main(&c, "/repo").unwrap();
        threads::create(
            &c,
            "billing-pg",
            "move billing to Postgres",
            "/repo",
            None,
            Some("main"),
            "",
        )
        .unwrap();
        threads::send(&c, "billing-pg", "main", "move billing to Postgres").unwrap();
        let llm = Scripted::new(vec![
            tool_use(
                "remember",
                json!({"entity": "billing", "attribute": "database", "value": "Postgres 16"}),
            ),
            Scripted::text("Done: migration written on thread/billing-pg."),
            // nap
            Scripted::json(
                json!({"summary": "decided on PG16", "state": "schema drafted", "facts": []}),
            ),
            // follow-up turn
            Scripted::text("Backfill script added."),
            // handoff
            Scripted::json(
                json!({"state": "migration + backfill written", "outcome": "success", "task_type": "db-migration", "summary": "Wrote billing migration.", "facts": [{"entity": "billing", "attribute": "owner", "value": "payments team"}]}),
            ),
        ]);
        let mut a =
            agent(&c, &llm, "billing-pg").with_local(Provider::Ollama, Some("qwen3:8b".into()));
        // steering message lands while the first tool call runs
        threads::send(&c, "billing-pg", "user", "also add a backfill").unwrap();
        let Step::Idle(text) = a.step().unwrap() else {
            panic!()
        };
        assert!(text.contains("migration written"));
        {
            let reqs = llm.requests.lock().unwrap();
            let first = reqs[0]["messages"][0]["content"].as_str().unwrap();
            assert!(first.contains("<thread name=\"billing-pg\">"), "{first}");
            assert!(
                first.contains("[from main] move billing") && first.contains("also add a backfill")
            );
            assert!(
                reqs[0]["system"]
                    .as_str()
                    .unwrap()
                    .contains("background thread")
            );
        }
        a.nap(false).unwrap();
        threads::send(&c, "billing-pg", "main", "status?").unwrap();
        a.step().unwrap();
        {
            let reqs = llm.requests.lock().unwrap();
            let after = reqs[3]["messages"].as_array().unwrap();
            assert_eq!(after.len(), 1, "conversation restarted from the distillate");
            assert!(
                after[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("decided on PG16")
            );
        }
        let h = a.handoff().unwrap().unwrap();
        assert_eq!(h.outcome.as_deref(), Some("success"));
        assert!(
            threads::get(&c, "billing-pg")
                .unwrap()
                .state
                .contains("backfill")
        );
        assert_eq!(memory::list_facts(&c, false).unwrap().len(), 2);
        assert_eq!(consolidate::pending(&c).unwrap(), 1);
        // turns, the nap and the handoff all run on the thread's model (one model loaded locally)
        let reqs = llm.requests.lock().unwrap();
        assert_eq!(reqs.len(), 5);
        assert!(reqs.iter().all(|r| r["model"] == "qwen3:8b"), "{reqs:?}");
        drop(reqs);
        // the conversation itself survives for follow-ups
        let (msgs, _) = threads::load_messages(&c, "billing-pg").unwrap();
        assert!(!msgs.is_empty());
    }

    fn bg_thread_with_mail(c: &Connection) {
        threads::ensure_main(c, "/repo").unwrap();
        threads::create(c, "probe", "look around", "/repo", None, Some("main"), "").unwrap();
        threads::send(c, "probe", "main", "look around").unwrap();
    }

    #[test]
    fn failed_handoff_retries_once_then_seals_with_the_last_output() {
        let c = db::open_memory();
        bg_thread_with_mail(&c);
        let llm = Scripted::new(vec![
            Scripted::text("Found it: the config is stale."),
            // the handoff answers tool calls, then prose: never the JSON object
            tool_use("bash", json!({"command": "ls"})),
            Scripted::text("Here's my summary!"),
        ]);
        let mut a = agent(&c, &llm, "probe");
        a.step().unwrap();
        let run = a.run_id.clone().unwrap();
        let h = a.handoff().unwrap().unwrap();
        assert!(
            h.error.as_deref().unwrap().contains("non-JSON"),
            "{:?}",
            h.error
        );
        assert_eq!(h.outcome, None);
        assert_eq!(h.summary, "Found it: the config is stale.");
        assert_eq!(
            llm.requests.lock().unwrap().len(),
            3,
            "turn, handoff, one retry"
        );
        assert!(a.run_id.is_none());
        assert!(threads::live_run(&c, "probe").unwrap().is_none());
        let summary: String = c
            .query_row("SELECT summary FROM runs WHERE id = ?1", [&run], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(summary, "Found it: the config is stale.");
        // a request error (the script is exhausted) closes the run the same way
        threads::send(&c, "probe", "main", "again").unwrap();
        let llm = Scripted::new(vec![Scripted::text("ok")]);
        let mut a = agent(&c, &llm, "probe");
        a.step().unwrap();
        let h = a.handoff().unwrap().unwrap();
        assert!(h.error.unwrap().contains("exhausted"));
        assert!(threads::live_run(&c, "probe").unwrap().is_none());
    }

    #[test]
    fn handoff_retry_can_succeed() {
        let c = db::open_memory();
        bg_thread_with_mail(&c);
        let llm = Scripted::new(vec![
            Scripted::text("done"),
            Scripted::text("not json"),
            Scripted::json(
                json!({"state": "s", "outcome": "success", "task_type": "probe", "summary": "Looked around.", "facts": []}),
            ),
        ]);
        let mut a = agent(&c, &llm, "probe");
        a.step().unwrap();
        let h = a.handoff().unwrap().unwrap();
        assert!(h.error.is_none());
        assert_eq!(
            (h.outcome.as_deref(), h.summary.as_str()),
            (Some("success"), "Looked around.")
        );
    }

    #[test]
    fn main_sees_threads_and_reports_as_mail() {
        let c = db::open_memory();
        threads::ensure_main(&c, "/repo").unwrap();
        threads::create(
            &c,
            "fix-auth",
            "fix login redirect",
            "/repo",
            None,
            Some("main"),
            "",
        )
        .unwrap();
        threads::send(&c, "main", "fix-auth", "finished (success): redirect fixed").unwrap();
        threads::send(&c, "main", "user", "what's the status?").unwrap();
        let llm = Scripted::new(vec![Scripted::text("fix-auth is done.")]);
        let mut a = agent(&c, &llm, "main");
        a.step().unwrap();
        let reqs = llm.requests.lock().unwrap();
        let first = reqs[0]["messages"][0]["content"].as_str().unwrap();
        assert!(
            first.contains("<repo") && first.contains("fix-auth [idle]"),
            "{first}"
        );
        assert!(
            first.contains("[from fix-auth] finished") && first.ends_with("what's the status?")
        );
        assert!(reqs[0]["tools"].to_string().contains("spawn_thread"));
    }

    #[test]
    fn restart_closes_dangling_tool_use() {
        let c = db::open_memory();
        threads::ensure_main(&c, "/repo").unwrap();
        let msgs = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "x1", "name": "bash", "input": {"command": "ls"}}]}),
        ];
        threads::save_messages(&c, "main", &msgs, "").unwrap();
        let llm = Scripted::new(vec![]);
        let a = agent(&c, &llm, "main");
        assert!(a.has_pending(), "resumes by answering the interrupted call");
    }
}
