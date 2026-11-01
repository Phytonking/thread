//! Semantic (facts) and procedural (skills) stores, plus recall.
//!
//! Policies, straight from the AgentSleep design:
//! - Facts start tentative and promote to active once corroborated by a second
//!   independent run. Nothing is deleted: contradicted facts are superseded,
//!   idle ones go cold, both stay queryable for audit.
//! - A single run cannot overturn a well-supported fact; the challenger waits
//!   as a tentative conflict until it is corroborated too (poisoning resistance).
//! - Skills start tentative and promote only after successful application.

use crate::db::{self, audit, now};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::json;

pub const PROMOTE_SUPPORT: i64 = 2;
pub const SKILL_PROMOTE_SUCCESSES: i64 = 2;
const DAY: i64 = 86_400;

#[derive(Debug, PartialEq)]
pub enum FactOutcome {
    New(i64),
    Corroborated { id: i64, promoted: bool },
    Superseded { old: i64, old_value: String, new: i64 },
    /// New value contradicts a well-supported fact; held as tentative.
    Contested { incumbent: i64, challenger: i64 },
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub id: i64,
    pub entity: String,
    pub attribute: String,
    pub value: String,
    pub status: String,
    pub support: i64,
    pub runs: Vec<String>,
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn row_to_fact(r: &rusqlite::Row) -> rusqlite::Result<Fact> {
    let runs: String = r.get(6)?;
    Ok(Fact {
        id: r.get(0)?, entity: r.get(1)?, attribute: r.get(2)?, value: r.get(3)?,
        status: r.get(4)?, support: r.get(5)?,
        runs: serde_json::from_str(&runs).unwrap_or_default(),
    })
}
const FACT_COLS: &str = "id, entity, attribute, value, status, support, runs";

pub fn get_fact(conn: &Connection, id: i64) -> Result<Fact> {
    Ok(conn.query_row(&format!("SELECT {FACT_COLS} FROM facts WHERE id = ?1"), [id], row_to_fact)?)
}

fn live_facts_for_key(conn: &Connection, entity: &str, attribute: &str) -> Result<Vec<Fact>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FACT_COLS} FROM facts WHERE entity = ?1 AND attribute = ?2 AND status IN ('active','tentative') ORDER BY support DESC, id DESC"))?;
    let rows = stmt.query_map(params![entity, attribute], row_to_fact)?.collect::<Result<_, _>>()?;
    Ok(rows)
}

fn insert_fact(conn: &Connection, entity: &str, attribute: &str, value: &str, status: &str, run: Option<&str>) -> Result<i64> {
    let runs = json!(run.into_iter().collect::<Vec<_>>()).to_string();
    let t = now();
    conn.execute(
        "INSERT INTO facts (entity, attribute, value, status, runs, created_at, last_access) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![entity, attribute, value, status, runs, t],
    )?;
    let id = conn.last_insert_rowid();
    db::index(conn, "fact", &id.to_string(), &format!("{entity} {attribute}: {value}"))?;
    Ok(id)
}

fn supersede(conn: &Connection, old: i64, new: i64) -> Result<()> {
    conn.execute("UPDATE facts SET status = 'superseded', valid_to = ?2, replaced_by = ?3 WHERE id = ?1", params![old, now(), new])?;
    conn.execute("UPDATE facts SET status = 'active' WHERE id = ?1", [new])?;
    db::unindex(conn, "fact", &old.to_string())?;
    audit(conn, "fact.supersede", &old.to_string(), &format!("replaced by {new}"))
}

/// Assert entity.attribute = value. `run` None means the user asserted it
/// directly, which is trusted and goes straight to active.
pub fn assert_fact(conn: &Connection, entity: &str, attribute: &str, value: &str, run: Option<&str>) -> Result<FactOutcome> {
    let (entity, attribute) = (norm(entity), norm(attribute));
    let value = value.trim();
    let live = live_facts_for_key(conn, &entity, &attribute)?;

    // Same value already known: corroborate.
    if let Some(f) = live.iter().find(|f| norm(&f.value) == norm(value)) {
        let mut runs = f.runs.clone();
        let fresh = match run { Some(s) if !runs.iter().any(|x| x == s) => { runs.push(s.into()); true } _ => false };
        let support = if fresh { f.support + 1 } else { f.support };
        let promote = f.status == "tentative" && (support >= PROMOTE_SUPPORT || run.is_none());
        conn.execute("UPDATE facts SET support = ?2, runs = ?3, last_access = ?4 WHERE id = ?1",
            params![f.id, support, json!(runs).to_string(), now()])?;
        if promote {
            // A corroborated challenger now overturns whatever it was contesting.
            for other in live.iter().filter(|o| o.id != f.id && o.status == "active") {
                supersede(conn, other.id, f.id)?;
            }
            conn.execute("UPDATE facts SET status = 'active' WHERE id = ?1", [f.id])?;
            audit(conn, "fact.promote", &f.id.to_string(), &format!("support={support}"))?;
        }
        return Ok(FactOutcome::Corroborated { id: f.id, promoted: promote });
    }

    let incumbent = live.iter().find(|f| f.status == "active").or(live.first());
    match incumbent {
        None => {
            let status = if run.is_none() { "active" } else { "tentative" };
            let id = insert_fact(conn, &entity, &attribute, value, status, run)?;
            audit(conn, "fact.new", &id.to_string(), &format!("{entity}.{attribute} = {value} ({status})"))?;
            Ok(FactOutcome::New(id))
        }
        Some(old) if old.support >= PROMOTE_SUPPORT && run.is_some() => {
            let id = insert_fact(conn, &entity, &attribute, value, "tentative", run)?;
            audit(conn, "fact.contest", &old.id.to_string(), &format!("challenger {id}: {value}"))?;
            Ok(FactOutcome::Contested { incumbent: old.id, challenger: id })
        }
        Some(old) => {
            let id = insert_fact(conn, &entity, &attribute, value, "tentative", run)?;
            supersede(conn, old.id, id)?;
            Ok(FactOutcome::Superseded { old: old.id, old_value: old.value.clone(), new: id })
        }
    }
}

pub fn list_facts(conn: &Connection, all: bool) -> Result<Vec<Fact>> {
    let filter = if all { "" } else { "WHERE status IN ('active','tentative')" };
    let mut stmt = conn.prepare(&format!("SELECT {FACT_COLS} FROM facts {filter} ORDER BY entity, attribute, id"))?;
    let rows = stmt.query_map([], row_to_fact)?.collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Threads whose runs asserted this fact.
pub fn threads_of_fact(conn: &Connection, f: &Fact) -> Result<Vec<String>> {
    let mut out = vec![];
    for r in &f.runs {
        if let Some(t) = crate::threads::thread_of_run(conn, r)? {
            if !out.contains(&t) { out.push(t); }
        }
    }
    Ok(out)
}

// ---- skills ----

#[derive(Debug, Clone)]
pub struct Skill {
    pub id: i64,
    pub name: String,
    pub version: i64,
    pub when_to_use: String,
    pub steps: String,
    pub status: String,
    pub uses: i64,
    pub successes: i64,
}

fn row_to_skill(r: &rusqlite::Row) -> rusqlite::Result<Skill> {
    Ok(Skill { id: r.get(0)?, name: r.get(1)?, version: r.get(2)?, when_to_use: r.get(3)?, steps: r.get(4)?,
        status: r.get(5)?, uses: r.get(6)?, successes: r.get(7)? })
}
const SKILL_COLS: &str = "id, name, version, when_to_use, steps, status, uses, successes";

pub fn save_skill(conn: &Connection, name: &str, when_to_use: &str, steps: &str, runs: &[String]) -> Result<i64> {
    let version: i64 = conn.query_row("SELECT COALESCE(MAX(version), 0) + 1 FROM skills WHERE name = ?1", [name], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO skills (name, version, when_to_use, steps, runs, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![name, version, when_to_use, steps, json!(runs).to_string(), now()],
    )?;
    let id = conn.last_insert_rowid();
    // Newer version replaces older ones in recall; older rows are kept.
    let mut stmt = conn.prepare("SELECT id FROM skills WHERE name = ?1 AND id != ?2 AND status != 'retired'")?;
    for old in stmt.query_map(params![name, id], |r| r.get::<_, i64>(0))? {
        let old = old?;
        conn.execute("UPDATE skills SET status = 'retired' WHERE id = ?1", [old])?;
        db::unindex(conn, "skill", &old.to_string())?;
    }
    db::index(conn, "skill", &id.to_string(), &format!("{name}: {when_to_use}"))?;
    audit(conn, "skill.new", &id.to_string(), &format!("{name} v{version} (tentative)"))?;
    Ok(id)
}

pub fn get_skill(conn: &Connection, id: i64) -> Result<Skill> {
    Ok(conn.query_row(&format!("SELECT {SKILL_COLS} FROM skills WHERE id = ?1"), [id], row_to_skill)?)
}

pub fn list_skills(conn: &Connection) -> Result<Vec<Skill>> {
    let mut stmt = conn.prepare(&format!("SELECT {SKILL_COLS} FROM skills WHERE status != 'retired' ORDER BY name"))?;
    let rows = stmt.query_map([], row_to_skill)?.collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Feed a run outcome back into each skill that was offered in it.
pub fn record_skill_outcome(conn: &Connection, skill_id: i64, success: bool) -> Result<()> {
    conn.execute("UPDATE skills SET uses = uses + 1, successes = successes + ?2, last_used = ?3 WHERE id = ?1",
        params![skill_id, success as i64, now()])?;
    let s = get_skill(conn, skill_id)?;
    if s.status == "tentative" && s.successes >= SKILL_PROMOTE_SUCCESSES {
        conn.execute("UPDATE skills SET status = 'promoted' WHERE id = ?1", [skill_id])?;
        audit(conn, "skill.promote", &skill_id.to_string(), &format!("{}/{} successes", s.successes, s.uses))?;
    } else if s.uses >= 3 && (s.successes as f64) / (s.uses as f64) < 0.34 {
        conn.execute("UPDATE skills SET status = 'retired' WHERE id = ?1", [skill_id])?;
        db::unindex(conn, "skill", &skill_id.to_string())?;
        audit(conn, "skill.retire", &skill_id.to_string(), "low success rate")?;
    }
    Ok(())
}

// ---- recall ----

#[derive(Debug, Clone)]
pub struct Hit {
    pub kind: String,
    pub r#ref: String,
    pub text: String,
}

const STOP: &[&str] = &["the", "and", "for", "with", "that", "this", "from", "have", "are", "was", "you", "what", "how", "can", "into", "about", "then", "them", "they", "will", "would", "should", "could", "just", "like", "need", "want", "let", "lets", "please"];

pub fn fts_query(text: &str) -> Option<String> {
    let mut terms: Vec<String> = vec![];
    for w in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let w = w.to_lowercase();
        if w.len() >= 3 && !STOP.contains(&w.as_str()) && !terms.contains(&w) {
            terms.push(w);
        }
    }
    if terms.is_empty() { return None; }
    Some(terms.iter().take(24).map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" OR "))
}

pub fn recall(conn: &Connection, query: &str, kinds: &[&str], limit: usize) -> Result<Vec<Hit>> {
    let Some(q) = fts_query(query) else { return Ok(vec![]) };
    let mut stmt = conn.prepare("SELECT kind, ref, text FROM recall_fts WHERE recall_fts MATCH ?1 ORDER BY bm25(recall_fts) LIMIT ?2")?;
    let hits: Vec<Hit> = stmt.query_map(params![q, (limit * 4) as i64], |r| Ok(Hit { kind: r.get(0)?, r#ref: r.get(1)?, text: r.get(2)? }))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter().filter(|h| kinds.is_empty() || kinds.contains(&h.kind.as_str())).take(limit).collect();
    let t = now();
    for h in &hits {
        if h.kind == "fact" {
            conn.execute("UPDATE facts SET access_count = access_count + 1, last_access = ?2 WHERE id = ?1", params![h.r#ref, t])?;
        }
    }
    Ok(hits)
}

/// Render recall hits as prompt text, flagging tentative facts.
pub fn render_hits(conn: &Connection, hits: &[Hit]) -> Result<String> {
    let mut out = String::new();
    for h in hits {
        let tag = match h.kind.as_str() {
            "fact" => {
                let f = get_fact(conn, h.r#ref.parse()?)?;
                if f.status == "tentative" { "fact, tentative".to_string() } else { "fact".into() }
            }
            k => k.to_string(),
        };
        out.push_str(&format!("- ({tag}) {}\n", h.text.replace('\n', " ")));
    }
    Ok(out)
}

// ---- tiering (sleep's forgetting) ----

/// Move idle, weakly supported memories to cold storage. Returns (facts, skills) changed.
pub fn decay(conn: &Connection, at: i64) -> Result<(usize, usize)> {
    let mut cold = vec![];
    {
        let mut stmt = conn.prepare(
            "SELECT id FROM facts WHERE
               (status = 'tentative' AND support < 2 AND last_access < ?1)
            OR (status = 'active' AND support < 3 AND access_count < 2 AND last_access < ?2)")?;
        for id in stmt.query_map(params![at - 14 * DAY, at - 45 * DAY], |r| r.get::<_, i64>(0))? { cold.push(id?); }
    }
    for id in &cold {
        conn.execute("UPDATE facts SET status = 'cold' WHERE id = ?1", [id])?;
        db::unindex(conn, "fact", &id.to_string())?;
        audit(conn, "fact.cold", &id.to_string(), "idle")?;
    }
    let mut retired = vec![];
    {
        let mut stmt = conn.prepare("SELECT id FROM skills WHERE status = 'tentative' AND uses = 0 AND created_at < ?1")?;
        for id in stmt.query_map([at - 30 * DAY], |r| r.get::<_, i64>(0))? { retired.push(id?); }
    }
    for id in &retired {
        conn.execute("UPDATE skills SET status = 'retired' WHERE id = ?1", [id])?;
        db::unindex(conn, "skill", &id.to_string())?;
        audit(conn, "skill.retire", &id.to_string(), "never applied")?;
    }
    Ok((cold.len(), retired.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tentative_until_corroborated() {
        let c = db::open_memory();
        let FactOutcome::New(id) = assert_fact(&c, "Client A", "database", "Supabase", Some("s1")).unwrap() else { panic!() };
        assert_eq!(get_fact(&c, id).unwrap().status, "tentative");
        // same run again does not count as corroboration
        assert_eq!(assert_fact(&c, "client a", "Database", "supabase", Some("s1")).unwrap(), FactOutcome::Corroborated { id, promoted: false });
        assert_eq!(assert_fact(&c, "client a", "database", "Supabase", Some("s2")).unwrap(), FactOutcome::Corroborated { id, promoted: true });
        assert_eq!(get_fact(&c, id).unwrap().status, "active");
    }

    #[test]
    fn weak_fact_is_superseded_but_kept() {
        let c = db::open_memory();
        let FactOutcome::New(old) = assert_fact(&c, "client a", "database", "Supabase", Some("s1")).unwrap() else { panic!() };
        let FactOutcome::Superseded { new, .. } = assert_fact(&c, "client a", "database", "Postgres on RDS", Some("s2")).unwrap() else { panic!() };
        assert_eq!(get_fact(&c, old).unwrap().status, "superseded");
        assert_eq!(get_fact(&c, new).unwrap().status, "active");
        let hits = recall(&c, "client database", &["fact"], 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("RDS"));
    }

    #[test]
    fn strong_fact_resists_single_session_then_yields_to_corroboration() {
        let c = db::open_memory();
        assert_fact(&c, "api", "auth", "jwt", Some("s1")).unwrap();
        let FactOutcome::Corroborated { id: inc, promoted: true } = assert_fact(&c, "api", "auth", "jwt", Some("s2")).unwrap() else { panic!() };
        let FactOutcome::Contested { challenger, .. } = assert_fact(&c, "api", "auth", "run cookies", Some("s3")).unwrap() else { panic!() };
        assert_eq!(get_fact(&c, inc).unwrap().status, "active");
        assert_eq!(get_fact(&c, challenger).unwrap().status, "tentative");
        assert_eq!(assert_fact(&c, "api", "auth", "run cookies", Some("s4")).unwrap(), FactOutcome::Corroborated { id: challenger, promoted: true });
        assert_eq!(get_fact(&c, inc).unwrap().status, "superseded");
    }

    #[test]
    fn user_assertion_is_trusted() {
        let c = db::open_memory();
        let FactOutcome::New(id) = assert_fact(&c, "me", "editor", "helix", None).unwrap() else { panic!() };
        assert_eq!(get_fact(&c, id).unwrap().status, "active");
    }

    #[test]
    fn skill_promotes_after_successes_and_versions() {
        let c = db::open_memory();
        let id = save_skill(&c, "csv-analysis", "analyzing a CSV", "1. clean\n2. analyze", &[]).unwrap();
        record_skill_outcome(&c, id, true).unwrap();
        assert_eq!(get_skill(&c, id).unwrap().status, "tentative");
        record_skill_outcome(&c, id, true).unwrap();
        assert_eq!(get_skill(&c, id).unwrap().status, "promoted");
        let v2 = save_skill(&c, "csv-analysis", "analyzing a CSV", "1. profile\n2. clean\n3. analyze", &[]).unwrap();
        assert_eq!(get_skill(&c, v2).unwrap().version, 2);
        assert_eq!(get_skill(&c, id).unwrap().status, "retired");
        assert_eq!(list_skills(&c).unwrap().len(), 1);
    }

    #[test]
    fn decay_moves_idle_tentative_to_cold() {
        let c = db::open_memory();
        let FactOutcome::New(id) = assert_fact(&c, "x", "y", "z", Some("s1")).unwrap() else { panic!() };
        assert_eq!(decay(&c, now()).unwrap(), (0, 0));
        assert_eq!(decay(&c, now() + 15 * DAY).unwrap(), (1, 0));
        assert_eq!(get_fact(&c, id).unwrap().status, "cold");
        assert!(recall(&c, "x y z", &[], 5).unwrap().is_empty());
    }

    #[test]
    fn fts_query_sanitizes() {
        assert_eq!(fts_query("the a"), None);
        assert_eq!(fts_query("fix \"auth\" bug; auth!").unwrap(), "\"fix\" OR \"auth\" OR \"bug\"");
    }
}
