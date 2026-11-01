//! Lightweight memory consolidation.
//!
//! Facts are written at three points, all by the model that already holds the
//! context, so nothing re-reads transcripts by default:
//!   - `remember` tool calls, mid-work
//!   - a nap, when a thread's context grows long
//!   - the handoff, when a run goes idle
//!
//! After a run is sealed, `consolidate` does the cheap, deterministic part:
//!   - prediction error from external signals (outcome vs. task-type baseline,
//!     timing, retries, tool errors, user corrections, novelty)
//!   - replay only the surprising runs (bounded), asking what went wrong
//!   - abstract a tentative skill once a task type has 3 successful runs
//!   - decay idle memories to cold storage
//!
//! `learn` propagates contradictions immediately: when a fact is superseded, every
//! other open thread that relied on the old value gets an inbox notice.

use crate::agent::fact_schema;
use crate::db::{audit, now};
use crate::llm::{self, Llm};
use crate::memory::{self, FactOutcome};
use crate::threads;
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::json;
use std::collections::BTreeMap;

pub const REPLAY_ABOVE: f64 = 0.6;
pub const MAX_REPLAYS: usize = 3;
pub const SKILL_MIN_RUNS: usize = 3;

#[derive(Debug, Default)]
pub struct Report {
    pub scored: Vec<(String, f64)>,
    pub replayed: usize,
    pub learned: usize,
    pub skills_new: Vec<String>,
    pub cold: usize,
    pub retired: usize,
}

impl Report {
    pub fn is_quiet(&self) -> bool {
        self.replayed == 0
            && self.learned == 0
            && self.skills_new.is_empty()
            && self.cold == 0
            && self.retired == 0
    }
}

/// Assert a fact and tell other threads if it overturned something they relied on.
pub fn learn(
    conn: &Connection,
    run: Option<&str>,
    thread_id: &str,
    entity: &str,
    attribute: &str,
    value: &str,
) -> Result<FactOutcome> {
    let out = memory::assert_fact(conn, entity, attribute, value, run)?;
    let new_id = match &out {
        FactOutcome::Superseded { new, .. } => Some(*new),
        FactOutcome::Corroborated { id, promoted: true } => Some(*id),
        _ => None,
    };
    if let Some(new_id) = new_id {
        let new = memory::get_fact(conn, new_id)?;
        let mut stmt = conn.prepare("SELECT id FROM facts WHERE replaced_by = ?1")?;
        let olds: Vec<i64> = stmt
            .query_map([new_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for old_id in olds {
            let old = memory::get_fact(conn, old_id)?;
            for t in memory::threads_of_fact(conn, &old)? {
                if t == thread_id {
                    continue;
                }
                let open = threads::get(conn, &t)
                    .map(|t| !matches!(t.status.as_str(), "done" | "stopped"))
                    .unwrap_or(false);
                if open {
                    threads::send(
                        conn,
                        &t,
                        "memory",
                        &format!(
                            "{} {} changed from \"{}\" to \"{}\" (learned in thread {thread_id})",
                            new.entity, new.attribute, old.value, new.value
                        ),
                    )?;
                }
            }
        }
    }
    Ok(out)
}

pub fn learn_all(
    conn: &Connection,
    run: Option<&str>,
    thread_id: &str,
    facts: &serde_json::Value,
) -> Result<usize> {
    let mut n = 0;
    for f in facts.as_array().into_iter().flatten() {
        let (e, a, v) = (
            f["entity"].as_str().unwrap_or(""),
            f["attribute"].as_str().unwrap_or(""),
            f["value"].as_str().unwrap_or(""),
        );
        if e.is_empty() || a.is_empty() || v.is_empty() {
            continue;
        }
        learn(conn, run, thread_id, e, a, v)?;
        n += 1;
    }
    Ok(n)
}

// ---- prediction error ----

#[derive(Debug, Default, Clone)]
pub struct Signals {
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub retries: usize,
    pub corrections: usize,
    pub duration: i64,
    pub outcome: String,
    pub task_type: String,
}

const CORRECTION_CUES: &[&str] = &[
    "no,",
    "no.",
    "nope",
    "wrong",
    "that's not",
    "thats not",
    "not what i",
    "instead",
    "undo",
    "revert",
    "don't",
    "do not",
    "stop",
];

pub fn signals(conn: &Connection, run_id: &str) -> Result<Signals> {
    let (started, ended, outcome, task_type): (i64, Option<i64>, Option<String>, Option<String>) =
        conn.query_row(
            "SELECT started_at, ended_at, outcome, task_type FROM runs WHERE id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let mut s = Signals {
        duration: ended.unwrap_or(started) - started,
        outcome: outcome.unwrap_or_default(),
        task_type: task_type.unwrap_or_default(),
        ..Default::default()
    };
    let mut stmt = conn
        .prepare("SELECT kind, tool, is_error, body FROM events WHERE run_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map([run_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut last_failed_tool: Option<String> = None;
    let mut user_turns = 0;
    for row in rows {
        let (kind, tool, is_error, body) = row?;
        match kind.as_str() {
            "tool_call" => {
                s.tool_calls += 1;
                if last_failed_tool.is_some() && last_failed_tool == tool {
                    s.retries += 1;
                }
            }
            "tool_result" => {
                if is_error {
                    s.tool_errors += 1;
                    last_failed_tool = tool;
                } else {
                    last_failed_tool = None;
                }
            }
            "user" => {
                user_turns += 1;
                let lower = body.to_lowercase();
                if user_turns > 1
                    && CORRECTION_CUES
                        .iter()
                        .any(|c| lower.starts_with(c) || lower.contains(&format!(" {c}")))
                {
                    s.corrections += 1;
                }
            }
            _ => {}
        }
    }
    Ok(s)
}

#[derive(Debug, Default)]
pub struct Baseline {
    pub n: usize,
    pub success_rate: f64,
    pub mean_duration: f64,
    pub sd_duration: f64,
}

pub fn baseline(conn: &Connection, task_type: &str, excluding: &str) -> Result<Baseline> {
    let mut stmt = conn.prepare(
        "SELECT outcome, COALESCE(ended_at, started_at) - started_at FROM runs
        WHERE task_type = ?1 AND id != ?2 AND status != 'live'",
    )?;
    let rows: Vec<(Option<String>, i64)> = stmt
        .query_map(params![task_type, excluding], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let n = rows.len();
    if n == 0 {
        return Ok(Baseline::default());
    }
    let success_rate = rows
        .iter()
        .filter(|(o, _)| o.as_deref() == Some("success"))
        .count() as f64
        / n as f64;
    let mean = rows.iter().map(|(_, d)| *d as f64).sum::<f64>() / n as f64;
    let var = rows
        .iter()
        .map(|(_, d)| (*d as f64 - mean).powi(2))
        .sum::<f64>()
        / n as f64;
    Ok(Baseline {
        n,
        success_rate,
        mean_duration: mean,
        sd_duration: var.sqrt(),
    })
}

/// Prediction error, plus the human-readable reasons behind it.
pub fn surprise(s: &Signals, b: &Baseline) -> (f64, Vec<String>) {
    let mut score = 0.0;
    let mut why = vec![];
    let success = if s.outcome == "success" { 1.0 } else { 0.0 };
    let dev = if b.n >= 3 {
        (success - b.success_rate).abs()
    } else if success < 1.0 {
        0.3
    } else {
        0.0
    };
    if dev > 0.0 {
        score += dev;
        why.push(if b.n >= 3 {
            format!(
                "outcome {} vs {:.0}% historical success",
                s.outcome,
                b.success_rate * 100.0
            )
        } else {
            format!("outcome {}", s.outcome)
        });
    }
    match b.n {
        0 => {
            score += 0.5;
            why.push("first run of this task type".into());
        }
        1 | 2 => score += 0.2,
        _ => {}
    }
    if b.n >= 3 && b.sd_duration > 0.0 {
        let z = (s.duration as f64 - b.mean_duration) / b.sd_duration;
        if z > 0.0 {
            score += (z / 3.0).min(1.0) * 0.5;
            if z > 1.5 {
                why.push(format!(
                    "took {}s vs typical {:.0}s",
                    s.duration, b.mean_duration
                ));
            }
        }
    }
    if s.retries > 0 {
        score += (s.retries as f64 / 3.0).min(1.0) * 0.5;
        why.push(format!("{} retries after failed tool calls", s.retries));
    }
    if s.corrections > 0 {
        score += (s.corrections as f64 / 2.0).min(1.0) * 0.7;
        why.push(format!("{} user corrections", s.corrections));
    }
    if s.tool_calls > 0 && s.tool_errors > 0 {
        score += 0.3 * s.tool_errors as f64 / s.tool_calls as f64;
    }
    (score, why)
}

// ---- replay (surprising runs only) ----

fn transcript(conn: &Connection, run_id: &str, budget: usize) -> Result<String> {
    let mut stmt = conn.prepare("SELECT kind, tool, is_error, body FROM events WHERE run_id = ?1 AND kind != 'thinking' ORDER BY id")?;
    let lines: Vec<String> = stmt
        .query_map([run_id], |r| {
            let (kind, tool, err, body): (String, Option<String>, i64, String) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            let body = llm::truncate(&body, 600);
            Ok(match kind.as_str() {
                "user" => format!("USER: {body}"),
                "assistant" => format!("AGENT: {body}"),
                "tool_call" => format!("CALL {}: {body}", tool.unwrap_or_default()),
                "tool_result" => {
                    format!("RESULT{}: {body}", if err != 0 { " (ERROR)" } else { "" })
                }
                "nap" => format!("[segment summary] {body}"),
                k => format!("{k}: {body}"),
            })
        })?
        .collect::<Result<_, _>>()?;
    let full = lines.join("\n");
    if full.len() <= budget {
        return Ok(full);
    }
    let head = llm::truncate(&full, budget / 3);
    let tail_start = full.len() - budget * 2 / 3;
    let tail_start = (tail_start..full.len())
        .find(|i| full.is_char_boundary(*i))
        .unwrap_or(full.len());
    Ok(format!("{head}\n…\n{}", &full[tail_start..]))
}

const REPLAY_SYSTEM: &str = "You review an AI agent's work run that was flagged as surprising, to extract lessons for next time. \
Return durable facts only: things that will still be true and useful later (a pitfall and its fix, a constraint, a configuration detail, a user preference revealed by a correction). \
Use stable lowercase entity names and short attribute names. Return an empty list if nothing qualifies.";

fn replay(conn: &Connection, llm: &dyn Llm, run_id: &str, why: &[String]) -> Result<usize> {
    let (summary, thread_id): (String, String) = conn.query_row(
        "SELECT COALESCE(summary,''), thread_id FROM runs WHERE id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let prompt = format!(
        "Flagged because: {}\nRun summary: {summary}\n\nTranscript:\n{}",
        why.join("; "),
        transcript(conn, run_id, 60_000)?
    );
    let v = llm::structured(
        llm,
        REPLAY_SYSTEM,
        &prompt,
        json!({"type": "object", "additionalProperties": false, "required": ["facts"],
        "properties": {"facts": {"type": "array", "items": fact_schema()}}}),
    )?;
    learn_all(conn, Some(run_id), &thread_id, &v["facts"])
}

// ---- skills ----

const SKILL_SYSTEM: &str = "You abstract a reusable procedure from several successful runs of the same kind of task by an AI agent. \
Write what to do, not why: concrete numbered steps, including the checks or traps the runs reveal. Keep it general enough for the next instance of this task type.";

fn extract_skills(conn: &Connection, llm: &dyn Llm, report: &mut Report) -> Result<()> {
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, task_type, COALESCE(summary,'') FROM runs
            WHERE status = 'consolidated' AND outcome = 'success' AND task_type NOT IN ('main','general') ORDER BY started_at")?;
        for r in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (id, tt, sum) = r?;
            groups.entry(tt).or_default().push((id, sum));
        }
    }
    let live: Vec<String> = memory::list_skills(conn)?
        .into_iter()
        .map(|s| s.name)
        .collect();
    for (task_type, runs) in groups {
        if runs.len() < SKILL_MIN_RUNS || live.contains(&task_type) {
            continue;
        }
        let mut prompt = format!("Task type: {task_type}\n\n");
        for (i, (rid, sum)) in runs.iter().rev().take(6).enumerate() {
            prompt.push_str(&format!(
                "Run {}: {sum}\nTool sequence: {}\n\n",
                i + 1,
                tool_sequence(conn, rid)?.join(" → ")
            ));
        }
        let v = llm::structured(
            llm,
            SKILL_SYSTEM,
            &prompt,
            json!({"type": "object", "additionalProperties": false,
            "required": ["when_to_use", "steps"], "properties": {"when_to_use": {"type": "string"}, "steps": {"type": "string"}}}),
        )?;
        let ids: Vec<String> = runs.iter().map(|(r, _)| r.clone()).collect();
        memory::save_skill(
            conn,
            &task_type,
            v["when_to_use"].as_str().unwrap_or(""),
            v["steps"].as_str().unwrap_or(""),
            &ids,
        )?;
        report.skills_new.push(task_type);
    }
    Ok(())
}

fn tool_sequence(conn: &Connection, run_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT kind, tool, is_error FROM events WHERE run_id = ?1 AND kind IN ('tool_call','tool_result') ORDER BY id")?;
    let mut seq: Vec<String> = vec![];
    for r in stmt.query_map([run_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (kind, tool, err) = r?;
        if kind == "tool_call" {
            seq.push(tool.unwrap_or_default());
        } else if err != 0
            && let Some(last) = seq.last_mut()
        {
            last.push_str("(failed)");
        }
    }
    Ok(seq)
}

// ---- driver ----

/// Atomically claim sealed runs so concurrent threads never consolidate the same one.
fn claim(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT id FROM runs WHERE status = 'sealed' ORDER BY started_at")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut claimed = vec![];
    for id in ids {
        if conn.execute(
            "UPDATE runs SET status = 'consolidating' WHERE id = ?1 AND status = 'sealed'",
            [&id],
        )? == 1
        {
            claimed.push(id);
        }
    }
    Ok(claimed)
}

pub fn pending(conn: &Connection) -> Result<usize> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM runs WHERE status = 'sealed'",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize)
}

pub fn consolidate(conn: &Connection, llm: &dyn Llm) -> Result<Report> {
    let mut report = Report::default();
    let mut scored = vec![];
    for rid in claim(conn)? {
        let s = signals(conn, &rid)?;
        if s.task_type == "main" {
            conn.execute(
                "UPDATE runs SET status = 'consolidated' WHERE id = ?1",
                [&rid],
            )?;
            continue;
        }
        let (score, why) = surprise(&s, &baseline(conn, &s.task_type, &rid)?);
        conn.execute(
            "UPDATE runs SET surprise = ?2 WHERE id = ?1",
            params![rid, score],
        )?;
        scored.push((rid, score, why));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (rid, score, why) in scored {
        if score >= REPLAY_ABOVE && report.replayed < MAX_REPLAYS {
            report.learned += replay(conn, llm, &rid, &why)?;
            report.replayed += 1;
        }
        conn.execute(
            "UPDATE runs SET status = 'consolidated' WHERE id = ?1",
            [&rid],
        )?;
        report.scored.push((rid, score));
    }
    extract_skills(conn, llm, &mut report)?;
    let (cold, retired) = memory::decay(conn, now())?;
    report.cold = cold;
    report.retired = retired;
    if !report.is_quiet() {
        audit(conn, "consolidate", "-", &format!("{report:?}"))?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::llm::Scripted;

    fn sealed_run(
        conn: &Connection,
        thread: &str,
        task: &str,
        outcome: &str,
        events: &[(&str, Option<&str>, bool, &str)],
    ) -> String {
        let rid = threads::start_run(conn, thread).unwrap();
        for (k, t, e, b) in events {
            threads::log_event(conn, &rid, k, *t, *e, b).unwrap();
        }
        threads::seal_run(conn, &rid, Some(outcome), task, &format!("did {task}")).unwrap();
        rid
    }

    fn setup() -> Connection {
        let c = db::open_memory();
        threads::ensure_main(&c, "/repo").unwrap();
        c
    }

    #[test]
    fn signals_count_retries_errors_and_corrections() {
        let c = setup();
        let rid = sealed_run(
            &c,
            "main",
            "x",
            "success",
            &[
                ("user", None, false, "analyze data.csv"),
                ("tool_call", Some("bash"), false, "{}"),
                ("tool_result", Some("bash"), true, "exit 1"),
                ("tool_call", Some("bash"), false, "{}"),
                ("tool_result", Some("bash"), false, "ok"),
                ("user", None, false, "no, use the cleaned file instead"),
            ],
        );
        let s = signals(&c, &rid).unwrap();
        assert_eq!(
            (s.tool_calls, s.tool_errors, s.retries, s.corrections),
            (2, 1, 1, 1)
        );
    }

    #[test]
    fn surprise_prefers_deviation_from_baseline() {
        let routine = Baseline {
            n: 10,
            success_rate: 0.9,
            mean_duration: 100.0,
            sd_duration: 20.0,
        };
        let ok = Signals {
            outcome: "success".into(),
            duration: 100,
            ..Default::default()
        };
        let failed = Signals {
            outcome: "failure".into(),
            duration: 400,
            retries: 3,
            ..Default::default()
        };
        assert!(surprise(&ok, &routine).0 < REPLAY_ABOVE);
        let (score, why) = surprise(&failed, &routine);
        assert!(score > 1.5);
        assert!(
            why.iter().any(|w| w.contains("retries")) && why.iter().any(|w| w.contains("took"))
        );
    }

    #[test]
    fn only_surprising_runs_replay_and_skills_form() {
        let c = setup();
        threads::create(&c, "csv", "csv work", "/repo", None, Some("main"), "").unwrap();
        // three routine successes, one surprising failure
        for _ in 0..3 {
            sealed_run(
                &c,
                "csv",
                "csv-analysis",
                "success",
                &[
                    ("tool_call", Some("bash"), false, "{}"),
                    ("tool_result", Some("bash"), false, "ok"),
                ],
            );
        }
        let bad = sealed_run(
            &c,
            "csv",
            "migration",
            "failure",
            &[
                ("tool_call", Some("bash"), false, "{}"),
                ("tool_result", Some("bash"), true, "auth schema mismatch"),
                ("tool_call", Some("bash"), false, "{}"),
                ("tool_result", Some("bash"), true, "auth schema mismatch"),
            ],
        );
        let llm = Scripted::new(vec![
            Scripted::json(
                json!({"facts": [{"entity": "supabase", "attribute": "migration pitfall", "value": "export auth schema separately"}]}),
            ),
            Scripted::json(
                json!({"when_to_use": "analyzing a CSV", "steps": "1. clean\n2. analyze"}),
            ),
        ]);
        let r = consolidate(&c, &llm).unwrap();
        assert_eq!(
            r.replayed, 1,
            "only the failed migration is surprising enough"
        );
        assert_eq!(r.scored[0].0, bad);
        assert_eq!(r.learned, 1);
        assert_eq!(r.skills_new, vec!["csv-analysis".to_string()]);
        assert_eq!(llm.requests.lock().unwrap().len(), 2);
        assert_eq!(pending(&c).unwrap(), 0);
        // nothing left: a second pass makes no calls
        let r2 = consolidate(&c, &Scripted::new(vec![])).unwrap();
        assert!(r2.is_quiet());
    }

    #[test]
    fn superseded_fact_notifies_other_threads() {
        let c = setup();
        threads::create(
            &c,
            "client-a",
            "onboard client A",
            "/repo",
            None,
            Some("main"),
            "",
        )
        .unwrap();
        threads::create(
            &c,
            "client-b",
            "migrate client B",
            "/repo",
            None,
            Some("main"),
            "",
        )
        .unwrap();
        let ra = threads::start_run(&c, "client-a").unwrap();
        let rb = threads::start_run(&c, "client-b").unwrap();
        learn(&c, Some(&ra), "client-a", "shared db", "host", "supabase").unwrap();
        learn(&c, Some(&rb), "client-b", "shared db", "host", "rds").unwrap();
        let mail = threads::take_mail(&c, "client-a").unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].0, "memory");
        assert!(mail[0].1.contains("\"supabase\" to \"rds\""));
        assert!(
            threads::take_mail(&c, "client-b").unwrap().is_empty(),
            "source thread is not notified"
        );
    }
}
