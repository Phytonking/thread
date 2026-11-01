//! Providers. Subscription logins are used by driving each vendor's official CLI
//! headlessly (Claude Code, Codex, Grok Build), so auth stays in those tools.
//! `api` is the fallback: any OpenAI-compatible endpoint with a key.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    Grok,
    Api,
}

pub const ALL: [Provider; 4] = [
    Provider::Claude,
    Provider::Codex,
    Provider::Grok,
    Provider::Api,
];

impl Provider {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "claude" | "anthropic" | "claude-code" => Provider::Claude,
            "codex" | "openai" | "chatgpt" => Provider::Codex,
            "grok" | "xai" => Provider::Grok,
            "api" | "key" | "anthropic-api" => Provider::Api,
            other => bail!("unknown provider '{other}' (claude, codex, grok, api)"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Grok => "grok",
            Provider::Api => "api",
        }
    }

    pub fn binary(self) -> Option<&'static str> {
        match self {
            Provider::Claude => Some("claude"),
            Provider::Codex => Some("codex"),
            Provider::Grok => Some("grok"),
            Provider::Api => None,
        }
    }

    pub fn installed(self) -> bool {
        match self.binary() {
            Some(b) => which(b).is_some(),
            None => true,
        }
    }

    /// (logged in, detail)
    pub fn status(self) -> (bool, String) {
        match self {
            Provider::Claude => {
                if !self.installed() {
                    return (
                        false,
                        "not installed — https://claude.com/claude-code".into(),
                    );
                }
                match Command::new("claude").args(["auth", "status"]).output() {
                    Ok(o) => {
                        let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
                        if v["loggedIn"] == true {
                            (
                                true,
                                format!("logged in ({})", v["authMethod"].as_str().unwrap_or("?")),
                            )
                        } else {
                            (false, "not logged in — thread login claude".into())
                        }
                    }
                    Err(e) => (false, e.to_string()),
                }
            }
            Provider::Codex => {
                if !self.installed() {
                    return (false, "not installed — npm i -g @openai/codex".into());
                }
                match Command::new("codex").args(["login", "status"]).output() {
                    Ok(o) => {
                        let text = format!(
                            "{}{}",
                            String::from_utf8_lossy(&o.stdout),
                            String::from_utf8_lossy(&o.stderr)
                        );
                        let line = text
                            .lines()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        (
                            o.status.success() && line.to_lowercase().contains("logged in"),
                            line,
                        )
                    }
                    Err(e) => (false, e.to_string()),
                }
            }
            Provider::Grok => {
                if !self.installed() {
                    return (
                        false,
                        "not installed — curl -fsSL https://x.ai/cli/install.sh | bash".into(),
                    );
                }
                let cached = home()
                    .map(|h| h.join(".grok").join("auth.json").exists())
                    .unwrap_or(false);
                if cached {
                    (true, "logged in (cached OAuth)".into())
                } else if std::env::var("XAI_API_KEY").is_ok() {
                    (
                        true,
                        "XAI_API_KEY set (billed to the API, not your subscription)".into(),
                    )
                } else {
                    (false, "not logged in — thread login grok".into())
                }
            }
            Provider::Api => {
                let (base, key) = api_config();
                match (key, is_local(&base)) {
                    (Some(_), _) => (true, format!("OpenAI-compatible: {base}")),
                    (None, true) => (true, format!("OpenAI-compatible: {base} (local, no key)")),
                    (None, false) => (
                        false,
                        "not set up — thread login api (any OpenAI-compatible endpoint)".into(),
                    ),
                }
            }
        }
    }

    /// Interactive login through the vendor's own CLI.
    pub fn login(self) -> Result<()> {
        let (bin, args): (&str, &[&str]) = match self {
            Provider::Claude => ("claude", &["auth", "login"]),
            Provider::Codex => ("codex", &["login"]),
            Provider::Grok => ("grok", &["login"]),
            Provider::Api => return login_api_key(),
        };
        if !self.installed() {
            bail!("{bin} is not installed: {}", self.status().1);
        }
        let st = Command::new(bin)
            .args(args)
            .status()
            .with_context(|| format!("running {bin}"))?;
        if !st.success() {
            bail!("{bin} login exited with {st}");
        }
        Ok(())
    }
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn config_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")))
        .context("no HOME")?;
    Ok(base.join("thread"))
}

fn read_json(name: &str) -> Value {
    config_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join(name)).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

fn write_json(name: &str, v: &Value, private: bool) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(v)?)?;
    if private {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// (base URL, key) for the OpenAI-compatible `api` provider.
/// Env wins: OPENAI_BASE_URL / OPENAI_API_KEY, then `thread login api`.
pub fn api_config() -> (String, Option<String>) {
    let creds = read_json("credentials.json");
    let base = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| creds["api_base_url"].as_str().map(String::from))
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            creds["api_key"]
                .as_str()
                .filter(|k| !k.is_empty())
                .map(String::from)
        });
    (base.trim_end_matches('/').to_string(), key)
}

pub fn is_local(base: &str) -> bool {
    ["://localhost", "://127.0.0.1", "://0.0.0.0", "://[::1]"]
        .iter()
        .any(|h| base.contains(h))
}

/// Model for api threads that haven't picked one: `thread login api`'s choice, else the server's first listed model.
pub fn api_default_model() -> Option<String> {
    std::env::var("THREAD_API_MODEL")
        .ok()
        .or_else(|| {
            read_json("credentials.json")["api_model"]
                .as_str()
                .map(String::from)
        })
        .or_else(|| models(Provider::Api, false).0.first().map(|m| m.id.clone()))
}

fn prompt_line(label: &str) -> Result<String> {
    eprint!("{label}");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn login_api_key() -> Result<()> {
    let (cur_base, _) = api_config();
    eprintln!(
        "Any OpenAI-compatible endpoint works, e.g. https://api.openai.com/v1, https://openrouter.ai/api/v1, https://api.x.ai/v1, http://localhost:11434/v1 (Ollama)."
    );
    let base = prompt_line(&format!("Base URL [{cur_base}]: "))?;
    let base = if base.is_empty() {
        cur_base
    } else {
        base.trim_end_matches('/').to_string()
    };
    let key = prompt_line(if is_local(&base) {
        "API key (blank for none): "
    } else {
        "API key: "
    })?;
    if key.is_empty() && !is_local(&base) {
        bail!("{base} needs an API key");
    }
    let mut creds = read_json("credentials.json");
    creds["api_base_url"] = json!(base);
    creds["api_key"] = json!(key);
    if let Some(m) = creds.as_object_mut() {
        m.remove("api_model");
    }
    write_json("credentials.json", &creds, true)?;
    let mut cache = read_json("models.json");
    if let Some(m) = cache.as_object_mut() {
        m.remove("api");
    }
    write_json("models.json", &cache, false)?;
    let (list, _, _) = models(Provider::Api, true);
    if list.is_empty() {
        bail!("saved, but {base}/models returned nothing; check the URL and key");
    }
    eprintln!("{} models available:", list.len());
    for (i, m) in list.iter().take(20).enumerate() {
        eprintln!("  {:>2}. {}", i + 1, m.id);
    }
    if list.len() > 20 {
        eprintln!(
            "  … and {} more (/model in a thread lists them all)",
            list.len() - 20
        );
    }
    let pick = prompt_line("Default model [1]: ")?;
    let id = resolve_in(
        Provider::Api,
        &list,
        true,
        if pick.is_empty() { "1" } else { &pick },
    )?;
    let mut creds = read_json("credentials.json");
    creds["api_model"] = json!(id);
    write_json("credentials.json", &creds, true)?;
    eprintln!("default api model: {id}");
    Ok(())
}

/// Tools the user said never to ask about again (`/always`), by name: `WebSearch`, `Bash`, `bash`…
pub fn always_allowed() -> Vec<String> {
    read_json("config.json")["always_allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

pub fn allow_always(tool: &str) -> Result<()> {
    let mut cfg = read_json("config.json");
    let mut list = always_allowed();
    if !list.iter().any(|t| t == tool) {
        list.push(tool.to_string());
    }
    cfg["always_allow"] = json!(list);
    write_json("config.json", &cfg, false)
}

/// The tool an approval request is about: the first word of its description.
pub fn tool_of(what: &str) -> &str {
    what.split_whitespace().next().unwrap_or("")
}

/// Show thinking steps in full (`/expand all`) instead of one line each.
pub fn thinking_expanded() -> bool {
    read_json("config.json")["thinking"] == "expanded"
}

pub fn set_thinking_expanded(on: bool) -> Result<()> {
    let mut cfg = read_json("config.json");
    cfg["thinking"] = json!(if on { "expanded" } else { "collapsed" });
    write_json("config.json", &cfg, false)
}

pub fn set_default(p: Provider) -> Result<()> {
    let mut cfg = read_json("config.json");
    cfg["provider"] = json!(p.name());
    write_json("config.json", &cfg, false)
}

/// --provider flag > $THREAD_PROVIDER > `thread use` > first logged-in CLI > api key.
pub fn resolve_default(flag: Option<&str>) -> Result<Provider> {
    if let Some(f) = flag {
        return Provider::parse(f);
    }
    if let Ok(e) = std::env::var("THREAD_PROVIDER") {
        return Provider::parse(&e);
    }
    if let Some(c) = read_json("config.json")["provider"].as_str() {
        return Provider::parse(c);
    }
    for p in [Provider::Claude, Provider::Codex, Provider::Grok] {
        if p.installed() && p.status().0 {
            return Ok(p);
        }
    }
    if Provider::Api.status().0 {
        return Ok(Provider::Api);
    }
    bail!(
        "no provider is logged in. Run `thread login claude` (or codex, grok), or `thread login api` for an API key"
    )
}

// ---- the user's MCP servers (`/mcp`), shared by every thread and provider ----

pub fn mcp_servers() -> Vec<(String, Value)> {
    read_json("mcp.json")["servers"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// `target` is a URL (http transport) or a command with arguments (stdio).
pub fn mcp_add(name: &str, target: &[String]) -> Result<()> {
    if name.is_empty()
        || name == "thread"
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("server names are letters, digits, - and _ (and not 'thread')");
    }
    let Some(first) = target.first() else {
        bail!("usage: /mcp add <name> <command> [args…] | <url>")
    };
    let spec = if first.starts_with("http://") || first.starts_with("https://") {
        json!({"url": first})
    } else {
        json!({"command": first, "args": &target[1..], "env": {}})
    };
    let mut cfg = read_json("mcp.json");
    cfg["servers"][name] = spec.clone();
    write_json("mcp.json", &cfg, false)?;
    // Grok has no per-call MCP flag, so register with it directly.
    if Provider::Grok.installed() {
        let mut c = Command::new("grok");
        c.args(["mcp", "add"]);
        if let Some(url) = spec["url"].as_str() {
            c.args(["--transport", "http", name, url]);
        } else {
            c.arg(name).arg("--").args(target);
        }
        c.output().ok();
    }
    Ok(())
}

pub fn mcp_remove(name: &str) -> Result<bool> {
    let mut cfg = read_json("mcp.json");
    let removed = cfg["servers"]
        .as_object_mut()
        .and_then(|m| m.remove(name))
        .is_some();
    write_json("mcp.json", &cfg, false)?;
    if Provider::Grok.installed() {
        Command::new("grok")
            .args(["mcp", "remove", name])
            .output()
            .ok();
    }
    Ok(removed)
}

// ---- models (pulled live from each provider, cached briefly) ----

#[derive(Clone)]
pub struct ModelOpt {
    pub id: String,
    pub note: String,
}

fn opt(id: &str, note: &str) -> ModelOpt {
    ModelOpt {
        id: id.into(),
        note: note.into(),
    }
}

const MODEL_TTL: i64 = 12 * 3600;
const CLAUDE_ALIASES: [(&str, &str); 4] = [
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku: fastest, lightest on usage"),
    ("fable", "Fable: most capable, highest usage"),
];

/// The model menu for a provider, whether it is authoritative (other names are
/// rejected), and when it was fetched. `refresh` skips the cache.
pub fn models(p: Provider, refresh: bool) -> (Vec<ModelOpt>, bool, i64) {
    let key = p.name();
    let cache = read_json("models.json");
    let cached = |max_age: i64| -> Option<(Vec<ModelOpt>, i64)> {
        let e = &cache[key];
        let at = e["fetched_at"].as_i64()?;
        if crate::db::now() - at > max_age {
            return None;
        }
        let list: Vec<ModelOpt> = e["list"]
            .as_array()?
            .iter()
            .filter_map(|m| Some(opt(m["id"].as_str()?, m["note"].as_str().unwrap_or(""))))
            .collect();
        (!list.is_empty()).then_some((list, at))
    };
    // claude and api: fetching costs a second or two, so use a fresh-enough cache first.
    if !refresh
        && matches!(p, Provider::Claude | Provider::Api)
        && let Some((list, at)) = cached(MODEL_TTL)
    {
        return (list, p != Provider::Claude, at);
    }
    let live = match p {
        Provider::Claude => claude_live(),
        Provider::Codex => codex_live(),
        Provider::Grok => grok_live(),
        Provider::Api => api_live(),
    };
    if !live.is_empty() {
        let mut cache = cache.clone();
        cache[key] = json!({"fetched_at": crate::db::now(), "list": live.iter().map(|m| json!({"id": m.id, "note": m.note})).collect::<Vec<_>>()});
        write_json("models.json", &cache, false).ok();
        return (live, p != Provider::Claude, crate::db::now());
    }
    // offline or the CLI is missing: any cache at all, then a last-resort list
    if let Some((list, at)) = cached(i64::MAX) {
        return (list, p != Provider::Claude, at);
    }
    match p {
        Provider::Claude => (
            CLAUDE_ALIASES.iter().map(|(a, n)| opt(a, n)).collect(),
            false,
            0,
        ),
        Provider::Api => (vec![], false, 0),
        _ => (vec![], false, 0),
    }
}

/// Ask Claude Code which model each alias currently resolves to. It reports the
/// model in its `init` event before any API call; we stop it right there.
fn claude_live() -> Vec<ModelOpt> {
    if !Provider::Claude.installed() {
        return vec![];
    }
    let resolved: Vec<Option<String>> = std::thread::scope(|sc| {
        let hs: Vec<_> = CLAUDE_ALIASES
            .iter()
            .map(|(alias, _)| sc.spawn(move || resolve_claude_alias(alias)))
            .collect();
        hs.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    CLAUDE_ALIASES
        .iter()
        .zip(resolved)
        .filter_map(|((alias, note), id)| {
            let id = id?;
            Some(opt(alias, &format!("{note} → {id}")))
        })
        .collect()
}

fn resolve_claude_alias(alias: &str) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new("claude")
        .args([
            "-p",
            "--model",
            alias,
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--setting-sources",
            "project",
            "--tools",
            "",
        ])
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(b"x").ok();
    let mut found = None;
    for line in BufReader::new(child.stdout.take()?)
        .lines()
        .map_while(|l| l.ok())
    {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["type"] == "system" && v["subtype"] == "init" {
            found = v["model"].as_str().map(String::from);
            break;
        }
        if v["type"] == "result" {
            break;
        }
    }
    child.kill().ok();
    child.wait().ok();
    found
}

/// `codex debug models` refreshes the catalog for your account; fall back to Codex's own cache file.
fn codex_live() -> Vec<ModelOpt> {
    let catalog = Command::new("codex")
        .args(["debug", "models"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .or_else(|| {
            home()
                .and_then(|h| std::fs::read_to_string(h.join(".codex/models_cache.json")).ok())
                .and_then(|s| serde_json::from_str(&s).ok())
        });
    let Some(v) = catalog else { return vec![] };
    let mut ms: Vec<&Value> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .collect();
    ms.sort_by_key(|m| m["priority"].as_i64().unwrap_or(i64::MAX));
    ms.into_iter()
        .filter_map(|m| {
            Some(opt(
                m["slug"].as_str()?,
                m["display_name"].as_str().unwrap_or(""),
            ))
        })
        .collect()
}

fn grok_live() -> Vec<ModelOpt> {
    Command::new("grok")
        .arg("models")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .map(|w| w.trim_start_matches(['*', '-', '•']))
                .filter(|w| w.starts_with("grok"))
                .map(|w| opt(w, ""))
                .collect()
        })
        .unwrap_or_default()
}

/// `GET {base}/models`, the OpenAI-compatible model list, newest first.
fn api_live() -> Vec<ModelOpt> {
    let (base, key) = api_config();
    if key.is_none() && !is_local(&base) {
        return vec![];
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let mut req = agent.get(&format!("{base}/models"));
    if let Some(k) = &key {
        req = req.header("authorization", &format!("Bearer {k}"));
    }
    let Ok(mut resp) = req.call() else {
        return vec![];
    };
    let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
    let mut ms: Vec<&Value> = v["data"]
        .as_array()
        .or(v["models"].as_array())
        .into_iter()
        .flatten()
        .collect();
    ms.sort_by(|a, b| b["created"].as_i64().cmp(&a["created"].as_i64()));
    ms.into_iter()
        .filter_map(|m| {
            let id = m["id"].as_str().or(m["name"].as_str())?;
            Some(opt(
                id,
                m["name"]
                    .as_str()
                    .filter(|n| *n != id)
                    .or(m["owned_by"].as_str())
                    .unwrap_or(""),
            ))
        })
        .collect()
}

/// Turn what the user typed into a model id: a menu number, an exact id, or a unique partial match.
pub fn resolve_model(p: Provider, input: &str) -> Result<String> {
    let (list, strict, _) = models(p, false);
    resolve_in(p, &list, strict, input)
}

fn resolve_in(p: Provider, list: &[ModelOpt], strict: bool, input: &str) -> Result<String> {
    let q = input.trim();
    if let Ok(n) = q.parse::<usize>() {
        return list
            .get(n.wrapping_sub(1))
            .map(|m| m.id.clone())
            .ok_or_else(|| anyhow::anyhow!("no option {n}; /model to see the list"));
    }
    if let Some(m) = list.iter().find(|m| m.id.eq_ignore_ascii_case(q)) {
        return Ok(m.id.clone());
    }
    let lq = q.to_lowercase();
    let hits: Vec<&ModelOpt> = list
        .iter()
        .filter(|m| m.id.to_lowercase().contains(&lq) || m.note.to_lowercase().contains(&lq))
        .collect();
    match hits.len() {
        1 => return Ok(hits[0].id.clone()),
        n if n > 1 => bail!(
            "'{q}' matches {}: pick one with /model <number>",
            hits.iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => {}
    }
    let looks_valid = p == Provider::Claude && lq.starts_with("claude-");
    if strict || (!looks_valid && !list.is_empty()) {
        bail!(
            "{} doesn't offer '{q}'. Options: {}",
            p.name(),
            list.iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(q.to_string())
}

/// Random v4 UUID (session ids for claude and grok).
pub fn uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        f.read_exact(&mut b).ok();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_resolution() {
        let claude = vec![
            opt("opus", "Opus → claude-opus-5-5"),
            opt("sonnet", "Sonnet → claude-sonnet-5-5"),
            opt("haiku", "Haiku → claude-haiku-5-5"),
            opt("fable", "Fable → claude-fable-5-1"),
        ];
        assert_eq!(
            resolve_in(Provider::Claude, &claude, false, "1").unwrap(),
            "opus"
        );
        assert_eq!(
            resolve_in(Provider::Claude, &claude, false, "HAIKU").unwrap(),
            "haiku"
        );
        assert_eq!(
            resolve_in(Provider::Claude, &claude, false, "fable-5-1").unwrap(),
            "fable",
            "matches the resolved id too"
        );
        assert_eq!(
            resolve_in(Provider::Claude, &claude, false, "claude-opus-4-8").unwrap(),
            "claude-opus-4-8"
        );
        assert!(resolve_in(Provider::Claude, &claude, false, "luna").is_err());
        assert!(resolve_in(Provider::Claude, &claude, false, "9").is_err());
        let codex = vec![
            opt("gpt-6-luna", "GPT-6-Luna"),
            opt("gpt-5.6-terra", "GPT-5.6-Terra"),
            opt("gpt-5.6-luna", "GPT-5.6-Luna"),
        ];
        assert!(
            resolve_in(Provider::Codex, &codex, true, "luna")
                .unwrap_err()
                .to_string()
                .contains("matches")
        );
        assert_eq!(
            resolve_in(Provider::Codex, &codex, true, "terra").unwrap(),
            "gpt-5.6-terra"
        );
        assert!(resolve_in(Provider::Codex, &codex, true, "haiku").is_err());
    }

    #[test]
    fn parse_and_uuid() {
        assert_eq!(Provider::parse("ChatGPT").unwrap(), Provider::Codex);
        assert_eq!(Provider::parse("xai").unwrap(), Provider::Grok);
        assert!(Provider::parse("gemini").is_err());
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, uuid());
    }
}
