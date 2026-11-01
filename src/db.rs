//! SQLite storage, one file per repo at `<repo>/.thread/thread.db`.
//! Threads (durable conversations), their inboxes, runs (episodes) and the raw
//! event log, plus the semantic (facts) and procedural (skills) stores.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS threads (
  id          TEXT PRIMARY KEY,                  -- 'main' or a short id
  title       TEXT NOT NULL,
  kind        TEXT NOT NULL,                     -- main | background
  status      TEXT NOT NULL DEFAULT 'idle',      -- idle | running | done | failed | stopped
  task        TEXT NOT NULL DEFAULT '',
  workdir     TEXT NOT NULL,
  branch      TEXT,                              -- set when the thread has its own worktree
  parent_id   TEXT,
  state       TEXT NOT NULL DEFAULT '',          -- carry-forward: where things stand
  messages    TEXT NOT NULL DEFAULT '[]',        -- the live conversation, append-only
  prefix      TEXT NOT NULL DEFAULT '',          -- text owed to the next user message (nap distillate)
  trusted     INTEGER NOT NULL DEFAULT 0,        -- background tools run without approval
  provider    TEXT NOT NULL DEFAULT '',        -- claude | codex | grok | api
  session     TEXT,                              -- the provider CLI's session id, for resume
  model       TEXT,                              -- per-thread model override
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

-- Messages between threads: follow-ups to workers, reports and memory notices to main.
CREATE TABLE IF NOT EXISTS inbox (
  id          INTEGER PRIMARY KEY,
  thread_id   TEXT NOT NULL,
  sender      TEXT NOT NULL,                     -- user | main | <thread id> | memory
  body        TEXT NOT NULL,
  delivered   INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS inbox_pending ON inbox(thread_id, delivered);

-- Episodic store. A run is one stretch of work on a thread, from a request until idle.
CREATE TABLE IF NOT EXISTS runs (
  id          TEXT PRIMARY KEY,
  thread_id   TEXT NOT NULL,
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER,
  status      TEXT NOT NULL DEFAULT 'live',      -- live | sealed | consolidating | consolidated
  outcome     TEXT,                              -- success | partial | failure
  task_type   TEXT,
  summary     TEXT,
  surprise    REAL,
  skills_used TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS events (
  id          INTEGER PRIMARY KEY,
  run_id      TEXT NOT NULL,
  ts          INTEGER NOT NULL,
  kind        TEXT NOT NULL,                     -- user | assistant | tool_call | tool_result | nap
  tool        TEXT,
  is_error    INTEGER NOT NULL DEFAULT 0,
  body        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_run ON events(run_id, id);

-- Semantic store. Lossless: superseded facts stay, linked by replaced_by.
CREATE TABLE IF NOT EXISTS facts (
  id           INTEGER PRIMARY KEY,
  entity       TEXT NOT NULL,
  attribute    TEXT NOT NULL,
  value        TEXT NOT NULL,
  status       TEXT NOT NULL DEFAULT 'tentative', -- tentative | active | superseded | cold
  support      INTEGER NOT NULL DEFAULT 1,        -- distinct runs asserting it
  runs         TEXT NOT NULL DEFAULT '[]',
  created_at   INTEGER NOT NULL,
  valid_to     INTEGER,
  replaced_by  INTEGER,
  last_access  INTEGER NOT NULL,
  access_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS facts_key ON facts(entity, attribute, status);

-- Procedural store. Tentative until applied successfully.
CREATE TABLE IF NOT EXISTS skills (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL,
  version     INTEGER NOT NULL DEFAULT 1,
  when_to_use TEXT NOT NULL,
  steps       TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT 'tentative',   -- tentative | promoted | retired
  uses        INTEGER NOT NULL DEFAULT 0,
  successes   INTEGER NOT NULL DEFAULT 0,
  runs        TEXT NOT NULL DEFAULT '[]',
  created_at  INTEGER NOT NULL,
  last_used   INTEGER
);

CREATE TABLE IF NOT EXISTS audit (
  id      INTEGER PRIMARY KEY,
  ts      INTEGER NOT NULL,
  kind    TEXT NOT NULL,
  ref     TEXT NOT NULL,
  detail  TEXT NOT NULL
);

-- Approval requests, shared across processes (CLI providers ask through the MCP server).
CREATE TABLE IF NOT EXISTS approvals (
  id          INTEGER PRIMARY KEY,
  thread_id   TEXT NOT NULL,
  what        TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT 'pending',  -- pending | allowed | denied
  created_at  INTEGER NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS recall_fts USING fts5(kind UNINDEXED, ref UNINDEXED, text);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    // Background threads each hold their own connection to this file.
    conn.busy_timeout(Duration::from_secs(15))?;
    conn.execute_batch(SCHEMA)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Columns added after a database may already exist.
fn migrate(conn: &Connection) -> Result<()> {
    let cols: Vec<String> = conn.prepare("SELECT name FROM pragma_table_info('threads')")?
        .query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    if !cols.iter().any(|c| c == "provider") { conn.execute_batch("ALTER TABLE threads ADD COLUMN provider TEXT NOT NULL DEFAULT ''")?; }
    if !cols.iter().any(|c| c == "session") { conn.execute_batch("ALTER TABLE threads ADD COLUMN session TEXT")?; }
    if !cols.iter().any(|c| c == "model") { conn.execute_batch("ALTER TABLE threads ADD COLUMN model TEXT")?; }
    Ok(())
}

#[cfg(test)]
pub fn open_memory() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Short id: 6 base36 chars from time, pid and a counter.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let mut x = nanos ^ ((std::process::id() as u64) << 40) ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E3779B97F4A7C15);
    // splitmix finalizer so consecutive ids don't share prefixes
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..6).map(|_| { let c = alphabet[(x % 36) as usize] as char; x /= 36; c }).collect()
}

pub fn audit(conn: &Connection, kind: &str, r#ref: &str, detail: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO audit (ts, kind, ref, detail) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![now(), kind, r#ref, detail],
    )?;
    Ok(())
}

/// Replace the FTS row for (kind, ref).
pub fn index(conn: &Connection, kind: &str, r#ref: &str, text: &str) -> Result<()> {
    unindex(conn, kind, r#ref)?;
    conn.execute("INSERT INTO recall_fts (kind, ref, text) VALUES (?1, ?2, ?3)", rusqlite::params![kind, r#ref, text])?;
    Ok(())
}

pub fn unindex(conn: &Connection, kind: &str, r#ref: &str) -> Result<()> {
    conn.execute("DELETE FROM recall_fts WHERE kind = ?1 AND ref = ?2", rusqlite::params![kind, r#ref])?;
    Ok(())
}
