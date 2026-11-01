mod agent;
mod cli;
mod consolidate;
mod db;
mod input;
mod llm;
mod mcp;
mod memory;
mod picker;
mod provider;
mod runtime;
mod threads;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use runtime::Runtime;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use provider::Provider;
use threads::MAIN;

/// A lightweight agent harness: `main` plus named background threads, with memory consolidation.
#[derive(Parser)]
#[command(name = "thread", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Thread to open (default: main)
    name: Option<String>,
    /// Pick a thread to open from all of this repo's threads, closed ones included
    #[arg(long, short = 'r')]
    resume: bool,
    /// Run tools without asking, in every thread
    #[arg(long, global = true)]
    yolo: bool,
    /// Provider for threads that don't name one: claude | codex | grok | api (default: `thread use`, else first logged in)
    #[arg(long, global = true)]
    provider: Option<String>,
    /// Model for the default provider (/model picks per thread)
    #[arg(long, global = true)]
    model: Option<String>,
    /// Model for memory consolidation (default: $THREAD_MEMORY_MODEL or the thread model)
    #[arg(long, global = true)]
    memory_model: Option<String>,
    /// Reasoning effort sent to the api provider as `reasoning_effort` (only when set)
    #[arg(long, global = true)]
    effort: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in: claude, codex or grok (your subscription, via its CLI), or api (a key). No argument: show status
    Login { provider: Option<String> },
    /// Show which providers are installed and logged in
    #[command(hide = true)]
    Status,
    /// Set the default provider
    #[command(hide = true)]
    Use { provider: String },
    /// MCP server for provider CLIs (started by the harness)
    #[command(hide = true)]
    Mcp,
    /// Open a thread by name (same as `thread <name>`)
    #[command(hide = true)]
    Open { name: String },
    /// List threads
    Ls { #[arg(long)] all: bool },
    /// Show a thread's state, latest output, and runs
    #[command(hide = true)]
    Show { name: String },
    /// One-shot: send a message, wait for all threads to settle, exit
    Ask { prompt: String, #[arg(long, default_value = MAIN)] to: String },
    /// Stop a background thread
    #[command(hide = true)]
    Stop { name: String },
    /// Delete threads and their worktrees (branches are kept). `--failed` removes every failed thread
    #[command(hide = true)]
    Rm { names: Vec<String>, #[arg(long)] failed: bool },
    /// Search memory
    #[command(hide = true)]
    Recall { query: String },
    /// Assert a fact directly (trusted, goes straight to active)
    #[command(hide = true)]
    Remember { entity: String, attribute: String, value: String },
    /// List facts
    #[command(hide = true)]
    Facts { #[arg(long)] all: bool },
    /// List skills
    #[command(hide = true)]
    Skills,
    /// Run consolidation over sealed runs now
    #[command(hide = true)]
    Consolidate,
    /// Audit log of memory changes
    #[command(hide = true)]
    Log { #[arg(short, default_value_t = 30)] n: usize },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// The repo root (git toplevel, else the current directory).
fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let out = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).current_dir(&cwd).output();
    Ok(match out {
        Ok(o) if o.status.success() => PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()),
        _ => cwd,
    })
}

/// Keep `.thread/` out of git without touching the repo's .gitignore.
fn exclude_from_git(root: &Path) {
    let path = root.join(".git").join("info").join("exclude");
    if !root.join(".git").is_dir() { return; }
    let cur = std::fs::read_to_string(&path).unwrap_or_default();
    if !cur.lines().any(|l| l.trim() == ".thread/") {
        std::fs::create_dir_all(path.parent().unwrap()).ok();
        std::fs::write(&path, format!("{cur}{}.thread/\n", if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" })).ok();
    }
}

struct NoKey;
impl llm::Llm for NoKey {
    fn send(&self, _: serde_json::Value) -> Result<serde_json::Value> {
        anyhow::bail!("this thread uses the api provider, which isn't set up (thread login api)")
    }
}

fn print_status() {
    for p in provider::ALL {
        let (ok, detail) = p.status();
        println!("{} {:<7} {detail}", if ok { "\x1b[32m✓\x1b[0m" } else { "\x1b[2m·\x1b[0m" }, p.name());
    }
    match provider::resolve_default(None) {
        Ok(p) => println!("\ndefault: {} (change with `thread use <provider>`)", p.name()),
        Err(e) => println!("\n{e}"),
    }
}

fn real_main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("mcp") { return mcp::serve(); }
    let cli = Cli::parse();
    match &cli.cmd {
        Some(Cmd::Status) => { print_status(); return Ok(()); }
        Some(Cmd::Use { provider }) => {
            let p = Provider::parse(provider)?;
            provider::set_default(p)?;
            println!("default provider: {}", p.name());
            if !p.status().0 { println!("note: {}", p.status().1); }
            return Ok(());
        }
        Some(Cmd::Login { provider }) => {
            let Some(name) = provider else { print_status(); println!("\nthread login <claude|codex|grok|api>"); return Ok(()); };
            let p = Provider::parse(name)?;
            p.login()?;
            if p == Provider::Grok {
                // Grok has no per-call MCP flag; register the harness server once (it reads its thread from env).
                let exe = std::env::current_exe()?;
                std::process::Command::new("grok").args(["mcp", "add", "thread", "--"]).arg(&exe).arg("mcp").status().ok();
            }
            let (ok, detail) = p.status();
            println!("{} {}: {detail}", if ok { "✓" } else { "✗" }, p.name());
            if ok && provider::resolve_default(None).is_err() { provider::set_default(p)?; }
            return Ok(());
        }
        _ => {}
    }
    let root = repo_root()?;
    exclude_from_git(&root);
    let db_path = root.join(".thread").join("thread.db");
    let conn = db::open(&db_path)?;
    threads::ensure_main(&conn, &root.display().to_string())?;

    let model_flag = cli.model.clone().or_else(|| std::env::var("THREAD_MODEL").ok());
    let memory_model = cli.memory_model.clone().or_else(|| std::env::var("THREAD_MEMORY_MODEL").ok());
    let effort = cli.effort.clone().or_else(|| std::env::var("THREAD_EFFORT").ok());
    let provider_flag = cli.provider.clone();
    let api_llm = |model: Option<String>, effort: Option<String>| -> Option<Arc<dyn llm::Llm>> {
        llm::OpenAiCompat::from_config(model, effort).ok().map(|a| Arc::new(a) as Arc<dyn llm::Llm>)
    };
    let runtime = |interactive: bool| -> Result<Arc<Runtime>> {
        let default = provider::resolve_default(provider_flag.as_deref())?;
        let work = api_llm(model_flag.clone(), effort.clone()).unwrap_or_else(|| Arc::new(NoKey));
        let mem_provider = std::env::var("THREAD_MEMORY_PROVIDER").ok().map(|p| Provider::parse(&p)).transpose()?.unwrap_or(default);
        let mem_model = memory_model.clone().or_else(|| (mem_provider == default).then(|| model_flag.clone()).flatten());
        let mem = runtime::memory_llm(mem_provider, api_llm(mem_model.clone(), None), mem_model)?;
        let cli_model = if default == Provider::Api { None } else { model_flag.clone() };
        let rt = Runtime::with(db_path.clone(), root.clone(), work, mem, cli.yolo, interactive, default, cli_model);
        rt.start_supervisor();
        Ok(rt)
    };

    let cmd = match (cli.cmd, cli.name.clone()) {
        (Some(c), _) => c,
        (None, None) if cli.resume => match pick_thread(&conn, None, true, "resume a thread")? {
            Some(name) => Cmd::Open { name },
            None => return Ok(()),
        },
        (None, name) => Cmd::Open { name: name.unwrap_or_else(|| MAIN.into()) },
    };
    match cmd {
        Cmd::Open { name } => {
            let start = threads::find(&conn, &name)?;
            repl(&conn, &runtime(true)?, &start.id)?;
        }
        Cmd::Ls { all } => print_list(&conn, None, all)?,
        Cmd::Show { name } => show(&conn, &threads::find(&conn, &name)?.id)?,
        Cmd::Ask { prompt, to } => {
            let rt = runtime(false)?;
            let t = threads::find(&conn, &to)?;
            rt.set_focus(&t.id);
            rt.resume(&conn)?;
            rt.deliver(&conn, &t.id, "user", &prompt)?;
            rt.wait_idle(&conn, Duration::from_secs(3600))?;
            rt.shutdown(&conn, Duration::from_secs(300))?;
        }
        Cmd::Stop { name } => {
            let t = threads::find(&conn, &name)?;
            threads::set_status(&conn, &t.id, "stopped")?;
            println!("stopped {} (takes effect if it is running in another session on its next step)", t.id);
        }
        Cmd::Rm { names, failed } => {
            let ids = rm_targets(&conn, &names, failed)?;
            anyhow::ensure!(!ids.is_empty(), "usage: thread rm <name…> | thread rm --failed");
            for id in ids { println!("{}", runtime::delete_thread(&conn, &root, None, &id)?); }
        }
        Cmd::Recall { query } => {
            let hits = memory::recall(&conn, &query, &[], 15)?;
            if hits.is_empty() { println!("nothing found"); } else { print!("{}", memory::render_hits(&conn, &hits)?); }
        }
        Cmd::Remember { entity, attribute, value } => {
            println!("{:?}", memory::assert_fact(&conn, &entity, &attribute, &value, None)?);
        }
        Cmd::Facts { all } => {
            for f in memory::list_facts(&conn, all)? {
                println!("{:>4}  [{:<10}] {} · {} = {}  (support {})", f.id, f.status, f.entity, f.attribute, f.value, f.support);
            }
        }
        Cmd::Skills => {
            for s in memory::list_skills(&conn)? {
                println!("{} v{} [{}] {}/{} successful\n  when: {}\n{}\n", s.name, s.version, s.status, s.successes, s.uses, s.when_to_use,
                    s.steps.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"));
            }
        }
        Cmd::Consolidate => {
            if consolidate::pending(&conn)? == 0 { println!("nothing to consolidate"); return Ok(()); }
            let rt = runtime(false)?;
            let r = consolidate::consolidate(&conn, rt.mem_llm.as_ref())?;
            println!("{} runs scored, {} replayed, {} facts learned, new skills: {:?}, {} to cold", r.scored.len(), r.replayed, r.learned, r.skills_new, r.cold + r.retired);
        }
        Cmd::Login { .. } | Cmd::Status | Cmd::Use { .. } | Cmd::Mcp => unreachable!(),
        Cmd::Log { n } => {
            let mut stmt = conn.prepare("SELECT ts, kind, ref, detail FROM audit ORDER BY id DESC LIMIT ?1")?;
            for r in stmt.query_map([n as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))? {
                let (ts, kind, rf, detail) = r?;
                println!("{}  {kind:<16} {rf:<10} {}", ago(ts), llm::truncate(&detail, 120));
            }
        }
    }
    Ok(())
}

const HELP: &str = "\
just type to talk to the focused thread
y / n                  answer an approval
/model [name]          show or set this thread's model (`default` to reset)
/provider [name [model]] show providers, or move this thread (claude, codex, grok, api)
/mcp                   list MCP servers  ·  /mcp add <name> <command…|url>  ·  /mcp rm <name>
/threads               list threads
/y /n /always [name|all] answer approvals from any thread (`always`: never ask about that tool again)
/rm <name…> | --failed delete threads (worktrees removed, branches kept)
/switch [name]         focus another thread (main to go back; no name: pick from a list)
/clear                 clear the screen and scrollback
/expand [n|all]        show the latest (or nth-latest) thinking step in full; `all` shows every step in full
/collapse              thinking back to one line per step
! <command>            run a shell command in this thread's directory; its output goes along with your next message
exit                   quit
commands work without the slash too: `switch fix-auth`, `threads`, `clear`, `main`";

fn repl(conn: &Connection, rt: &Arc<Runtime>, start: &str) -> Result<()> {
    let resumed = rt.resume(conn)?;
    if !resumed.is_empty() { rt.notice(&format!("resumed: {}", resumed.join(", "))); }
    switch(conn, rt, start)?;
    input::install();
    let mut show_prompt = true;
    loop {
        if show_prompt { rt.prompt(); }
        show_prompt = true;
        let line = match input::read_line()? {
            input::Input::Line(l) => l,
            input::Input::Eof => break,
            // Ctrl-C: from a thread back to main; on main, quit the way `exit` does
            input::Input::Interrupt => {
                eprintln!();
                if rt.focus() == MAIN { break; }
                switch(conn, rt, MAIN)?;
                continue;
            }
        };
        let input = if line.trim_start().starts_with('!') { line.trim().to_string() } else { normalize(conn, line.trim()) };
        let input = input.as_str();
        if input.is_empty() { continue; }
        let focus = rt.focus();
        // a message to the focused thread: its worker prints the prompt when it goes idle
        if !input.starts_with(['/', '!']) && !matches!(input, "exit" | "quit") && !(matches!(input, "y" | "n" | "yes" | "no" | "a" | "always") && rt.pending_approval(conn, &focus).is_some()) {
            show_prompt = false;
        }
        if let Err(e) = handle(conn, rt, &focus, input) {
            if e.to_string() == "exit" { break; }
            eprintln!("\x1b[31m{e:#}\x1b[0m");
        }
    }
    exit(conn, rt)
}

fn handle(conn: &Connection, rt: &Arc<Runtime>, focus: &str, input: &str) -> Result<()> {
    if matches!(input, "exit" | "quit" | "/exit" | "/quit") { anyhow::bail!("exit"); }
    if matches!(input, "y" | "n" | "yes" | "no" | "a" | "always") && rt.pending_approval(conn, focus).is_some() {
        return answer(conn, rt, focus, if input.starts_with('a') { "/always" } else if input.starts_with('y') { "/y" } else { "/n" }, "");
    }
    if let Some(cmd) = input.strip_prefix('!') {
        return shell(conn, rt, focus, cmd.trim());
    }
    if !input.starts_with('/') {
        let t = threads::get(conn, focus)?;
        if t.kind == "background" && t.task.is_empty() { threads::set_task(conn, focus, input)?; }
        let note = rt.shell_notes.lock().unwrap().remove(focus);
        let msg = match note { Some(n) => format!("{n}\n{input}"), None => input.to_string() };
        return rt.deliver(conn, focus, "user", &msg);
    }
    let (cmd, arg) = input.split_once(char::is_whitespace).map(|(c, a)| (c, a.trim())).unwrap_or((input, ""));
    let t = threads::get(conn, focus)?;
    match cmd {
        "/help" | "/" | "/?" => eprintln!("{HELP}"),
        "/threads" | "/ls" => print_list(conn, Some(rt), false)?,
        "/switch" => {
            let id = if arg.is_empty() {
                match pick_thread(conn, Some(rt), false, "switch to")? { Some(id) => id, None => return Ok(()) }
            } else {
                threads::find(conn, arg)?.id
            };
            switch(conn, rt, &id)?;
        }
        "/main" => switch(conn, rt, MAIN)?,
        "/y" | "/n" | "/always" => answer(conn, rt, focus, cmd, arg)?,
        "/expand" if arg == "all" => {
            rt.thinking_expanded.store(true, std::sync::atomic::Ordering::Relaxed);
            provider::set_thinking_expanded(true)?;
            rt.notice("thinking steps now print in full · /collapse for one line each");
        }
        "/expand" => {
            let n: usize = if arg.is_empty() { 1 } else { arg.parse().context("usage: /expand [n|all]")? };
            let steps = threads::thinking(conn, focus, n.max(1))?;
            match steps.get(n.max(1) - 1) {
                Some(t) => eprintln!("{}", runtime::render_thinking(t, true)),
                None if steps.is_empty() => rt.notice(&format!("no thinking from {focus} yet")),
                None => rt.notice(&format!("{focus} has only {} thinking step(s)", steps.len())),
            }
        }
        "/collapse" => {
            rt.thinking_expanded.store(false, std::sync::atomic::Ordering::Relaxed);
            provider::set_thinking_expanded(false)?;
            rt.notice("thinking steps now print as one line · /expand to open the latest");
        }
        "/rm" => {
            let names: Vec<String> = arg.split_whitespace().filter(|a| *a != "--failed").map(String::from).collect();
            let ids = rm_targets(conn, &names, arg.split_whitespace().any(|a| a == "--failed"))?;
            anyhow::ensure!(!ids.is_empty(), "usage: /rm <name…> | /rm --failed");
            if ids.iter().any(|id| id == focus) { switch(conn, rt, MAIN)?; }
            for id in ids { rt.notice(&runtime::delete_thread(conn, &rt.root, Some(rt), &id)?); }
        }
        // screen and scrollback; nothing reprinted, the prompt alone says where you are
        "/clear" => { eprint!("\x1b[H\x1b[2J\x1b[3J"); }
        "/model" => {
            let p = rt.provider_of(&t);
            let flag_model = if p == rt.default_provider { rt.cli_model.clone() } else { None };
            match arg {
                "" | "refresh" => {
                    let current = t.model.clone().or(flag_model);
                    let (list, _, fetched) = provider::models(p, arg == "refresh");
                    let age = if fetched == 0 { "offline list".to_string() } else { format!("fetched {} ago", ago(fetched)) };
                    eprintln!("{} on {} — model: {}  \x1b[2m({age} · /model refresh)\x1b[0m", t.id, p.name(), current.as_deref().unwrap_or("default"));
                    for (i, m) in list.iter().enumerate() {
                        let mark = if current.as_deref() == Some(m.id.as_str()) { "▸" } else { " " };
                        eprintln!("{mark} {:>2}. {:<16} \x1b[2m{}\x1b[0m", i + 1, m.id, m.note);
                    }
                    if list.is_empty() { eprintln!("\x1b[2m(no model list available for {}; /model <name> passes it through)\x1b[0m", p.name()); }
                    eprintln!("\x1b[2m/model <number|name> to pick · /model default to reset\x1b[0m");
                }
                "default" | "reset" => { threads::set_model(conn, &t.id, None)?; rt.notice(&format!("{} uses {}'s default model", t.id, p.name())); }
                m => {
                    let id = provider::resolve_model(p, m)?;
                    threads::set_model(conn, &t.id, Some(&id))?;
                    rt.notice(&format!("{} now uses {id} on {} (from its next turn)", t.id, p.name()));
                }
            }
        }
        "/provider" => {
            if arg.is_empty() {
                let current = rt.provider_of(&t);
                for p in provider::ALL {
                    let (ok, detail) = p.status();
                    eprintln!("{} {} {:<7} \x1b[2m{detail}\x1b[0m", if p == current { "▸" } else { " " },
                        if ok { "\x1b[32m✓\x1b[0m" } else { "\x1b[2m·\x1b[0m" }, p.name());
                }
                eprintln!("\x1b[2m/provider <name> to switch {} · thread login <name> to sign in\x1b[0m", t.id);
                return Ok(());
            }
            let (name, model) = arg.split_once(char::is_whitespace).map(|(p, m)| (p, Some(m.trim()))).unwrap_or((arg, None));
            let msg = runtime::configure_thread(conn, rt.default_provider, &t.id, Some(name), model)?;
            rt.reload(&t.id);
            rt.notice(&format!("{msg} — fresh session; thread state and memory carry over{}", if model.is_none() { " · /model to pick one" } else { "" }));
        }
        "/mcp" => {
            let mut parts = arg.split_whitespace();
            match parts.next() {
                None => {
                    eprintln!("thread    \x1b[2mbuilt in: memory, threads, approvals\x1b[0m");
                    for (name, s) in provider::mcp_servers() {
                        let target = s["url"].as_str().map(String::from).unwrap_or_else(|| {
                            let args: Vec<&str> = s["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
                            format!("{} {}", s["command"].as_str().unwrap_or(""), args.join(" "))
                        });
                        eprintln!("{name:<9} \x1b[2m{}\x1b[0m", target.trim());
                    }
                    eprintln!("\x1b[2m/mcp add <name> <command…|url> · /mcp rm <name> — used by claude, codex and grok threads (from their next turn)\x1b[0m");
                }
                Some("add") => {
                    let name = parts.next().context("usage: /mcp add <name> <command…|url>")?;
                    let target: Vec<String> = parts.map(String::from).collect();
                    provider::mcp_add(name, &target)?;
                    rt.notice(&format!("added {name}"));
                }
                Some("rm" | "remove") => {
                    let name = parts.next().context("usage: /mcp rm <name>")?;
                    if provider::mcp_remove(name)? { rt.notice(&format!("removed {name}")); } else { rt.notice(&format!("no server named {name}")); }
                }
                Some(other) => anyhow::bail!("unknown /mcp command '{other}' (add, rm)"),
            }
        }
        _ => anyhow::bail!("unknown command {cmd} — /help"),
    }
    Ok(())
}

/// `! <command>`: run it in the thread's directory, show the output as it comes, and hold
/// it for the user's next message to that thread (so the agent sees what they saw).
fn shell(conn: &Connection, rt: &Arc<Runtime>, focus: &str, cmd: &str) -> Result<()> {
    anyhow::ensure!(!cmd.is_empty(), "usage: ! <command>");
    let dir = threads::get(conn, focus)?.workdir;
    let sh = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
    let mut child = std::process::Command::new(&sh).arg("-c").arg(format!("exec 2>&1\n{cmd}")).current_dir(&dir)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).spawn()
        .with_context(|| format!("running {sh}"))?;
    let mut out = String::new();
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap_or_default();
        println!("{line}");
        out.push_str(&line);
        out.push('\n');
    }
    let status = child.wait()?;
    input::take(); // a Ctrl-C stopped the command, not the REPL
    if !status.success() { eprintln!("\x1b[31mexit {}\x1b[0m", status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())); }
    let note = format!("[I ran `{cmd}` in {dir}{}]\n{}\n", if status.success() { String::new() } else { format!(", exit {}", status.code().unwrap_or(-1)) },
        llm::truncate(out.trim_end(), 8000));
    rt.shell_notes.lock().unwrap().entry(focus.to_string()).or_default().push_str(&note);
    Ok(())
}

/// `/y`, `/n`, `/always` [name|all]: answer approvals from anywhere. No name: the focused
/// thread's, else the oldest. `/always` also stops asking about that tool, in every thread.
fn answer(conn: &Connection, rt: &Arc<Runtime>, focus: &str, cmd: &str, arg: &str) -> Result<()> {
    let pending = threads::pending_approvals(conn)?;
    let target: Option<String> = match arg {
        "all" => None,
        "" => Some(pending.iter().find(|(_, t, _)| t == focus).or(pending.first()).map(|(_, t, _)| t.clone())
            .context("nothing is waiting for approval")?),
        name => Some(threads::find(conn, name)?.id),
    };
    let yes = cmd != "/n";
    let mut answered = 0;
    while let Some((t, what)) = rt.answer(conn, target.as_deref(), yes) {
        if cmd == "/always" {
            let tool = provider::tool_of(&what);
            provider::allow_always(tool)?;
            rt.notice(&format!("✓ {t}: {what} — {tool} won't ask again (any thread)"));
        } else {
            rt.notice(&format!("{} {t}: {what}", if yes { "✓ allowed" } else { "✗ denied" }));
        }
        // without `all`: one request
        answered += 1;
        // `/always` also clears that tool's other pending requests, in every thread
        if cmd == "/always" {
            let tool = provider::tool_of(&what).to_string();
            while let Some(id) = threads::pending_approvals(conn)?.into_iter().find(|(_, _, w)| provider::tool_of(w) == tool).map(|(id, _, _)| id) {
                threads::set_approval(conn, id, true)?;
            }
        }
        if arg != "all" { break; }
    }
    if answered == 0 { rt.notice("nothing is waiting for approval"); }
    Ok(())
}

/// Map what was typed onto a canonical command: case, aliases (`/thread`, `/t`, `/ls`),
/// and bare words without the slash. A bare word only counts when it can't be a message:
/// alone on the line, or `switch <name>` / `thread <name>` naming a thread that exists.
fn normalize(conn: &Connection, input: &str) -> String {
    let (word, arg) = input.split_once(char::is_whitespace).map(|(w, a)| (w, a.trim())).unwrap_or((input, ""));
    let slashed = word.starts_with('/');
    let name = word.trim_start_matches('/').to_lowercase();
    let canon = match name.as_str() {
        "switch" | "thread" | "open" => "/switch",
        "s" | "t" if slashed => "/switch",
        "yes" | "allow" if slashed => "/y",
        "no" | "deny" if slashed => "/n",
        "rm" | "remove" | "delete" if slashed => "/rm",
        "threads" | "ls" => "/threads",
        "main" => "/main",
        "clear" => "/clear",
        "expand" => "/expand",
        "collapse" => "/collapse",
        "help" => "/help",
        "model" => "/model",
        "provider" => "/provider",
        "mcp" => "/mcp",
        "exit" | "quit" => "exit",
        _ => return input.to_string(),
    };
    if !slashed {
        let is_command = match canon {
            "/switch" => arg.is_empty() || (!arg.contains(char::is_whitespace) && threads::find(conn, arg).is_ok()),
            _ => arg.is_empty(),
        };
        if !is_command { return input.to_string(); }
    }
    if arg.is_empty() { canon.to_string() } else { format!("{canon} {arg}") }
}

/// Threads named (resolved like `/switch`), plus every failed one with `failed`.
fn rm_targets(conn: &Connection, names: &[String], failed: bool) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for n in names { ids.push(threads::find(conn, n)?.id); }
    if failed { ids.extend(threads::list(conn, true)?.into_iter().filter(|t| t.status == "failed").map(|t| t.id)); }
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    Ok(ids)
}

/// Choose a thread from a list. `None` if cancelled.
fn pick_thread(conn: &Connection, rt: Option<&Arc<Runtime>>, all: bool, title: &str) -> Result<Option<String>> {
    let list = threads::list(conn, all)?;
    let focus = rt.map(|r| r.focus());
    let w = list.iter().map(|t| t.id.chars().count()).max().unwrap_or(0);
    let items: Vec<String> = list.iter().map(|t| {
        let provider = rt.map(|r| r.provider_of(t).name().to_string()).unwrap_or_else(|| t.provider.clone());
        let detail = if t.state.is_empty() { &t.title } else { t.state.lines().next().unwrap_or("") };
        format!("{:<w$}  {:<7} {:>4}  {:<6} {}", t.id, t.status, ago(t.updated_at), provider, detail)
    }).collect();
    let start = list.iter().position(|t| Some(&t.id) == focus.as_ref()).unwrap_or(0);
    Ok(picker::pick(title, &items, start)?.map(|i| list[i].id.clone()))
}

/// Focus a thread. Leaving an idle background thread hands off its run.
fn switch(conn: &Connection, rt: &Arc<Runtime>, to: &str) -> Result<()> {
    let from = rt.focus();
    if from != to && from != MAIN { rt.request_seal(&from); }
    rt.set_focus(to);
    let t = threads::get(conn, to)?;
    let branch = t.branch.as_ref().map(|b| format!(" · {b}")).unwrap_or_default();
    eprintln!("\x1b[1m── {} \x1b[0m\x1b[2m[{}] {}{branch}\x1b[0m", t.id, t.status, rt.provider_of(&t).name());
    if !t.task.is_empty() && t.kind == "background" { eprintln!("\x1b[2mtask: {}\x1b[0m", llm::truncate(&t.task, 300)); }
    if !t.state.is_empty() { eprintln!("\x1b[2mstate: {}\x1b[0m", llm::truncate(&t.state, 400)); }
    if let Some(last) = threads::recent_output(conn, to, 1)?.pop() {
        eprintln!("\x1b[2mlast:\x1b[0m {}", llm::truncate(&last, 1200));
    }
    if let Some(what) = rt.pending_approval(conn, to) { eprintln!("\x1b[33mallow {what}? (y/n · a = always allow {})\x1b[0m", provider::tool_of(&what)); }
    if t.id == MAIN && from == to {
        let others = threads::list(conn, false)?.len() - 1;
        if others > 0 { eprintln!("\x1b[2m{others} other thread(s) — /threads, /switch <name> · /help\x1b[0m"); }
        else { eprintln!("\x1b[2m/help for commands · exit to quit\x1b[0m"); }
    }
    Ok(())
}

fn exit(conn: &Connection, rt: &Arc<Runtime>) -> Result<()> {
    let running = rt.running(conn)?;
    if !running.is_empty() {
        eprint!("\x1b[33m{} still running ({}). [l]eave them (they resume next launch) or [w]ait? \x1b[0m", running.len(), running.join(", "));
        // Ctrl-C here means leave
        let wait = matches!(input::read_line()?, input::Input::Line(a) if a.trim().starts_with('w'));
        if wait {
            input::force_next();
            rt.notice("waiting for threads to finish… (ctrl-c to stop waiting and quit)");
            rt.wait_idle(conn, Duration::from_secs(3600))?;
        }
    }
    input::force_next();
    rt.notice("handing off… (ctrl-c again to quit now; unfinished work resumes next launch)");
    rt.shutdown(conn, Duration::from_secs(180))
}

fn ago(ts: i64) -> String {
    let d = db::now() - ts;
    match d {
        ..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

fn print_list(conn: &Connection, rt: Option<&Arc<Runtime>>, all: bool) -> Result<()> {
    let focus = rt.map(|r| r.focus());
    for t in threads::list(conn, all)? {
        let marker = if focus.as_deref() == Some(t.id.as_str()) { "▸" } else { " " };
        let approval = rt.and_then(|r| r.pending_approval(conn, &t.id)).map(|w| format!(" \x1b[33m⚠ wants to {w}\x1b[0m")).unwrap_or_default();
        let branch = t.branch.as_ref().map(|b| format!(" {b}")).unwrap_or_default();
        let detail = if t.state.is_empty() { &t.title } else { t.state.lines().next().unwrap_or("") };
        println!("{marker} {:<24} {:<8} {:>4}\x1b[2m{branch}  {}\x1b[0m{approval}", t.id, t.status, ago(t.updated_at), llm::truncate(detail, 80));
    }
    Ok(())
}

fn show(conn: &Connection, id: &str) -> Result<()> {
    let t = threads::get(conn, id)?;
    println!("{} [{}] {}", t.id, t.status, t.title);
    if let Some(b) = &t.branch { println!("branch: {b} at {}", t.workdir); }
    if let Some(p) = &t.parent_id { println!("spawned by: {p}"); }
    if !t.task.is_empty() { println!("task: {}", t.task); }
    println!("\nwhere things stand:\n{}\n", if t.state.is_empty() { "(nothing yet)" } else { &t.state });
    if let Some(last) = threads::recent_output(conn, id, 1)?.pop() { println!("latest output:\n{}\n", llm::truncate(&last, 2000)); }
    let mut stmt = conn.prepare("SELECT id, started_at, status, COALESCE(outcome,''), COALESCE(task_type,''), COALESCE(summary,''), surprise FROM runs WHERE thread_id = ?1 ORDER BY started_at DESC LIMIT 10")?;
    let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
        r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<f64>>(6)?)))?;
    println!("runs:");
    for r in rows {
        let (rid, started, status, outcome, tt, summary, surprise) = r?;
        let sp = surprise.map(|s| format!(" surprise {s:.2}")).unwrap_or_default();
        println!("  {rid} {} ago [{status}] {outcome} {tt}{sp}\n    {}", ago(started), llm::truncate(&summary, 200));
    }
    Ok(())
}
