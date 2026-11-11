//! Providers. Subscription logins are used by driving each vendor's official CLI
//! headlessly (Claude Code, Codex, Grok Build, fx), so auth stays in those tools.
//! `api` is the fallback: any OpenAI-compatible endpoint with a key.
//! `ollama`, `fm` (Apple Foundation Models) and `local` (LM Studio, llama.cpp, vLLM…)
//! run on this machine through the same in-process agent loop as `api`, no key needed.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
    Grok,
    Fx,
    Api,
    Ollama,
    Fm,
    Local,
}

pub const ALL: [Provider; 8] = [
    Provider::Claude,
    Provider::Codex,
    Provider::Grok,
    Provider::Fx,
    Provider::Api,
    Provider::Ollama,
    Provider::Fm,
    Provider::Local,
];

/// Names accepted wherever a provider is chosen (tool enums, help text).
pub const NAMES: &str = "claude, codex, grok, fx, api, ollama, fm, local";

impl Provider {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "claude" | "anthropic" | "claude-code" => Provider::Claude,
            "codex" | "openai" | "chatgpt" => Provider::Codex,
            "grok" | "xai" => Provider::Grok,
            "fx" | "fx.sh" | "fxsh" => Provider::Fx,
            "api" | "key" | "anthropic-api" => Provider::Api,
            "ollama" => Provider::Ollama,
            "fm" | "apple" | "afm" | "foundation-models" => Provider::Fm,
            "local" | "lmstudio" | "lm-studio" | "llama.cpp" | "llamacpp" | "llama-server"
            | "vllm" | "openai-compat" => Provider::Local,
            other => bail!("unknown provider '{other}' ({NAMES})"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Grok => "grok",
            Provider::Fx => "fx",
            Provider::Api => "api",
            Provider::Ollama => "ollama",
            Provider::Fm => "fm",
            Provider::Local => "local",
        }
    }

    /// The vendor CLI a provider drives; `None` for the in-process ones.
    pub fn binary(self) -> Option<&'static str> {
        match self {
            Provider::Claude => Some("claude"),
            Provider::Codex => Some("codex"),
            Provider::Grok => Some("grok"),
            Provider::Fx => Some("fx"),
            _ => None,
        }
    }

    /// Runs the harness's own agent loop over an OpenAI-compatible (or Ollama) endpoint.
    pub fn in_process(self) -> bool {
        self.binary().is_none()
    }

    /// A model served on this machine (or the user's own server): no key, no usage billing.
    pub fn is_local_model(self) -> bool {
        matches!(self, Provider::Ollama | Provider::Fm | Provider::Local)
    }

    pub fn installed(self) -> bool {
        match self {
            Provider::Fm => which("fm").is_some(),
            _ => match self.binary() {
                Some(b) => which(b).is_some(),
                None => true,
            },
        }
    }

    /// Context window to plan around, in tokens, when it is small enough to matter.
    /// `THREAD_CONTEXT_WINDOW` overrides it (used for the local providers).
    pub fn context_window(self) -> Option<u64> {
        if let Some(n) = std::env::var("THREAD_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            return Some(n);
        }
        match self {
            Provider::Fm => Some(FM_CONTEXT),
            Provider::Ollama => Some(ollama_num_ctx()),
            _ => None,
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
            Provider::Fx => {
                if !self.installed() {
                    return (false, "not installed — https://fx.sh/docs".into());
                }
                match Command::new("fx").arg("status").output() {
                    Ok(o) => fx_auth_status(&format!(
                        "{}\n{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )),
                    Err(e) => (false, e.to_string()),
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
            Provider::Ollama => ollama_status(),
            Provider::Fm => fm_status(),
            Provider::Local => match local_base() {
                None => (
                    false,
                    format!(
                        "no server found ({}) — start one, or set THREAD_LOCAL_URL / thread login local",
                        LOCAL_SERVERS
                            .iter()
                            .map(|(n, u)| format!("{n} {}", u.trim_end_matches("/v1")))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                Some(base) if reachable(&format!("{base}/models")) => {
                    (true, format!("OpenAI-compatible server at {base}"))
                }
                Some(base) => (false, format!("{base} is not reachable")),
            },
        }
    }

    /// (base URL, key) for in-process providers. Ollama's is the server root (native API).
    pub fn endpoint(self) -> Option<(String, Option<String>)> {
        match self {
            Provider::Api => Some(api_config()),
            Provider::Ollama => Some((ollama_host(), None)),
            Provider::Fm => Some((fm_base(), None)),
            Provider::Local => local_base().map(|b| {
                let key = std::env::var("THREAD_LOCAL_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty());
                (b, key)
            }),
            _ => None,
        }
    }

    /// Interactive login through the vendor's own CLI.
    pub fn login(self) -> Result<()> {
        let (bin, args): (&str, &[&str]) = match self {
            Provider::Claude => ("claude", &["auth", "login"]),
            Provider::Codex => ("codex", &["login"]),
            Provider::Grok => ("grok", &["login"]),
            Provider::Fx => ("fx", &["login"]),
            Provider::Api => return login_api_key(),
            Provider::Ollama => {
                if which("ollama").is_none() && !reachable(&format!("{}/api/tags", ollama_host())) {
                    bail!("Ollama is not installed: https://ollama.com/download");
                }
                eprintln!(
                    "Ollama needs no login. Start it (`ollama serve` or the app), pull a model with tool calling (e.g. `ollama pull qwen3`), then `thread use ollama`."
                );
                return Ok(());
            }
            Provider::Fm => {
                if !self.installed() {
                    bail!("{}", fm_status().1);
                }
                // Apple's terms must be accepted once, interactively.
                Command::new("fm").arg("license").status().ok();
                return Ok(());
            }
            Provider::Local => return login_local(),
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

// ---- local model servers: ollama, fm, local ----

/// Apple's on-device model has a small window (measured ~8k tokens on AFM 3).
const FM_CONTEXT: u64 = 8192;
const FM_DEFAULT_URL: &str = "http://127.0.0.1:1976/v1";
/// Ollama's own default context is 4096 tokens and it truncates silently, which cuts off
/// the system prompt and tools. We ask for this much instead (`THREAD_OLLAMA_NUM_CTX`).
const OLLAMA_NUM_CTX: u64 = 32768;
/// Well-known OpenAI-compatible servers, probed in order when `local` has no URL set.
pub const LOCAL_SERVERS: [(&str, &str); 3] = [
    ("LM Studio", "http://127.0.0.1:1234/v1"),
    ("llama.cpp", "http://127.0.0.1:8080/v1"),
    ("vLLM", "http://127.0.0.1:8000/v1"),
];

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn get_json(url: &str, timeout_ms: u64) -> Option<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
        .build()
        .into();
    let mut resp = agent.get(url).call().ok()?;
    resp.body_mut().read_json().ok()
}

/// Something answers `GET url` with JSON, quickly.
pub fn reachable(url: &str) -> bool {
    get_json(url, 1500).is_some()
}

/// `OLLAMA_HOST` as Ollama itself reads it (`host`, `host:port` or a URL), as a base URL.
pub fn ollama_host() -> String {
    normalize_host(
        &env_nonempty("OLLAMA_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        11434,
    )
}

fn normalize_host(raw: &str, default_port: u16) -> String {
    let raw = raw.trim().trim_end_matches('/');
    let (scheme, rest) = raw.split_once("://").unwrap_or(("http", raw));
    // the bind-all address is how servers listen, not where clients connect
    let rest = rest.replace("0.0.0.0", "127.0.0.1");
    let has_port = rest
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if has_port || scheme == "https" {
        format!("{scheme}://{rest}")
    } else {
        format!("{scheme}://{rest}:{default_port}")
    }
}

pub fn ollama_num_ctx() -> u64 {
    env_nonempty("THREAD_OLLAMA_NUM_CTX")
        .and_then(|v| v.parse().ok())
        .unwrap_or(OLLAMA_NUM_CTX)
}

fn ollama_status() -> (bool, String) {
    let host = ollama_host();
    match get_json(&format!("{host}/api/tags"), 2000) {
        Some(v) => {
            let ms = v["models"].as_array().cloned().unwrap_or_default();
            let tools = ms.iter().filter(|m| ollama_has_tools(m)).count();
            if ms.is_empty() {
                (
                    false,
                    format!("running at {host}, but no models — ollama pull qwen3"),
                )
            } else {
                (
                    true,
                    format!(
                        "running at {host}: {} models, {tools} with tool calling",
                        ms.len()
                    ),
                )
            }
        }
        None if which("ollama").is_some() => (
            false,
            format!("installed but not running at {host} — ollama serve (or open the app)"),
        ),
        None => (false, "not installed — https://ollama.com/download".into()),
    }
}

/// Older Ollama versions don't report capabilities; then let the server decide.
fn ollama_has_tools(m: &Value) -> bool {
    m["capabilities"]
        .as_array()
        .is_none_or(|c| c.iter().any(|x| x == "tools"))
}

/// `GET /api/tags`: installed models, most recently pulled first, noting tool support.
fn ollama_live() -> Vec<ModelOpt> {
    let Some(v) = get_json(&format!("{}/api/tags", ollama_host()), 3000) else {
        return vec![];
    };
    let mut ms: Vec<&Value> = v["models"].as_array().into_iter().flatten().collect();
    ms.sort_by(|a, b| b["modified_at"].as_str().cmp(&a["modified_at"].as_str()));
    ms.into_iter()
        .filter_map(|m| {
            let id = m["name"].as_str().or(m["model"].as_str())?;
            let mut note = vec![];
            if let Some(s) = m["details"]["parameter_size"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                note.push(s.to_string());
            }
            if m["remote_model"].is_string() {
                note.push("cloud".into());
            }
            note.push(if ollama_has_tools(m) {
                "tools".into()
            } else {
                "no tool calling".into()
            });
            Some(opt(id, &note.join(" · ")))
        })
        .collect()
}

pub fn fm_base() -> String {
    env_nonempty("THREAD_FM_URL")
        .or_else(|| {
            read_json("config.json")["fm_url"]
                .as_str()
                .map(String::from)
        })
        .unwrap_or_else(|| FM_DEFAULT_URL.into())
        .trim_end_matches('/')
        .to_string()
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn fm_output(args: &[&str]) -> Option<String> {
    let o = Command::new("fm")
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .ok()?;
    Some(strip_ansi(&format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )))
}

fn fm_status() -> (bool, String) {
    let base = fm_base();
    if reachable(&format!("{base}/models")) {
        return (true, format!("Apple Foundation Models server at {base}"));
    }
    if which("fm").is_none() {
        return (
            false,
            "not available — needs the `fm` CLI (macOS with Apple Intelligence), or an OpenAI-compatible bridge at THREAD_FM_URL".into(),
        );
    }
    let license = fm_output(&["license", "--status"]).unwrap_or_default();
    if !license.to_lowercase().contains("agreed to") {
        return (
            false,
            "accept Apple's terms once — thread login fm (runs `fm license`)".into(),
        );
    }
    let models = fm_output(&["models"]).unwrap_or_default();
    if !models
        .lines()
        .any(|l| l.trim_start().starts_with("✓ system"))
    {
        let why = models
            .lines()
            .find(|l| l.contains("system"))
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| "`fm models` lists no on-device model".into());
        return (
            false,
            format!("on-device model unavailable (is Apple Intelligence on?): {why}"),
        );
    }
    (
        true,
        format!(
            "on-device Apple Foundation Model, ~{}k context (thread starts `fm serve` at {base} on demand)",
            FM_CONTEXT / 1024
        ),
    )
}

fn port_of(url: &str) -> Option<u16> {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after.split('/').next()?;
    host.rsplit_once(':')?.1.parse().ok()
}

/// Start `fm serve` if nothing answers at the fm URL. It lives as long as this process: a
/// shell holds our stdin pipe and stops the server when the pipe closes (when we exit).
/// It runs in its own process group, so Ctrl-C in the REPL doesn't take it down.
pub fn ensure_fm_server() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::sync::Mutex;
    static SERVER: Mutex<Option<std::process::Child>> = Mutex::new(None);
    let base = fm_base();
    let mut guard = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    if reachable(&format!("{base}/models")) {
        return Ok(());
    }
    if !is_local(&base) {
        bail!("fm: nothing answers at {base}");
    }
    if which("fm").is_none() {
        bail!("fm: {}", fm_status().1);
    }
    if let Some(mut old) = guard.take() {
        old.kill().ok();
        old.wait().ok();
    }
    let port = port_of(&base).unwrap_or(1976).to_string();
    let child = Command::new("/bin/sh")
        .args([
            "-c",
            r#"fm serve --port "$1" >/dev/null 2>&1 & pid=$!; read _; kill $pid"#,
            "sh",
            &port,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("starting fm serve")?;
    *guard = Some(child);
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(200));
        if reachable(&format!("{base}/models")) {
            return Ok(());
        }
    }
    bail!("fm serve did not come up at {base}: {}", fm_status().1)
}

/// The `local` server: THREAD_LOCAL_URL, else `thread login local`'s choice, else the first
/// well-known port that answers (checked once per process).
pub fn local_base() -> Option<String> {
    if let Some(u) = env_nonempty("THREAD_LOCAL_URL").or_else(|| {
        read_json("config.json")["local_url"]
            .as_str()
            .map(String::from)
    }) {
        return Some(u.trim_end_matches('/').to_string());
    }
    static FOUND: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            LOCAL_SERVERS
                .iter()
                .map(|(_, u)| u.to_string())
                .find(|u| reachable(&format!("{u}/models")))
        })
        .clone()
}

fn login_local() -> Result<()> {
    let cur = local_base().unwrap_or_else(|| LOCAL_SERVERS[0].1.into());
    eprintln!(
        "Any OpenAI-compatible server: LM Studio http://127.0.0.1:1234/v1, llama.cpp (llama-server) http://127.0.0.1:8080/v1, vLLM http://127.0.0.1:8000/v1."
    );
    let base = prompt_line(&format!("Base URL [{cur}]: "))?;
    let base = if base.is_empty() {
        cur
    } else {
        base.trim_end_matches('/').to_string()
    };
    let mut cfg = read_json("config.json");
    cfg["local_url"] = json!(base);
    if let Some(m) = cfg.as_object_mut() {
        m.remove("local_model");
    }
    write_json("config.json", &cfg, false)?;
    let list = openai_models(&base, None, 10_000);
    if list.is_empty() {
        bail!("saved, but {base}/models returned nothing; is the server running?");
    }
    for (i, m) in list.iter().take(20).enumerate() {
        eprintln!("  {:>2}. {}", i + 1, m.id);
    }
    let pick = prompt_line("Default model [1]: ")?;
    let id = resolve_in(
        Provider::Local,
        &list,
        true,
        if pick.is_empty() { "1" } else { &pick },
    )?;
    let mut cfg = read_json("config.json");
    cfg["local_model"] = json!(id);
    write_json("config.json", &cfg, false)?;
    eprintln!("default local model: {id}");
    Ok(())
}

/// The model an in-process thread uses when neither it nor --model names one.
pub fn default_model(p: Provider) -> Option<String> {
    match p {
        Provider::Api => api_default_model(),
        Provider::Ollama => env_nonempty("THREAD_OLLAMA_MODEL")
            .or_else(|| {
                read_json("config.json")["ollama_model"]
                    .as_str()
                    .map(String::from)
            })
            .or_else(|| {
                let list = models(p, false).0;
                // prefer a model on this machine that can call tools
                let pick = list
                    .iter()
                    .find(|m| m.note.ends_with("tools") && !m.note.contains("cloud"))
                    .or_else(|| list.iter().find(|m| m.note.ends_with("tools")))
                    .or(list.first());
                pick.map(|m| m.id.clone())
            }),
        Provider::Fm => env_nonempty("THREAD_FM_MODEL").or_else(|| Some("system".into())),
        Provider::Local => env_nonempty("THREAD_LOCAL_MODEL")
            .or_else(|| {
                read_json("config.json")["local_model"]
                    .as_str()
                    .map(String::from)
            })
            .or_else(|| models(p, false).0.first().map(|m| m.id.clone())),
        _ => None,
    }
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
    for p in [
        Provider::Claude,
        Provider::Codex,
        Provider::Grok,
        Provider::Fx,
    ] {
        if p.installed() && p.status().0 {
            return Ok(p);
        }
    }
    if Provider::Api.status().0 {
        return Ok(Provider::Api);
    }
    // nothing in the cloud: a running Ollama with models will do
    if Provider::Ollama.status().0 {
        return Ok(Provider::Ollama);
    }
    bail!(
        "no provider is logged in. Run `thread login claude` (or codex, grok, fx), `thread login api` for an API key, or `thread use ollama|fm|local` for a local model"
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
    // Same for fx: its profile-scope servers (~/.fx/mcp.json) start without a trust prompt.
    if Provider::Fx.installed() {
        let mut c = Command::new("fx");
        c.args(["mcp", "add"]);
        if let Some(url) = spec["url"].as_str() {
            c.args(["--transport", "http", name, url]);
        } else {
            c.arg(name).args(target);
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
    if Provider::Fx.installed() {
        Command::new("fx")
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
        Provider::Fx => fx_live(),
        Provider::Api => api_live(),
        Provider::Ollama => ollama_live(),
        Provider::Fm => {
            let live = openai_models(&fm_base(), None, 1500);
            if live.is_empty() {
                // the server starts on demand; these are what `fm serve` offers
                vec![
                    opt("system", "on-device Apple Foundation Model"),
                    opt("pcc", "Apple Foundation Model on Private Cloud Compute"),
                ]
            } else {
                live
            }
        }
        Provider::Local => match local_base() {
            Some(b) => openai_models(&b, env_nonempty("THREAD_LOCAL_API_KEY"), 5000),
            None => vec![],
        },
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

/// `fx models` prints `[models] N available` then one ` - <id>` line per model.
fn fx_live() -> Vec<ModelOpt> {
    if !Provider::Fx.installed() {
        return vec![];
    }
    Command::new("fx")
        .arg("models")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            let list = parse_fx_models(&out);
            if list.is_empty() {
                parse_fx_models(&String::from_utf8_lossy(&o.stderr))
            } else {
                list
            }
        })
        .unwrap_or_default()
}

fn parse_fx_models(out: &str) -> Vec<ModelOpt> {
    out.lines()
        .filter_map(|l| l.trim_start().strip_prefix("- "))
        .map(str::trim)
        .filter(|id| !id.is_empty() && !id.contains(' '))
        .map(|id| opt(id, ""))
        .collect()
}

/// Read `fx status` lines (`[status] auth=…`, `auth_expired=…`, `auth_refreshable=…`).
/// An expired login that fx can refresh still works, so it counts as logged in.
fn fx_auth_status(out: &str) -> (bool, String) {
    let field = |k: &str| {
        out.split_whitespace()
            .find_map(|w| w.strip_prefix(k)?.strip_prefix('='))
            .map(str::to_string)
    };
    let auth = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("[status] auth="))
        .map(|v| v.trim().to_string());
    let expired = field("auth_expired").as_deref() == Some("true");
    let refreshable = field("auth_refreshable").as_deref() == Some("true");
    match auth {
        Some(a)
            if !a.is_empty()
                && !matches!(
                    a.to_lowercase().as_str(),
                    "none" | "missing" | "unauthenticated" | "not configured"
                ) =>
        {
            if expired && !refreshable {
                (false, format!("login expired ({a}) — thread login fx"))
            } else {
                let team = field("team")
                    .map(|t| format!(", team {t}"))
                    .unwrap_or_default();
                (true, format!("logged in ({a}{team})"))
            }
        }
        _ => (
            false,
            "not logged in — thread login fx (or `fx setup` for a gateway key)".into(),
        ),
    }
}

/// fx has no per-call MCP flag: register the harness server in its profile config
/// (~/.fx/mcp.json), where servers start with no trust prompt and inherit the env
/// of `fx ask`, so THREAD_ID etc. reach it per thread. `replace` re-points an
/// existing entry at this binary; otherwise an existing entry is left alone.
pub fn register_fx_mcp(exe: &std::path::Path, replace: bool) {
    if !Provider::Fx.installed() {
        return;
    }
    if fx_mcp_registered() {
        if !replace {
            return;
        }
        Command::new("fx")
            .args(["mcp", "remove", "thread"])
            .output()
            .ok();
    }
    Command::new("fx")
        .args(["mcp", "add", "thread"])
        .arg(exe)
        .arg("mcp")
        .output()
        .ok();
}

fn fx_mcp_registered() -> bool {
    home()
        .and_then(|h| std::fs::read_to_string(h.join(".fx").join("mcp.json")).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| v["mcp"]["thread"].is_object())
}

/// `GET {base}/models`, the OpenAI-compatible model list, newest first.
fn api_live() -> Vec<ModelOpt> {
    let (base, key) = api_config();
    if key.is_none() && !is_local(&base) {
        return vec![];
    }
    openai_models(&base, key, 10_000)
}

fn openai_models(base: &str, key: Option<String>, timeout_ms: u64) -> Vec<ModelOpt> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(timeout_ms)))
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
        assert_eq!(Provider::parse("fx.sh").unwrap(), Provider::Fx);
        assert_eq!(Provider::parse("FX").unwrap(), Provider::Fx);
        assert!(Provider::parse("gemini").is_err());
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, uuid());
    }

    #[test]
    fn local_providers() {
        assert_eq!(Provider::parse("Ollama").unwrap(), Provider::Ollama);
        assert_eq!(Provider::parse("apple").unwrap(), Provider::Fm);
        for n in ["lmstudio", "llama.cpp", "vllm", "openai-compat"] {
            assert_eq!(Provider::parse(n).unwrap(), Provider::Local, "{n}");
        }
        assert!(
            Provider::parse("gemini")
                .unwrap_err()
                .to_string()
                .contains("ollama, fm, local")
        );
        for p in ALL {
            assert_eq!(Provider::parse(p.name()).unwrap(), p);
            assert_eq!(p.in_process(), p.binary().is_none());
        }
        assert!(Provider::Ollama.is_local_model() && !Provider::Api.is_local_model());
        assert!(Provider::Api.in_process() && !Provider::Claude.in_process());
    }

    #[test]
    fn fx_status_and_models() {
        let ok = "[status] model=spacexai/grok-4.7\n[status] auth=fx login\n[status] auth_refreshable=true\n[status] team=t-1\n";
        let (yes, detail) = fx_auth_status(ok);
        assert!(yes, "{detail}");
        assert_eq!(detail, "logged in (fx login, team t-1)");
        assert!(
            fx_auth_status(
                "[status] auth=fx login\n[status] auth_expired=true auth_refreshable=true"
            )
            .0,
            "expired but refreshable still works"
        );
        assert!(!fx_auth_status("[status] auth=fx login\n[status] auth_expired=true\n").0);
        assert!(!fx_auth_status("[status] auth=none\n").0);
        assert!(!fx_auth_status("[status] model=x\n").0);
        let ms = parse_fx_models(
            "[models] 3 available\n - anthropic/claude-sonnet-5.5\n - openai/gpt-5.6-luna\n - spacexai/grok-4.7\n",
        );
        assert_eq!(
            ms.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [
                "anthropic/claude-sonnet-5.5",
                "openai/gpt-5.6-luna",
                "spacexai/grok-4.7"
            ]
        );
        assert_eq!(
            resolve_in(Provider::Fx, &ms, true, "grok").unwrap(),
            "spacexai/grok-4.7"
        );
        assert!(resolve_in(Provider::Fx, &ms, true, "gemini").is_err());
    }

    #[test]
    fn hosts_and_urls() {
        assert_eq!(normalize_host("127.0.0.1", 11434), "http://127.0.0.1:11434");
        assert_eq!(
            normalize_host("0.0.0.0:9000", 11434),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            normalize_host("http://box.lan/", 11434),
            "http://box.lan:11434"
        );
        assert_eq!(
            normalize_host("https://ollama.example.com", 11434),
            "https://ollama.example.com"
        );
        assert_eq!(port_of("http://127.0.0.1:1976/v1"), Some(1976));
        assert_eq!(port_of("http://localhost/v1"), None);
        assert_eq!(strip_ansi("\x1b[32m✓ system\x1b[0m ok"), "✓ system ok");
    }

    #[test]
    fn ollama_tool_capability() {
        assert!(ollama_has_tools(
            &json!({"capabilities": ["completion", "tools"]})
        ));
        assert!(!ollama_has_tools(&json!({"capabilities": ["completion"]})));
        assert!(
            ollama_has_tools(&json!({"name": "old-server"})),
            "unknown: let the server decide"
        );
        let ollama = vec![
            opt("qwen3:8b", "8.2B · tools"),
            opt("gemma3:4b", "4B · no tool calling"),
        ];
        assert_eq!(
            resolve_in(Provider::Ollama, &ollama, false, "qwen3").unwrap(),
            "qwen3:8b"
        );
        assert!(resolve_in(Provider::Ollama, &ollama, false, "llama4").is_err());
    }
}
