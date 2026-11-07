//! The `api` provider: any OpenAI-compatible Chat Completions endpoint (OpenAI,
//! OpenRouter, xAI, Groq, Together, Ollama, LM Studio, vLLM, ...).
//!
//! The harness keeps one internal message format (content blocks: text, tool_use,
//! tool_result). `OpenAiCompat` translates requests to `/chat/completions` and
//! responses back, so the agent loop, naps, handoffs and memory calls don't care
//! which backend they talk to. The `Llm` trait also keeps everything testable offline.

use crate::provider::Provider;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

pub trait Llm: Send + Sync {
    /// Send a request in the harness's internal format, return the response in that format.
    fn send(&self, body: Value) -> Result<Value>;
}

/// Client for the in-process providers. `api`, `fm` and `local` speak Chat Completions;
/// `ollama` speaks Ollama's native `/api/chat`, the only way to set its context size
/// (its `/v1` endpoint is stuck at a 4k window and silently truncates the prompt).
pub struct OpenAiCompat {
    agent: ureq::Agent,
    pub provider: Provider,
    pub base_url: String,
    api_key: Option<String>,
    /// Used when neither the request nor the thread names a model.
    pub model: Option<String>,
    /// Sent as `reasoning_effort` only when set explicitly (not every server accepts it).
    pub effort: Option<String>,
}

impl OpenAiCompat {
    pub fn from_config(model: Option<String>, effort: Option<String>) -> Result<Self> {
        Self::for_provider(Provider::Api, model, effort)
    }

    pub fn for_provider(
        provider: Provider,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<Self> {
        let Some((base_url, api_key)) = provider.endpoint() else {
            bail!("{}: {}", provider.name(), provider.status().1);
        };
        if provider == Provider::Api && api_key.is_none() && !crate::provider::is_local(&base_url) {
            bail!("no API key for {base_url} (thread login api, or set OPENAI_API_KEY)");
        }
        let model = model.or_else(|| crate::provider::default_model(provider));
        // local models load into memory on first use and generate slowly on modest hardware
        let secs = std::env::var("THREAD_LLM_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if provider.is_local_model() { 1800 } else { 600 });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(secs)))
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self {
            agent,
            provider,
            base_url,
            api_key,
            model,
            effort,
        })
    }

    fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if self.provider == Provider::Ollama {
            format!("{base}/api/chat")
        } else {
            format!("{base}/chat/completions")
        }
    }

    fn post(&self, body: &Value) -> Result<(u16, Value)> {
        let url = self.url();
        let local = self.provider.is_local_model() || crate::provider::is_local(&self.base_url);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .agent
                .post(&url)
                .header("content-type", "application/json");
            if let Some(k) = &self.api_key {
                req = req.header("authorization", &format!("Bearer {k}"));
            }
            let mut resp = match req.send_json(body) {
                Ok(r) => r,
                // `fm serve` isn't a daemon: start it on first use (or after it died)
                Err(_) if attempt < 3 && self.provider == Provider::Fm => {
                    crate::provider::ensure_fm_server()?;
                    continue;
                }
                Err(e) if attempt < 4 && !local => {
                    eprintln!("  (network error, retrying: {e})");
                    backoff(attempt);
                    continue;
                }
                Err(e) => return Err(self.unreachable(e)),
            };
            let status = resp.status().as_u16();
            let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
            // A local server's 500 is deterministic (context overflow, guardrails); don't hammer it.
            let transient = matches!(status, 429 | 502..=504) || (status >= 500 && !local);
            if transient && attempt < 4 {
                backoff(attempt);
                continue;
            }
            return Ok((status, v));
        }
    }

    fn unreachable(&self, e: ureq::Error) -> anyhow::Error {
        let hint = match self.provider {
            Provider::Ollama => " — is Ollama running? (ollama serve, or open the app)",
            Provider::Fm => " — is `fm serve` running? (thread status)",
            Provider::Local => " — is the local server running? (thread status)",
            _ => "",
        };
        anyhow!(
            "{}: can't reach {}: {e}{hint}",
            self.provider.name(),
            self.url()
        )
    }

    /// One round trip in Chat Completions terms, whatever the wire.
    fn exchange(&self, req: &Value) -> Result<(u16, Value)> {
        if self.provider != Provider::Ollama {
            return self.post(req);
        }
        let (status, v) = self.post(&to_ollama(req, crate::provider::ollama_num_ctx()))?;
        Ok((status, if status == 200 { from_ollama(&v) } else { v }))
    }

    /// Explain errors that local models hit in practice; pass the rest through.
    fn explain(&self, status: u16, v: &Value, model: &str) -> anyhow::Error {
        let msg = err_message(v);
        let low = msg.to_lowercase();
        let who = self.provider.name();
        if low.contains("does not support tools")
            || (low.contains("tool") && low.contains("not supported"))
        {
            let hint = if self.provider == Provider::Ollama {
                " (ollama pull qwen3, or another model marked 'tools' in /model)"
            } else {
                " (/model to pick another)"
            };
            return anyhow!(
                "{who}: {model} can't call tools, and threads need them to do anything. Pick a model with tool calling{hint}"
            );
        }
        if is_context_overflow(&low) {
            let hint = match self.provider {
                Provider::Fm => {
                    "Apple's on-device model has a ~8k-token window: keep this thread's task small, or start a fresh one"
                }
                Provider::Ollama => {
                    "raise THREAD_OLLAMA_NUM_CTX (the model's own maximum is the ceiling)"
                }
                _ => "use a model with a longer context, or start a fresh thread",
            };
            return anyhow!(
                "{who}: the conversation no longer fits {model}'s context window ({msg}). {hint}"
            );
        }
        if self.provider == Provider::Ollama && status == 404 && low.contains("not found") {
            return anyhow!(
                "{who}: {msg} — ollama pull {model}, or /model to pick an installed one"
            );
        }
        let label = if self.provider == Provider::Api {
            "api"
        } else {
            who
        };
        anyhow!("{label} {status}: {msg}")
    }
}

fn is_context_overflow(low: &str) -> bool {
    [
        "context size",
        "context length",
        "context window",
        "maximum context",
        "too many tokens",
        "exceeds the available context",
    ]
    .iter()
    .any(|k| low.contains(k))
}

impl Llm for OpenAiCompat {
    fn send(&self, body: Value) -> Result<Value> {
        let model = body["model"]
            .as_str()
            .map(String::from)
            .or_else(|| self.model.clone())
            .ok_or_else(|| match self.provider {
                Provider::Ollama => anyhow!(
                    "ollama has no models installed; pull one with tool calling (ollama pull qwen3)"
                ),
                p => anyhow!(
                    "no model chosen for the {} provider; pick one with /model",
                    p.name()
                ),
            })?;
        let mut req = to_openai(&body, &model);
        // some servers (fm serve) stream unless told otherwise
        req["stream"] = json!(false);
        if let Some(e) = &self.effort {
            req["reasoning_effort"] = json!(e);
        }
        let structured = req.get("response_format").is_some();
        let (mut status, mut v) = self.exchange(&req)?;
        // Side requests (naps, handoffs, memory) only want JSON; a model without tool calling
        // can still write that, so drop the tool list rather than fail.
        if status != 200
            && structured
            && req.get("tools").is_some()
            && err_message(&v).to_lowercase().contains("tool")
        {
            req.as_object_mut().map(|o| o.remove("tools"));
            (status, v) = self.exchange(&req)?;
        }
        // Many compatible servers lack json_schema; fall back to JSON mode with the schema in the prompt.
        if status != 200
            && structured
            && (400..500).contains(&status)
            && !is_context_overflow(&err_message(&v).to_lowercase())
        {
            let schema = body["output_config"]["format"]["schema"].clone();
            req["response_format"] = json!({"type": "json_object"});
            if let Some(first) = req["messages"].as_array_mut().and_then(|m| m.first_mut()) {
                let sys = first["content"].as_str().unwrap_or("").to_string();
                first["content"] = json!(format!(
                    "{sys}\n\nRespond with only a JSON object matching this JSON Schema:\n{schema}"
                ));
            }
            (status, v) = self.exchange(&req)?;
        }
        if status != 200 {
            return Err(self.explain(status, &v, &model));
        }
        let mut out = from_openai(&v);
        // Local servers often report no usage; estimate (~4 bytes a token) so naps still trigger.
        if out["usage"]["input_tokens"].as_u64().is_none() {
            out["usage"]["input_tokens"] = json!(req.to_string().len() as u64 / 4);
        }
        Ok(out)
    }
}

/// Chat Completions request -> Ollama `/api/chat` request.
pub fn to_ollama(req: &Value, num_ctx: u64) -> Value {
    let mut names: std::collections::HashMap<String, String> = Default::default();
    let msgs: Vec<Value> = req["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let mut m = m.clone();
            if let Some(calls) = m["tool_calls"].as_array_mut() {
                for c in calls {
                    if let (Some(id), Some(name)) =
                        (c["id"].as_str(), c["function"]["name"].as_str())
                    {
                        names.insert(id.into(), name.into());
                    }
                    // Ollama wants arguments as an object, not a JSON string
                    let args = c["function"]["arguments"]
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_else(|| json!({}));
                    c["function"]["arguments"] = args;
                }
            }
            if m["role"] == "tool"
                && let Some(n) = m["tool_call_id"].as_str().and_then(|id| names.get(id))
            {
                m["tool_name"] = json!(n);
            }
            if m["content"].is_null() {
                m["content"] = json!("");
            }
            m
        })
        .collect();
    let mut out = json!({"model": req["model"], "messages": msgs, "stream": false,
        "options": {"num_ctx": num_ctx}});
    if let Some(t) = req.get("tools") {
        out["tools"] = t.clone();
    }
    match req["response_format"]["type"].as_str() {
        Some("json_schema") => {
            out["format"] = req["response_format"]["json_schema"]["schema"].clone()
        }
        Some("json_object") => out["format"] = json!("json"),
        _ => {}
    }
    out
}

/// Ollama `/api/chat` response -> Chat Completions response.
pub fn from_ollama(v: &Value) -> Value {
    let msg = &v["message"];
    let calls: Vec<Value> = msg["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, c)| {
            let args = &c["function"]["arguments"];
            let id = c["id"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("call_{}_{i}", &crate::provider::uuid()[..8]));
            json!({"id": id, "type": "function", "function": {"name": c["function"]["name"],
                "arguments": args.as_str().map(String::from).unwrap_or_else(|| args.to_string())}})
        })
        .collect();
    let finish = if !calls.is_empty() {
        "tool_calls"
    } else if v["done_reason"] == "length" {
        "length"
    } else {
        "stop"
    };
    let mut m =
        json!({"role": "assistant", "content": msg["content"], "reasoning": msg["thinking"]});
    if !calls.is_empty() {
        m["tool_calls"] = json!(calls);
    }
    json!({"choices": [{"finish_reason": finish, "message": m}],
        "usage": {"prompt_tokens": v["prompt_eval_count"], "completion_tokens": v["eval_count"]}})
}

fn err_message(v: &Value) -> String {
    v["error"]["message"]
        .as_str()
        .or(v["error"].as_str())
        .or(v["message"].as_str())
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
}

/// Function schemas for the two Anthropic-defined tools, which have none on the wire.
fn builtin_schema(kind: &str) -> Option<(&'static str, &'static str, Value)> {
    if kind.starts_with("bash_") {
        return Some((
            "bash",
            "Run a shell command in the thread's working directory. Returns stdout and stderr.",
            json!({
            "type": "object", "properties": {"command": {"type": "string"}, "restart": {"type": "boolean"}}, "required": ["command"]}),
        ));
    }
    if kind.starts_with("text_editor_") {
        return Some((
            "str_replace_based_edit_tool",
            "View, create and edit files. view: file (with optional view_range [start, end]) or directory. create: path + file_text. str_replace: path + old_str (must match exactly once) + new_str. insert: path + insert_line + insert_text.",
            json!({
            "type": "object", "properties": {
                "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert"]},
                "path": {"type": "string"}, "file_text": {"type": "string"}, "old_str": {"type": "string"}, "new_str": {"type": "string"},
                "insert_line": {"type": "integer"}, "insert_text": {"type": "string"},
                "view_range": {"type": "array", "items": {"type": "integer"}},
            }, "required": ["command", "path"]}),
        ));
    }
    None
}

fn block_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Internal request -> Chat Completions request.
pub fn to_openai(body: &Value, model: &str) -> Value {
    let mut msgs = vec![];
    if let Some(sys) = body["system"].as_str() {
        msgs.push(json!({"role": "system", "content": sys}));
    }
    for m in body["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        let Some(blocks) = m["content"].as_array() else {
            msgs.push(json!({"role": role, "content": block_text(&m["content"])}));
            continue;
        };
        if role == "assistant" {
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    json!({
                        "id": b["id"], "type": "function",
                        "function": {"name": b["name"], "arguments": b["input"].to_string()},
                    })
                })
                .collect();
            let mut out = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text.join("\n")) }});
            if !calls.is_empty() {
                out["tool_calls"] = json!(calls);
            }
            msgs.push(out);
        } else {
            // tool results first (they must follow the assistant's tool_calls), then any text
            for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                let mut content = block_text(&b["content"]);
                if b["is_error"] == true {
                    content = format!("[error] {content}");
                }
                msgs.push(
                    json!({"role": "tool", "tool_call_id": b["tool_use_id"], "content": content}),
                );
            }
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            if !text.is_empty() {
                msgs.push(json!({"role": "user", "content": text.join("\n")}));
            }
        }
    }
    let mut req = json!({"model": model, "messages": msgs});
    let tools: Vec<Value> = body["tools"].as_array().into_iter().flatten().filter_map(|t| {
        if let Some((name, desc, params)) = t["type"].as_str().and_then(builtin_schema) {
            return Some(json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}}));
        }
        let name = t["name"].as_str()?;
        Some(json!({"type": "function", "function": {"name": name, "description": t["description"], "parameters": t["input_schema"]}}))
    }).collect();
    if !tools.is_empty() {
        req["tools"] = json!(tools);
    }
    let schema = &body["output_config"]["format"]["schema"];
    if !schema.is_null() {
        req["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "output", "schema": schema, "strict": true}});
    }
    req
}

/// Chat Completions response -> internal response.
pub fn from_openai(v: &Value) -> Value {
    let choice = &v["choices"][0];
    let msg = &choice["message"];
    let mut content = vec![];
    // reasoning models on OpenAI-compatible servers (DeepSeek, vLLM, OpenRouter, Ollama…)
    for k in ["reasoning_content", "reasoning"] {
        if let Some(t) = msg[k].as_str().filter(|t| !t.trim().is_empty()) {
            content.push(json!({"type": "thinking", "thinking": t}));
            break;
        }
    }
    if let Some(mut t) = msg["content"].as_str() {
        // local servers often leave a reasoning model's <think>…</think> inline in the text
        if let Some((think, rest)) = t
            .trim_start()
            .strip_prefix("<think>")
            .and_then(|s| s.split_once("</think>"))
        {
            if content.is_empty() && !think.trim().is_empty() {
                content.push(json!({"type": "thinking", "thinking": think.trim()}));
            }
            t = rest.trim_start();
        }
        if !t.is_empty() {
            content.push(json!({"type": "text", "text": t}));
        }
    }
    let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
    for c in &calls {
        let args = c["function"]["arguments"].as_str().unwrap_or("{}");
        let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
        content.push(json!({"type": "tool_use", "id": c["id"], "name": c["function"]["name"], "input": input}));
    }
    let stop = match choice["finish_reason"].as_str() {
        _ if !calls.is_empty() => "tool_use",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    };
    json!({"stop_reason": stop, "content": content, "usage": {
        "input_tokens": v["usage"]["prompt_tokens"], "output_tokens": v["usage"]["completion_tokens"]}})
}

fn backoff(attempt: u32) {
    std::thread::sleep(Duration::from_millis(500 * 2u64.pow(attempt)));
}

/// Returns canned responses in order, recording requests. For tests.
#[cfg(test)]
use std::{collections::VecDeque, sync::Mutex};

#[cfg(test)]
pub struct Scripted {
    pub responses: Mutex<VecDeque<Value>>,
    pub requests: Mutex<Vec<Value>>,
}

#[cfg(test)]
impl Scripted {
    pub fn new(responses: Vec<Value>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        }
    }
    pub fn text(s: &str) -> Value {
        json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": s}], "usage": {"input_tokens": 10, "output_tokens": 10}})
    }
    pub fn json(v: Value) -> Value {
        Self::text(&v.to_string())
    }
}

#[cfg(test)]
impl Llm for Scripted {
    fn send(&self, body: Value) -> Result<Value> {
        self.requests.lock().unwrap().push(body);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("scripted llm exhausted"))
    }
}

// ---- response helpers ----

pub fn text_of(resp: &Value) -> String {
    resp["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Total prompt size the model saw on this request.
pub fn context_tokens(resp: &Value) -> u64 {
    let u = &resp["usage"];
    [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .map(|k| u[*k].as_u64().unwrap_or(0))
    .sum()
}

/// One-shot structured call: a single user prompt, response constrained to `schema`.
pub fn structured(llm: &dyn Llm, system: &str, prompt: &str, schema: Value) -> Result<Value> {
    let resp = llm.send(json!({
        "system": system,
        "messages": [{"role": "user", "content": prompt}],
        "output_config": {"format": {"type": "json_schema", "schema": schema}},
    }))?;
    parse_structured(&resp)
}

pub fn parse_structured(resp: &Value) -> Result<Value> {
    if resp["stop_reason"] == "refusal" {
        bail!("model declined the request");
    }
    let text = text_of(resp);
    serde_json::from_str(&text)
        .with_context(|| format!("model returned non-JSON: {}", truncate(&text, 200)))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[{} more bytes]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_tools_and_history_to_openai() {
        let body = json!({
            "system": "sys",
            "tools": [{"type": "bash_20250124", "name": "bash"}, {"name": "recall", "description": "d", "strict": true, "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "c1", "name": "bash", "input": {"command": "ls"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c1", "content": "a.txt", "is_error": false},
                    {"type": "text", "text": "[from main] hurry"}]},
            ],
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
        });
        let r = to_openai(&body, "gpt-x");
        let m = r["messages"].as_array().unwrap();
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert_eq!(m[2]["content"], "checking");
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "c1", "content": "a.txt"})
        );
        assert_eq!(
            m[4],
            json!({"role": "user", "content": "[from main] hurry"})
        );
        assert_eq!(r["tools"][0]["function"]["name"], "bash");
        assert!(r["tools"][0]["function"]["parameters"]["properties"]["command"].is_object());
        assert_eq!(
            r["tools"][1]["function"]["parameters"],
            json!({"type": "object"})
        );
        assert_eq!(r["response_format"]["type"], "json_schema");
    }

    #[test]
    fn translates_openai_response_back() {
        let v = json!({"choices": [{"finish_reason": "tool_calls", "message": {"content": null,
            "tool_calls": [{"id": "c9", "type": "function", "function": {"name": "remember", "arguments": "{\"entity\":\"x\"}"}}]}}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 7}});
        let r = from_openai(&v);
        assert_eq!(r["stop_reason"], "tool_use");
        assert_eq!(
            r["content"][0],
            json!({"type": "tool_use", "id": "c9", "name": "remember", "input": {"entity": "x"}})
        );
        assert_eq!(context_tokens(&r), 120);
        let r = from_openai(
            &json!({"choices": [{"finish_reason": "stop", "message": {"content": "done"}}]}),
        );
        assert_eq!(
            (r["stop_reason"].as_str(), text_of(&r).as_str()),
            (Some("end_turn"), "done")
        );
    }

    #[test]
    fn translates_to_and_from_ollama_native_chat() {
        let req = json!({"model": "qwen3:8b", "messages": [
            {"role": "system", "content": "sys"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "a.txt"},
        ], "tools": [{"type": "function", "function": {"name": "bash"}}],
           "response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}}}});
        let o = to_ollama(&req, 16384);
        assert_eq!(o["options"]["num_ctx"], 16384);
        assert_eq!(o["stream"], false);
        let m = o["messages"].as_array().unwrap();
        assert_eq!(m[1]["content"], "", "null content becomes empty");
        assert_eq!(
            m[1]["tool_calls"][0]["function"]["arguments"],
            json!({"command": "ls"}),
            "arguments are an object on the native API"
        );
        assert_eq!(m[2]["tool_name"], "bash");
        assert_eq!(o["format"], json!({"type": "object"}));
        assert_eq!(o["tools"][0]["function"]["name"], "bash");

        let v = json!({"message": {"role": "assistant", "content": "", "thinking": "hmm",
            "tool_calls": [{"function": {"name": "bash", "arguments": {"command": "pwd"}}}]},
            "done_reason": "stop", "prompt_eval_count": 900, "eval_count": 12});
        let r = from_openai(&from_ollama(&v));
        assert_eq!(r["stop_reason"], "tool_use");
        assert_eq!(
            r["content"][0],
            json!({"type": "thinking", "thinking": "hmm"})
        );
        let call = &r["content"][1];
        assert_eq!(
            (call["name"].as_str(), &call["input"]),
            (Some("bash"), &json!({"command": "pwd"}))
        );
        assert!(
            call["id"].as_str().unwrap().starts_with("call_"),
            "ids are made up when missing"
        );
        assert_eq!(context_tokens(&r), 900);
    }

    #[test]
    fn inline_think_tags_become_thinking() {
        let r = from_openai(&json!({"choices": [{"finish_reason": "stop",
            "message": {"content": "<think>\nweighing it\n</think>\n\nAnswer."}}]}));
        assert_eq!(
            r["content"][0],
            json!({"type": "thinking", "thinking": "weighing it"})
        );
        assert_eq!(text_of(&r), "Answer.");
        // a literal mention later in the text is left alone
        let r = from_openai(&json!({"choices": [{"finish_reason": "stop",
            "message": {"content": "use <think> tags"}}]}));
        assert_eq!(text_of(&r), "use <think> tags");
    }

    #[test]
    fn explains_local_model_errors() {
        let c = OpenAiCompat::for_provider(Provider::Fm, None, None).unwrap();
        assert_eq!(
            c.model.as_deref(),
            Some("system"),
            "fm needs no model choice"
        );
        let e = c
            .explain(400, &json!({"error": {"message": "registry.ollama.ai/library/gemma3 does not support tools"}}), "gemma3")
            .to_string();
        assert!(e.contains("can't call tools"), "{e}");
        let e = c
            .explain(
                500,
                &json!({"error": "Exceeded model context window size"}),
                "system",
            )
            .to_string();
        assert!(e.contains("context window") && e.contains("8k"), "{e}");
        let e = c
            .explain(418, &json!({"error": "teapot"}), "system")
            .to_string();
        assert_eq!(e, "fm 418: teapot");
    }
}
