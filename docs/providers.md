# Providers

[← README](../README.md) · [Guide](guide.md) · [Commands](commands.md) · **Providers** · [Safety](safety.md) · [Memory](memory.md) · [Configuration](configuration.md) · [Architecture](architecture.md)

thread drives each vendor's **official CLI headlessly**, so your Claude Pro/Max, ChatGPT, SuperGrok or fx login does the work. The CLIs keep their own auth, and thread never touches their tokens. If you'd rather pay per token, any OpenAI-compatible API works, and local models need no key at all.

| provider | log in with | runs as | harness tools | approvals |
|---|---|---|---|---|
| `claude` | `thread login claude` → `claude auth login` | `claude -p --output-format stream-json`, `--resume <session>` | MCP (`thread mcp`, passed per call) | Claude's permission prompts become your y/n queue. Reads, web search (Read, Grep, Glob, WebSearch, WebFetch…), the harness's own tools and `/always` tools never ask. Edits inside a thread's own worktree are auto-accepted |
| `codex` | `thread login codex` → `codex login` | `codex exec --json`, `codex exec resume <id>` | MCP via `-c mcp_servers.thread…`, pre-approved | Can't prompt headlessly, so it runs in Codex's `workspace-write` sandbox |
| `grok` | `thread login grok` → `grok login` (also registers MCP) | `grok -p --output-format json`, `-r <id>` | MCP (`grok mcp add thread`) | `--sandbox workspace` |
| `fx` | `thread login fx` → `fx login` (also registers MCP) | `fx ask --json`, `--resume-id <id>` | MCP in `~/.fx/mcp.json` | `--auto`; with `--yolo`, `--full-access` |
| `api` | `thread login api`: any OpenAI-compatible URL + key (saved mode 600), or `OPENAI_BASE_URL` / `OPENAI_API_KEY` | built-in tool loop over `/chat/completions` | native function calling | native y/n |
| `ollama` | nothing; needs a running [Ollama](https://ollama.com) | built-in loop over Ollama's native `/api/chat` | native | native y/n |
| `fm` | macOS with Apple Intelligence; `thread login fm` accepts Apple's terms once | built-in loop over `fm serve`, started on demand | native | native y/n |
| `local` | nothing; any local OpenAI-compatible server. `thread login local` saves its URL and model | built-in loop over `/chat/completions` | native | native y/n |

## Choosing a provider

**Default:** `--provider`, then `$THREAD_PROVIDER`, then `thread use <provider>`, then the first logged-in CLI (claude, codex, grok, fx), then `api` if a key is set, then a running Ollama with models installed.

**Per thread:** every thread has its own provider and model. Ask main in plain words ("have codex fix the tests", "review it with opus"). main spawns with `spawn_thread(..., provider, model)` and can move a running thread with `set_thread_model`. You can also move the focused thread yourself with `/provider codex [model]` and `/model <name>`. Switching provider starts a fresh session from the thread's next turn. Thread state and memory carry over. `list_threads` and `read_thread` show each thread as `provider/model`.

**Memory calls** (replay, skills) run as one-shot calls on the default provider, so they also use your subscription. Override with `THREAD_MEMORY_PROVIDER` and `--memory-model`.

## Behavior shared by the CLI providers

- **Same powers as running the CLI yourself.** Claude threads load your full config (`user,project,local`): your MCP servers, plugins, skills and hooks, plus the `thread` server. Codex keeps the servers in `~/.codex/config.toml` and adds `thread`. Set `THREAD_CLAUDE_SETTINGS=project,local` to run Claude without your personal setup.
- **One delegation path.** main runs with the CLI's own subagents turned off (Codex `multi_agent`), so background work only happens through `spawn_thread`. If a spawn fails you see the error, instead of the CLI quietly working around it with agents you can't switch to.
- **`--yolo`** maps to each CLI's skip-approvals mode: `--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`, `--always-approve`, `--full-access`. The zone still applies. See [Safety](safety.md).

## Model catalogs

Model lists are pulled live:

- **claude:** Claude Code reports what each alias (`opus`, `sonnet`, `haiku`, `fable`) resolves to right now, e.g. `opus → claude-opus-5-5`. Pick an alias and the thread follows the newest model in that tier.
- **codex:** `codex debug models`, which refreshes your ChatGPT account's catalog.
- **grok:** `grok models`.
- **fx:** `fx models`.
- **api:** `GET {base}/models`, newest first.
- **ollama:** `GET /api/tags`, newest first, each marked `tools` or `no tool calling`.
- **fm / local:** `GET /v1/models`.

Claude and api lists are cached for 12h in `~/.config/thread/models.json`. `/model refresh` re-pulls them. Pick a model by number, name, or a unique partial match. A name the provider can't run is rejected and the valid options are listed. `--model` applies only to the default provider. Spawned threads on the same provider inherit their parent's model.

## fx

- fx has no per-call MCP flag. The `thread mcp` server is registered once in `~/.fx/mcp.json`, by `thread login fx`, by `thread use fx`, or on the first fx turn if it's missing. `/mcp add|rm` changes are mirrored into fx's profile config.
- fx chooses the session id. thread reads it from the first turn's JSON and passes `--resume-id` afterwards.
- fx has no read-only mode. When a thread must not write, thread runs fx with `FX_PERMISSION_MODE=ask`. Headless fx then blocks every write, shell command and MCP call and ends the turn, and you get a clear error rather than a run with write access.
- fx has no schema flag. For structured calls (naps and handoffs) the schema goes in the prompt and thread pulls the JSON out of the reply.
- An expired login that fx can refresh counts as logged in.

## api

One base URL and key covers OpenAI, OpenRouter, xAI, Groq, Together, vLLM and others. `thread login api` asks for the URL and key (no key needed for localhost), lists the server's models, and lets you pick a default. Requests go to `/chat/completions` with function calling. Structured outputs use `response_format` json_schema, with a fallback to JSON mode for servers that don't support it. `--effort` is sent as `reasoning_effort`, but only if you set it.

## Local models

`ollama`, `fm` and `local` run through the same built-in loop as `api`, with no key and nothing billed. Pick one with `thread use ollama`, `--provider ollama`, `/provider ollama qwen3`, or by asking main to "have ollama do it".

### ollama

`thread status` lists installed models and which ones can call tools. The default is the newest installed model that can. `--model qwen3` resolves to the installed `qwen3:8b`.

thread uses Ollama's native `/api/chat` rather than `/v1`, because only the native API can set the context size. Ollama's default 4k window silently truncates the system prompt and tools, so thread requests `THREAD_OLLAMA_NUM_CTX` tokens (default 32768). Lower it on a small machine. Every request in a thread (turns, naps, handoffs) uses the thread's model, so Ollama keeps a single model loaded.

### fm

Apple's on-device Foundation Model, through the `fm` CLI that ships with macOS. thread starts `fm serve` on `127.0.0.1:1976` when it's first needed and stops it on exit. To use another OpenAI-compatible bridge, set `THREAD_FM_URL` (or `fm_url` in `config.json`). The model is `system` (on-device); `pcc` is Private Cloud Compute. The window is about 8k tokens, so fm threads nap early, and an overflow error says so plainly. Keep fm tasks small.

### local

Uses `THREAD_LOCAL_URL`, else what `thread login local` saved, else the first of LM Studio `:1234`, llama.cpp `:8080` or vLLM `:8000` that answers `GET /v1/models`. `THREAD_LOCAL_MODEL` and `THREAD_LOCAL_API_KEY` are optional.

### Quirks thread handles for you

- A model that can't call tools fails with an error naming one that can. Naps and handoffs only need JSON, so they retry without tools.
- On local providers, naps and handoffs never send the tool list. Small models like fm call a tool instead of answering, and the list uses up the window.
- A model that answers a structured request with tool calls is asked again without tools.
- When a server omits `usage`, thread estimates it so naps still trigger.
- Inline `<think>…</think>` is shown as a thinking step.
- Local servers aren't retried on 500s, because they return 500 for deterministic failures like context overflow.
- Timeouts are 30 minutes for local providers and 10 for the rest (`THREAD_LLM_TIMEOUT`, in seconds). `THREAD_CONTEXT_WINDOW` sets the window a local thread plans its naps around.
