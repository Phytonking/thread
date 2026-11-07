//! Runs every thread (main included) as a worker on its own OS thread.
//!
//! The REPL never talks to an agent directly: it delivers your input to the
//! focused thread's inbox, and only the focused thread prints its output. Other
//! threads print one-line notices (spawned, finished, needs approval). Switching
//! threads is just moving the focus.
//!
//! When a background thread goes idle while nobody is looking at it, its run is
//! handed off: state and facts are saved, a report goes to main's inbox (which
//! wakes main), and consolidation runs. Conversations are persisted after every
//! step, so threads left running when you quit resume on the next launch.

use crate::agent::{Agent, Echo, Step};
use crate::cli::{CliLlm, McpSpec};
use crate::llm::{self, Llm};
use crate::provider::Provider;
use crate::threads::{self, MAIN};
use crate::tools::Approval;
use crate::{consolidate, db};
use anyhow::{Result, bail};
use rusqlite::Connection;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Worker {
    wake: Sender<()>,
    cancel: Arc<AtomicBool>,
    seal: Arc<AtomicBool>,
    reload: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

pub struct Runtime {
    pub db_path: PathBuf,
    pub root: PathBuf,
    pub llm: Arc<dyn Llm>,
    pub mem_llm: Arc<dyn Llm>,
    pub yolo: bool,
    /// A human is at the REPL to answer approvals.
    pub interactive: bool,
    pub quiet: bool,
    /// Provider for threads that don't name one.
    pub default_provider: Provider,
    /// Model override for CLI providers (they use their own default otherwise).
    pub cli_model: Option<String>,
    /// Thinking steps print in full rather than one line each (`/expand all`, `/collapse`).
    pub thinking_expanded: AtomicBool,
    /// Output of `!` commands, attached to the user's next message to that thread.
    pub shell_notes: Mutex<HashMap<String, String>>,
    workers: Mutex<HashMap<String, Worker>>,
    focus: Mutex<String>,
}

impl Runtime {
    #[cfg(test)]
    pub fn new(
        db_path: PathBuf,
        root: PathBuf,
        llm: Arc<dyn Llm>,
        mem_llm: Arc<dyn Llm>,
        yolo: bool,
        interactive: bool,
    ) -> Arc<Self> {
        Self::with(
            db_path,
            root,
            llm,
            mem_llm,
            yolo,
            interactive,
            Provider::Api,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with(
        db_path: PathBuf,
        root: PathBuf,
        llm: Arc<dyn Llm>,
        mem_llm: Arc<dyn Llm>,
        yolo: bool,
        interactive: bool,
        default_provider: Provider,
        cli_model: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db_path,
            root,
            llm,
            mem_llm,
            yolo,
            interactive,
            quiet: false,
            default_provider,
            cli_model,
            thinking_expanded: AtomicBool::new(crate::provider::thinking_expanded()),
            shell_notes: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
            focus: Mutex::new(MAIN.to_string()),
        })
    }

    pub fn provider_of(&self, t: &threads::Thread) -> Provider {
        if t.provider.is_empty() {
            self.default_provider
        } else {
            Provider::parse(&t.provider).unwrap_or(self.default_provider)
        }
    }

    fn mcp_spec(&self, thread_id: &str) -> Option<McpSpec> {
        let exe = std::env::current_exe().ok()?;
        Some(McpSpec {
            exe,
            env: vec![
                ("THREAD_ID".into(), thread_id.into()),
                ("THREAD_DB".into(), self.db_path.display().to_string()),
                ("THREAD_ROOT".into(), self.root.display().to_string()),
                (
                    "THREAD_INTERACTIVE".into(),
                    if self.interactive {
                        "1".into()
                    } else {
                        "0".into()
                    },
                ),
            ],
            extra: crate::provider::mcp_servers().into_iter().collect(),
        })
    }

    // ---- focus & output ----

    pub fn focus(&self) -> String {
        self.focus.lock().unwrap().clone()
    }
    pub fn set_focus(&self, id: &str) {
        *self.focus.lock().unwrap() = id.to_string();
    }

    pub fn prompt(&self) {
        if self.quiet || !self.interactive {
            return;
        }
        eprint!("\x1b[36m{}› \x1b[0m", self.focus());
        std::io::stderr().flush().ok();
    }

    pub fn notice(&self, msg: &str) {
        if !self.quiet {
            eprintln!("\x1b[2m{msg}\x1b[0m");
        }
    }

    fn echo(&self, thread: &str, e: Echo) {
        if self.quiet {
            return;
        }
        if self.focus() == thread {
            match e {
                Echo::Text(t) => println!("{}", crate::markdown::for_stdout(t)),
                Echo::Thinking(t) => eprintln!(
                    "{}",
                    render_thinking(t, self.thinking_expanded.load(Ordering::Relaxed))
                ),
                Echo::Tool(name, input) => eprintln!("\x1b[2m• {name} {input}\x1b[0m"),
                Echo::ToolErr(err) => eprintln!("\x1b[31m  {err}\x1b[0m"),
                Echo::Info(i) => eprintln!("\x1b[2m({i})\x1b[0m"),
            }
        } else if let Echo::Info(i) = e {
            self.notice(&format!("[{thread}] {i}"));
        }
    }

    // ---- approvals (stored in the db so CLI subprocesses can ask too) ----

    pub fn answer(
        &self,
        conn: &Connection,
        thread: Option<&str>,
        yes: bool,
    ) -> Option<(String, String)> {
        threads::answer_approval(conn, thread, yes).ok().flatten()
    }

    pub fn pending_approval(&self, conn: &Connection, thread: &str) -> Option<String> {
        threads::pending_approvals(conn)
            .ok()?
            .into_iter()
            .find(|(_, t, _)| t == thread)
            .map(|(_, _, w)| w)
    }

    fn announce_approval(&self, thread: &str, what: &str) {
        if self.quiet {
            return;
        }
        if self.focus() == thread {
            eprintln!(
                "\x1b[33mallow {what}? (y/n · a = always allow {})\x1b[0m",
                crate::provider::tool_of(what)
            );
        } else {
            eprintln!(
                "\x1b[33m⚠ [{thread}] wants to {what}\x1b[0m\x1b[2m — /y {thread} · /n {thread} · /always {thread} · /y all\x1b[0m"
            );
        }
    }

    /// Background tick: wake threads that got mail from other processes (the MCP
    /// server), cancel threads marked stopped, and announce new approval requests.
    pub fn start_supervisor(self: &Arc<Self>) {
        let rt = self.clone();
        std::thread::Builder::new()
            .name("thread:supervisor".into())
            .spawn(move || {
                let Ok(conn) = db::open(&rt.db_path) else {
                    return;
                };
                let mut seen: HashSet<i64> = HashSet::new();
                loop {
                    for t in threads::list(&conn, true).unwrap_or_default() {
                        if t.status == "stopped" {
                            if let Some(w) = rt.workers.lock().unwrap().get(&t.id)
                                && !w.cancel.swap(true, Ordering::Relaxed)
                            {
                                w.wake.send(()).ok();
                            }
                        } else if threads::has_wake_mail(&conn, &t.id).unwrap_or(false) {
                            rt.ensure_worker(&t.id);
                            rt.wake(&t.id);
                        }
                    }
                    for (id, thread, what) in threads::pending_approvals(&conn).unwrap_or_default()
                    {
                        if seen.insert(id) {
                            rt.announce_approval(&thread, &what);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            })
            .expect("spawn supervisor");
    }

    // ---- workers ----

    fn ensure_worker(self: &Arc<Self>, id: &str) {
        let mut ws = self.workers.lock().unwrap();
        if ws.get(id).is_some_and(|w| !w.join.is_finished()) {
            return;
        }
        let (wake, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let seal = Arc::new(AtomicBool::new(false));
        let reload = Arc::new(AtomicBool::new(false));
        let (rt, tid, c, s, rl) = (
            self.clone(),
            id.to_string(),
            cancel.clone(),
            seal.clone(),
            reload.clone(),
        );
        let join = std::thread::Builder::new()
            .name(format!("thread:{id}"))
            .spawn(move || {
                if let Err(e) = worker_loop(&rt, &tid, &rx, &c, &s, &rl) {
                    rt.notice(&format!("✗ [{tid}] crashed: {e:#}"));
                }
            })
            .expect("spawn worker");
        ws.insert(
            id.to_string(),
            Worker {
                wake,
                cancel,
                seal,
                reload,
                join,
            },
        );
    }

    fn wake(&self, id: &str) {
        if let Some(w) = self.workers.lock().unwrap().get(id) {
            w.wake.send(()).ok();
        }
    }

    /// Put a message in a thread's inbox and make sure it gets processed.
    pub fn deliver(
        self: &Arc<Self>,
        conn: &Connection,
        to: &str,
        from: &str,
        body: &str,
    ) -> Result<()> {
        let t = threads::get(conn, to)?;
        if matches!(t.status.as_str(), "stopped" | "done") {
            threads::set_status(conn, to, "idle")?;
        }
        threads::send(conn, to, from, body)?;
        self.ensure_worker(to);
        self.wake(to);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_thread(
        self: &Arc<Self>,
        conn: &Connection,
        name: &str,
        task: &str,
        worktree: bool,
        parent: &str,
        provider: &str,
        model: Option<&str>,
    ) -> Result<threads::Thread> {
        let t = create_thread(
            conn, &self.root, name, task, worktree, parent, provider, model,
        )?;
        if !task.is_empty() {
            self.ensure_worker(&t.id);
            self.wake(&t.id);
        }
        let p = self.provider_of(&t);
        let m = t
            .model
            .as_ref()
            .map(|m| format!(" {m}"))
            .unwrap_or_default();
        self.notice(&format!(
            "◆ spawned {} ({}{m}){} — /switch {} to watch",
            t.id,
            p.name(),
            t.branch
                .as_ref()
                .map(|b| format!(" on {b}"))
                .unwrap_or_default(),
            t.id
        ));
        Ok(t)
    }

    pub fn stop(&self, conn: &Connection, id: &str) -> Result<()> {
        if let Some(w) = self.workers.lock().unwrap().get(id) {
            w.cancel.store(true, Ordering::Relaxed);
            w.wake.send(()).ok();
        }
        while self.answer(conn, Some(id), false).is_some() {}
        threads::set_status(conn, id, "stopped")
    }

    /// Restart a thread's worker (after its provider changed). It hands off first.
    pub fn reload(&self, id: &str) {
        if let Some(w) = self.workers.lock().unwrap().get(id) {
            w.seal.store(true, Ordering::Relaxed);
            w.reload.store(true, Ordering::Relaxed);
            w.wake.send(()).ok();
        }
    }

    /// Ask an idle thread to hand off its run (used when you switch away from it).
    pub fn request_seal(&self, id: &str) {
        if let Some(w) = self.workers.lock().unwrap().get(id) {
            w.seal.store(true, Ordering::Relaxed);
            w.wake.send(()).ok();
        }
    }

    /// Restart workers for threads that were mid-run, or have mail, when the process last exited.
    pub fn resume(self: &Arc<Self>, conn: &Connection) -> Result<Vec<String>> {
        let mut resumed = vec![];
        for t in threads::list(conn, false)? {
            if t.status == "running" || threads::has_wake_mail(conn, &t.id)? {
                self.ensure_worker(&t.id);
                self.wake(&t.id);
                resumed.push(t.id);
            }
        }
        Ok(resumed)
    }

    pub fn running(&self, conn: &Connection) -> Result<Vec<String>> {
        Ok(threads::list(conn, false)?
            .into_iter()
            .filter(|t| t.status == "running")
            .map(|t| t.id)
            .collect())
    }

    fn alive(&self) -> Vec<String> {
        self.workers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, w)| !w.join.is_finished())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Wait until no thread is running or has unprocessed mail.
    pub fn wait_idle(&self, conn: &Connection, timeout: Duration) -> Result<bool> {
        let start = Instant::now();
        let mut quiet_since: Option<Instant> = None;
        while start.elapsed() < timeout {
            let busy = !self.running(conn)?.is_empty()
                || self
                    .alive()
                    .iter()
                    .any(|id| threads::has_wake_mail(conn, id).unwrap_or(false));
            if busy {
                quiet_since = None;
            } else {
                // stay idle briefly: a finishing thread's report is about to wake main
                let q = *quiet_since.get_or_insert_with(Instant::now);
                if q.elapsed() > Duration::from_millis(1500) {
                    return Ok(true);
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(false)
    }

    /// Hand off every idle thread's live run, then return. Running threads are left to resume next launch.
    pub fn shutdown(&self, conn: &Connection, timeout: Duration) -> Result<()> {
        let idle_with_run: Vec<String> = self
            .alive()
            .into_iter()
            .filter(|id| {
                threads::get(conn, id)
                    .map(|t| t.status != "running")
                    .unwrap_or(false)
            })
            .filter(|id| threads::live_run(conn, id).ok().flatten().is_some())
            .collect();
        for id in &idle_with_run {
            self.request_seal(id);
        }
        let start = Instant::now();
        while start.elapsed() < timeout {
            if idle_with_run
                .iter()
                .all(|id| threads::live_run(conn, id).ok().flatten().is_none())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }
}

fn worker_loop(
    rt: &Arc<Runtime>,
    id: &str,
    wake: &Receiver<()>,
    cancel: &Arc<AtomicBool>,
    seal: &AtomicBool,
    reload: &AtomicBool,
) -> Result<()> {
    let conn = db::open(&rt.db_path)?;
    let t = threads::get(&conn, id)?;
    let approval = if rt.yolo || t.trusted {
        Approval::Allow
    } else if rt.interactive {
        Approval::Ask
    } else {
        Approval::Deny
    };
    let (rt2, id2) = (rt.clone(), id.to_string());
    let echo = Box::new(move |e: Echo| rt2.echo(&id2, e));
    let root = rt.root.display().to_string();
    let provider = rt.provider_of(&t);
    let mut agent = Agent::load(
        &conn,
        rt.llm.as_ref(),
        id,
        &root,
        approval,
        Some(rt.clone()),
        cancel.clone(),
        echo,
    )?;
    if provider != Provider::Api {
        // --model was given for the default provider; other providers use their own default.
        let model = if provider == rt.default_provider {
            rt.cli_model.clone()
        } else {
            None
        };
        agent = agent.with_cli(provider, model, rt.mcp_spec(id));
    }
    let mut failed = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            threads::set_status(&conn, id, "stopped")?;
            return Ok(());
        }
        // Moved to another provider (by /provider here, or by main's set_thread_model through the MCP
        // server): hand off and exit; the next message starts a worker on the new provider.
        if rt.provider_of(&threads::get(&conn, id)?) != provider {
            if agent.run_id.is_some()
                && let Err(e) = finish_run(rt, &conn, &mut agent, "")
            {
                rt.notice(&format!("✗ [{id}] handoff failed: {e:#}"));
            }
            return Ok(());
        }
        if agent.approval == Approval::Ask && threads::get(&conn, id)?.trusted {
            agent.approval = Approval::Allow;
        }
        let mail = threads::has_wake_mail(&conn, id)?;
        if mail || (agent.has_pending() && !failed) {
            failed = false;
            threads::set_status(&conn, id, "running")?;
            match agent.step() {
                Ok(Step::Idle(text)) => {
                    threads::set_status(&conn, id, "idle")?;
                    if id != MAIN && rt.focus() != id {
                        finish_run(rt, &conn, &mut agent, &text)?;
                    }
                    if rt.focus() == id {
                        rt.prompt();
                    }
                }
                Ok(Step::Stopped) => {
                    threads::set_status(&conn, id, "stopped")?;
                    return Ok(());
                }
                Err(e) => {
                    failed = true;
                    threads::set_status(&conn, id, "failed")?;
                    rt.notice(&format!("✗ [{id}] failed: {e:#}"));
                    if id != MAIN {
                        rt.deliver(&conn, MAIN, id, &format!("failed with an error: {e:#}"))?;
                    }
                }
            }
            continue;
        }
        if seal.swap(false, Ordering::Relaxed)
            && agent.run_id.is_some()
            && let Err(e) = finish_run(rt, &conn, &mut agent, "")
        {
            rt.notice(&format!("✗ [{id}] handoff failed: {e:#}"));
        }
        if reload.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Err(RecvTimeoutError::Disconnected) = wake.recv_timeout(Duration::from_secs(30)) {
            return Ok(());
        }
        while wake.try_recv().is_ok() {} // coalesce repeated wakes
    }
}

fn finish_run(
    rt: &Arc<Runtime>,
    conn: &Connection,
    agent: &mut Agent,
    final_text: &str,
) -> Result<()> {
    let id = agent.thread.id.clone();
    let Some(h) = agent.handoff()? else {
        return Ok(());
    };
    if id != MAIN {
        let outcome = h.outcome.unwrap_or_else(|| "partial".into());
        rt.notice(&format!(
            "✓ [{id}] finished ({outcome}): {} — /switch {id}",
            llm::truncate(&h.summary, 200)
        ));
        if !final_text.is_empty() {
            rt.deliver(
                conn,
                MAIN,
                &id,
                &format!(
                    "finished ({outcome}): {}\n\nReport:\n{}",
                    h.summary,
                    llm::truncate(final_text, 3000)
                ),
            )?;
        }
    }
    let r = consolidate::consolidate(conn, rt.mem_llm.as_ref())?;
    if !r.is_quiet() {
        rt.notice(&format!(
            "  memory: {} replayed, {} learned{}{}",
            r.replayed,
            r.learned,
            if r.skills_new.is_empty() {
                String::new()
            } else {
                format!(", new skills: {}", r.skills_new.join(", "))
            },
            if r.cold + r.retired > 0 {
                format!(", {} to cold", r.cold + r.retired)
            } else {
                String::new()
            }
        ));
    }
    Ok(())
}

/// A thinking step: one dim line (its first line, `+N lines`), or the whole text.
pub fn render_thinking(t: &str, full: bool) -> String {
    let lines: Vec<&str> = t
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if full {
        let body = lines
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        return format!("\x1b[2;3m∴ thinking\n{body}\x1b[0m");
    }
    let first = lines
        .first()
        .map(|l| l.trim().trim_matches(['*', '#', ' ']))
        .unwrap_or("");
    let head: String = first.chars().take(100).collect();
    let cut = head.len() < first.len();
    let more = match (lines.len().saturating_sub(1), cut) {
        (0, false) => String::new(),
        (0, true) => " … \x1b[0;2m/expand".into(),
        (n, _) => format!(
            "{} \x1b[0;2m(+{n} line{} · /expand)",
            if cut { " …" } else { "" },
            if n == 1 { "" } else { "s" }
        ),
    };
    format!("\x1b[2;3m∴ {head}{more}\x1b[0m")
}

/// Remove a thread for good: stop it, remove its worktree (the branch is kept), delete it.
pub fn delete_thread(
    conn: &Connection,
    root: &Path,
    rt: Option<&Runtime>,
    id: &str,
) -> Result<String> {
    let t = threads::get(conn, id)?;
    if t.id == MAIN {
        bail!("main cannot be removed");
    }
    if let Some(rt) = rt {
        rt.stop(conn, &t.id)?;
    }
    let mut note = String::new();
    if let Some(b) = &t.branch {
        note = match remove_worktree(root, &t.workdir) {
            Ok(()) => format!("; branch {b} kept"),
            Err(e) => format!(
                "; worktree {} left in place ({e}); branch {b} kept",
                t.workdir
            ),
        };
    }
    threads::delete(conn, &t.id)?;
    Ok(format!("removed {}{note}", t.id))
}

/// Move a thread to another provider and/or model. `None` keeps that part; for the model,
/// `Some("default")` resets to the provider's default. A new provider means a fresh CLI
/// session (state and memory carry over) and, unless one is given, the provider's default model.
pub fn configure_thread(
    conn: &Connection,
    default: Provider,
    id: &str,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<String> {
    let t = threads::get(conn, id)?;
    let current = if t.provider.is_empty() {
        default
    } else {
        Provider::parse(&t.provider).unwrap_or(default)
    };
    let p = match provider {
        Some(name) => {
            let p = Provider::parse(name)?;
            if !p.status().0 {
                bail!("{}: {}", p.name(), p.status().1);
            }
            p
        }
        None => current,
    };
    let model = match model {
        Some("default" | "reset") => None,
        Some(m) => Some(crate::provider::resolve_model(p, m)?),
        None if p != current => None,
        None => t.model.clone(),
    };
    if p != current {
        threads::set_provider(conn, id, p.name())?;
    }
    threads::set_model(conn, id, model.as_deref())?;
    Ok(format!(
        "{id} now runs on {} with {} (from its next turn)",
        p.name(),
        model.as_deref().unwrap_or("its default model")
    ))
}

/// Create a background thread (and its worktree), and queue its task. Works without a
/// runtime, so the MCP server can spawn threads; the supervisor then starts them.
#[allow(clippy::too_many_arguments)]
pub fn create_thread(
    conn: &Connection,
    root: &Path,
    name: &str,
    task: &str,
    worktree: bool,
    parent: &str,
    provider: &str,
    model: Option<&str>,
) -> Result<threads::Thread> {
    let name = threads::unique_name(conn, name)?;
    let (workdir, branch) = if worktree && is_git(root) {
        let (dir, branch) = add_worktree(root, &name)?;
        (dir.display().to_string(), Some(branch))
    } else {
        (root.display().to_string(), None)
    };
    // "default" means the parent's provider: a codex main spawns codex threads. (Before, they
    // landed on the user's default but kept the parent's model, which that provider can't run.)
    let parent_t = threads::get(conn, parent).ok();
    let provider = match &parent_t {
        Some(p) if provider.is_empty() => p.provider.clone(),
        _ => provider.to_string(),
    };
    let mut t = threads::create(
        conn,
        &name,
        task,
        &workdir,
        branch.as_deref(),
        Some(parent),
        &provider,
    )?;
    // An explicit model wins; otherwise, on the parent's provider, inherit its /model choice.
    if model.is_some() {
        threads::set_model(conn, &name, model)?;
        t.model = model.map(String::from);
    } else if let Some(p) = parent_t
        && p.model.is_some()
        && provider == p.provider
    {
        threads::set_model(conn, &name, p.model.as_deref())?;
        t.model = p.model;
    }
    if !task.is_empty() {
        threads::send(conn, &name, parent, task)?;
    }
    Ok(t)
}

/// The memory model for a provider: the API model, or a one-shot CLI call on the subscription.
pub fn memory_llm(
    provider: Provider,
    api: Option<Arc<dyn Llm>>,
    model: Option<String>,
) -> Result<Arc<dyn Llm>> {
    Ok(match provider {
        Provider::Api => {
            api.ok_or_else(|| anyhow::anyhow!("the api provider isn't set up (thread login api)"))?
        }
        p => Arc::new(CliLlm { provider: p, model }),
    })
}

// ---- git ----

pub fn is_git(root: &Path) -> bool {
    root.join(".git").exists()
}

fn add_worktree(root: &Path, name: &str) -> Result<(PathBuf, String)> {
    let dir = root.join(".thread").join("worktrees").join(name);
    let branch = format!("thread/{name}");
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add", "-b", &branch])
        .arg(&dir)
        .arg("HEAD")
        .output()?;
    if !out.status.success() {
        // branch left over from an earlier thread with this name
        let retry = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add"])
            .arg(&dir)
            .arg(&branch)
            .output()?;
        if !retry.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok((dir, branch))
}

pub fn remove_worktree(root: &Path, dir: &str) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "remove", dir])
        .output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Scripted;
    use serde_json::json;

    fn tmp_root() -> PathBuf {
        let d = std::env::temp_dir().join(format!("thread-rt-{}", db::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_provider_follows_the_parent_with_its_model() {
        let root = tmp_root();
        let conn = db::open(&root.join(".thread/thread.db")).unwrap();
        threads::ensure_main(&conn, &root.display().to_string()).unwrap();
        threads::set_provider(&conn, MAIN, "codex").unwrap();
        threads::set_model(&conn, MAIN, Some("gpt-x")).unwrap();
        let t = create_thread(&conn, &root, "child", "", false, MAIN, "", None).unwrap();
        assert_eq!(
            (t.provider.as_str(), t.model.as_deref()),
            ("codex", Some("gpt-x"))
        );
        // another provider never carries the parent's model over
        let t = create_thread(&conn, &root, "other", "", false, MAIN, "claude", None).unwrap();
        assert_eq!((t.provider.as_str(), t.model.as_deref()), ("claude", None));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn thinking_collapses_to_one_line() {
        let one = render_thinking("**Planning**\n\nstep one\nstep two", false);
        assert!(
            one.contains("∴ Planning") && one.contains("+2 lines") && !one.contains('\n'),
            "{one}"
        );
        let full = render_thinking("**Planning**\nstep one", true);
        assert!(full.contains("step one"), "{full}");
        assert!(!render_thinking("short", false).contains("/expand"));
    }

    #[test]
    fn background_thread_reports_to_main_which_wakes() {
        let root = tmp_root();
        let db_path = root.join(".thread/thread.db");
        let conn = db::open(&db_path).unwrap();
        threads::ensure_main(&conn, &root.display().to_string()).unwrap();
        let work = Arc::new(Scripted::new(vec![
            // bg thread does its task
            Scripted::text("Investigated: the cache key ignores locale."),
            // bg handoff
            Scripted::json(
                json!({"state": "root cause found", "outcome": "success", "task_type": "investigate-bug", "summary": "Cache key ignores locale.", "facts": []}),
            ),
            // main wakes on the report
            Scripted::text("cache-bug finished: the cache key ignores locale."),
        ]));
        let mem = Arc::new(Scripted::new(vec![]));
        let mut rt = Runtime::new(db_path, root.clone(), work.clone(), mem, false, false);
        Arc::get_mut(&mut rt).unwrap().quiet = true;
        let t = rt
            .spawn_thread(
                &conn,
                "Cache Bug",
                "find why the cache returns wrong locale",
                false,
                MAIN,
                "",
                None,
            )
            .unwrap();
        assert_eq!(t.id, "cache-bug");
        assert!(rt.wait_idle(&conn, Duration::from_secs(10)).unwrap());
        let reqs = work.requests.lock().unwrap();
        assert_eq!(reqs.len(), 3, "task, handoff, main's reaction");
        let main_req = reqs[2]["messages"][0]["content"].as_str().unwrap();
        assert!(
            main_req.contains("[from cache-bug] finished (success)"),
            "{main_req}"
        );
        drop(reqs);
        assert_eq!(
            threads::get(&conn, "cache-bug").unwrap().state,
            "root cause found"
        );
        assert_eq!(threads::get(&conn, MAIN).unwrap().status, "idle");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn approvals_queue_across_connections() {
        let root = tmp_root();
        let db_path = root.join("t.db");
        let conn = db::open(&db_path).unwrap();
        let rt = Runtime::new(
            db_path.clone(),
            root.clone(),
            Arc::new(Scripted::new(vec![])),
            Arc::new(Scripted::new(vec![])),
            false,
            true,
        );
        // another process (the MCP server) asks
        let h = std::thread::spawn(move || {
            let c2 = db::open(&db_path).unwrap();
            let id = threads::request_approval(&c2, "fix-auth", "bash `rm -rf build`").unwrap();
            threads::wait_approval(&c2, id, || false).unwrap()
        });
        while rt.pending_approval(&conn, "fix-auth").is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            rt.answer(&conn, Some("fix-auth"), true).unwrap().1,
            "bash `rm -rf build`"
        );
        assert!(h.join().unwrap());
        assert!(rt.answer(&conn, None, true).is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
