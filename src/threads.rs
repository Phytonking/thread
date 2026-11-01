//! Threads are durable conversations. Every repo has `main`; background threads
//! are spawned from it (or by the user) under a human-readable name, which is
//! also their id, their branch (`thread/<name>`), and how you resume them.

use crate::db::{self, now};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

pub const MAIN: &str = "main";

#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub task: String,
    pub workdir: String,
    pub branch: Option<String>,
    pub parent_id: Option<String>,
    pub state: String,
    pub trusted: bool,
    pub updated_at: i64,
    /// Empty means "the default provider".
    pub provider: String,
    pub session: Option<String>,
    pub model: Option<String>,
}

const COLS: &str = "id, title, kind, status, task, workdir, branch, parent_id, state, trusted, updated_at, provider, session, model";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get(0)?, title: r.get(1)?, kind: r.get(2)?, status: r.get(3)?, task: r.get(4)?, workdir: r.get(5)?,
        branch: r.get(6)?, parent_id: r.get(7)?, state: r.get(8)?, trusted: r.get::<_, i64>(9)? != 0, updated_at: r.get(10)?,
        provider: r.get(11)?, session: r.get(12)?, model: r.get(13)?,
    })
}

/// Lowercase kebab-case, ascii only, max 40 chars.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() { s.push(c.to_ascii_lowercase()); }
        else if !s.ends_with('-') && !s.is_empty() { s.push('-'); }
    }
    let s: String = s.trim_end_matches('-').chars().take(40).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { format!("thread-{}", db::new_id()) } else { s }
}

fn exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM threads WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
}

/// A free name based on `wanted`: `fix-auth`, then `fix-auth-2`, ...
pub fn unique_name(conn: &Connection, wanted: &str) -> Result<String> {
    let base = slug(wanted);
    if base != MAIN && !exists(conn, &base)? { return Ok(base); }
    for n in 2.. {
        let cand = format!("{base}-{n}");
        if !exists(conn, &cand)? { return Ok(cand); }
    }
    unreachable!()
}

pub fn ensure_main(conn: &Connection, workdir: &str) -> Result<Thread> {
    let t = now();
    conn.execute(
        "INSERT OR IGNORE INTO threads (id, title, kind, workdir, created_at, updated_at) VALUES (?1, 'main', 'main', ?2, ?3, ?3)",
        params![MAIN, workdir, t],
    )?;
    get(conn, MAIN)
}

pub fn create(conn: &Connection, name: &str, task: &str, workdir: &str, branch: Option<&str>, parent: Option<&str>, provider: &str) -> Result<Thread> {
    let t = now();
    let title = crate::llm::truncate(task.lines().next().unwrap_or(name), 100);
    conn.execute(
        "INSERT INTO threads (id, title, kind, task, workdir, branch, parent_id, provider, created_at, updated_at) VALUES (?1, ?2, 'background', ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![name, title, task, workdir, branch, parent, provider, t],
    )?;
    db::index(conn, "thread", name, &format!("{name} {task}"))?;
    get(conn, name)
}

pub fn get(conn: &Connection, id: &str) -> Result<Thread> {
    conn.query_row(&format!("SELECT {COLS} FROM threads WHERE id = ?1"), [id], row).optional()?
        .ok_or_else(|| anyhow::anyhow!("no thread named '{id}'"))
}

/// Resolve a name the way a person types it: exact, then unique prefix, then unique substring.
pub fn find(conn: &Connection, query: &str) -> Result<Thread> {
    let q = query.trim().trim_start_matches('@');
    if let Ok(t) = get(conn, q) { return Ok(t); }
    let all = list(conn, true)?;
    for pred in [|t: &Thread, q: &str| t.id.starts_with(q), |t: &Thread, q: &str| t.id.contains(q) || t.title.to_lowercase().contains(q)] {
        let hits: Vec<&Thread> = all.iter().filter(|t| pred(t, &q.to_lowercase())).collect();
        match hits.len() {
            1 => return Ok(hits[0].clone()),
            0 => {}
            _ => bail!("'{q}' matches {}: {}", hits.len(), hits.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", ")),
        }
    }
    bail!("no thread matches '{q}' (see `thread ls`)")
}

pub fn list(conn: &Connection, include_closed: bool) -> Result<Vec<Thread>> {
    let filter = if include_closed { "" } else { "WHERE status NOT IN ('done','stopped')" };
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM threads {filter} ORDER BY kind = 'main' DESC, updated_at DESC"))?;
    let rows = stmt.query_map([], row)?.collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Forget a thread: its row, mail and pending approvals. Its runs stay (they are memory).
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    if id == MAIN { bail!("main cannot be removed"); }
    conn.execute("DELETE FROM threads WHERE id = ?1", [id])?;
    conn.execute("DELETE FROM inbox WHERE thread_id = ?1", [id])?;
    conn.execute("DELETE FROM approvals WHERE thread_id = ?1 AND status = 'pending'", [id])?;
    db::unindex(conn, "thread", id)
}

pub fn set_status(conn: &Connection, id: &str, status: &str) -> Result<()> {
    conn.execute("UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1", params![id, status, now()])?;
    Ok(())
}

pub fn set_state(conn: &Connection, id: &str, state: &str) -> Result<()> {
    conn.execute("UPDATE threads SET state = ?2, updated_at = ?3 WHERE id = ?1", params![id, state, now()])?;
    let t = get(conn, id)?;
    db::index(conn, "thread", id, &format!("{id} {} {state}", t.task))?;
    Ok(())
}

pub fn set_task(conn: &Connection, id: &str, task: &str) -> Result<()> {
    let title = crate::llm::truncate(task.lines().next().unwrap_or(id), 100);
    conn.execute("UPDATE threads SET task = ?2, title = ?3 WHERE id = ?1", params![id, task, title])?;
    db::index(conn, "thread", id, &format!("{id} {task}"))
}

pub fn set_provider(conn: &Connection, id: &str, provider: &str) -> Result<()> {
    conn.execute("UPDATE threads SET provider = ?2, session = NULL WHERE id = ?1", params![id, provider])?;
    Ok(())
}

pub fn set_model(conn: &Connection, id: &str, model: Option<&str>) -> Result<()> {
    conn.execute("UPDATE threads SET model = ?2 WHERE id = ?1", params![id, model])?;
    Ok(())
}

pub fn set_session(conn: &Connection, id: &str, session: &str) -> Result<()> {
    conn.execute("UPDATE threads SET session = ?2 WHERE id = ?1", params![id, session])?;
    Ok(())
}

// ---- conversation persistence ----

pub fn load_messages(conn: &Connection, id: &str) -> Result<(Vec<Value>, String)> {
    let (m, p): (String, String) = conn.query_row("SELECT messages, prefix FROM threads WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok((serde_json::from_str(&m)?, p))
}

pub fn save_messages(conn: &Connection, id: &str, messages: &[Value], prefix: &str) -> Result<()> {
    conn.execute("UPDATE threads SET messages = ?2, prefix = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, serde_json::to_string(messages)?, prefix, now()])?;
    Ok(())
}

// ---- inbox ----

pub fn send(conn: &Connection, to: &str, from: &str, body: &str) -> Result<()> {
    conn.execute("INSERT INTO inbox (thread_id, sender, body, created_at) VALUES (?1, ?2, ?3, ?4)", params![to, from, body, now()])?;
    Ok(())
}

/// Mail that should wake a thread. Memory notices wait for the next real message.
pub fn has_wake_mail(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM inbox WHERE thread_id = ?1 AND delivered = 0 AND sender != 'memory' LIMIT 1", [id], |_| Ok(())).optional()?.is_some())
}

/// Pending messages as (sender, body), marked delivered.
pub fn take_mail(conn: &Connection, id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT id, sender, body FROM inbox WHERE thread_id = ?1 AND delivered = 0 ORDER BY id")?;
    let rows: Vec<(i64, String, String)> = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    for (mid, _, _) in &rows { conn.execute("UPDATE inbox SET delivered = 1 WHERE id = ?1", [mid])?; }
    Ok(rows.into_iter().map(|(_, s, b)| (s, b)).collect())
}

// ---- runs & events ----

pub fn live_run(conn: &Connection, thread_id: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT id FROM runs WHERE thread_id = ?1 AND status = 'live' ORDER BY started_at DESC LIMIT 1", [thread_id], |r| r.get(0)).optional()?)
}

pub fn start_run(conn: &Connection, thread_id: &str) -> Result<String> {
    let id = db::new_id();
    conn.execute("INSERT INTO runs (id, thread_id, started_at) VALUES (?1, ?2, ?3)", params![id, thread_id, now()])?;
    Ok(id)
}

pub fn log_event(conn: &Connection, run_id: &str, kind: &str, tool: Option<&str>, is_error: bool, body: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO events (run_id, ts, kind, tool, is_error, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![run_id, now(), kind, tool, is_error as i64, body],
    )?;
    Ok(())
}

pub fn seal_run(conn: &Connection, run_id: &str, outcome: Option<&str>, task_type: &str, summary: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET status = 'sealed', ended_at = ?2, outcome = ?3, task_type = ?4, summary = ?5 WHERE id = ?1",
        params![run_id, now(), outcome, task_type, summary],
    )?;
    db::index(conn, "episode", run_id, &format!("[{task_type}] {summary}"))?;
    Ok(())
}

pub fn record_skills_offered(conn: &Connection, run_id: &str, skill_ids: &[i64]) -> Result<()> {
    conn.execute("UPDATE runs SET skills_used = ?2 WHERE id = ?1", params![run_id, serde_json::to_string(skill_ids)?])?;
    Ok(())
}

/// The latest assistant texts on a thread, newest last.
pub fn recent_output(conn: &Connection, thread_id: &str, n: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT e.body FROM events e JOIN runs r ON r.id = e.run_id
        WHERE r.thread_id = ?1 AND e.kind = 'assistant' ORDER BY e.id DESC LIMIT ?2")?;
    let mut rows: Vec<String> = stmt.query_map(params![thread_id, n as i64], |r| r.get(0))?.collect::<Result<_, _>>()?;
    rows.reverse();
    Ok(rows)
}

/// The thread's thinking steps, newest first.
pub fn thinking(conn: &Connection, thread_id: &str, n: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT e.body FROM events e JOIN runs r ON r.id = e.run_id
        WHERE r.thread_id = ?1 AND e.kind = 'thinking' ORDER BY e.id DESC LIMIT ?2")?;
    let rows = stmt.query_map(params![thread_id, n as i64], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(rows)
}

pub fn thread_of_run(conn: &Connection, run_id: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT thread_id FROM runs WHERE id = ?1", [run_id], |r| r.get(0)).optional()?)
}

// ---- approvals (shared across processes) ----

pub fn request_approval(conn: &Connection, thread_id: &str, what: &str) -> Result<i64> {
    conn.execute("INSERT INTO approvals (thread_id, what, created_at) VALUES (?1, ?2, ?3)", params![thread_id, what, now()])?;
    Ok(conn.last_insert_rowid())
}

pub fn approval_status(conn: &Connection, id: i64) -> Result<String> {
    Ok(conn.query_row("SELECT status FROM approvals WHERE id = ?1", [id], |r| r.get(0))?)
}

/// Pending approvals as (id, thread, what), oldest first.
pub fn pending_approvals(conn: &Connection) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare("SELECT id, thread_id, what FROM approvals WHERE status = 'pending' ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Answer the oldest pending approval for `thread` (or any). Returns (thread, what).
pub fn answer_approval(conn: &Connection, thread: Option<&str>, yes: bool) -> Result<Option<(String, String)>> {
    let Some((id, t, what)) = pending_approvals(conn)?.into_iter().find(|(_, t, _)| thread.is_none_or(|x| x == t)) else { return Ok(None) };
    conn.execute("UPDATE approvals SET status = ?2 WHERE id = ?1 AND status = 'pending'", params![id, if yes { "allowed" } else { "denied" }])?;
    Ok(Some((t, what)))
}

pub fn set_approval(conn: &Connection, id: i64, yes: bool) -> Result<()> {
    conn.execute("UPDATE approvals SET status = ?2 WHERE id = ?1 AND status = 'pending'", params![id, if yes { "allowed" } else { "denied" }])?;
    Ok(())
}

/// Block until approval `id` is answered, or `cancelled()` turns true (then it is denied).
pub fn wait_approval(conn: &Connection, id: i64, cancelled: impl Fn() -> bool) -> Result<bool> {
    loop {
        match approval_status(conn, id)?.as_str() {
            "allowed" => return Ok(true),
            "denied" => return Ok(false),
            _ => {}
        }
        if cancelled() {
            conn.execute("UPDATE approvals SET status = 'denied' WHERE id = ?1", [id])?;
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_slugged_unique_and_findable() {
        let c = db::open_memory();
        ensure_main(&c, "/repo").unwrap();
        assert_eq!(unique_name(&c, "Fix the Auth bug!").unwrap(), "fix-the-auth-bug");
        create(&c, "fix-auth", "fix login redirect", "/repo", None, Some(MAIN), "").unwrap();
        create(&c, "perf-audit", "profile the search endpoint", "/repo", None, Some(MAIN), "").unwrap();
        assert_eq!(unique_name(&c, "fix auth").unwrap(), "fix-auth-2");
        assert_eq!(unique_name(&c, "main").unwrap(), "main-2");
        assert_eq!(find(&c, "perf").unwrap().id, "perf-audit");
        assert_eq!(find(&c, "@fix-auth").unwrap().id, "fix-auth");
        assert_eq!(find(&c, "search").unwrap().id, "perf-audit"); // matches the task title
        assert!(find(&c, "zzz").is_err());
    }

    #[test]
    fn inbox_delivers_once() {
        let c = db::open_memory();
        ensure_main(&c, "/repo").unwrap();
        send(&c, MAIN, "fix-auth", "done").unwrap();
        assert!(has_wake_mail(&c, MAIN).unwrap());
        assert_eq!(take_mail(&c, MAIN).unwrap(), vec![("fix-auth".to_string(), "done".to_string())]);
        assert!(!has_wake_mail(&c, MAIN).unwrap());
    }

    #[test]
    fn messages_roundtrip() {
        let c = db::open_memory();
        ensure_main(&c, "/repo").unwrap();
        let m = vec![serde_json::json!({"role": "user", "content": "hi"})];
        save_messages(&c, MAIN, &m, "pre").unwrap();
        assert_eq!(load_messages(&c, MAIN).unwrap(), (m, "pre".to_string()));
    }
}
