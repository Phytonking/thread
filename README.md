# thread

A lightweight agent harness in Rust. One ~4 MB binary, SQLite bundled, no server.

- **Threads are sessions.** Running `thread` in a repo puts you on `main`, a persistent conversation for that repo that picks up where you left off. From main you ask for work, and the agent spawns **background threads** that run in parallel, each with a readable name, its own conversation and (when it edits code) its own git worktree and branch. You can switch between threads, message them, approve their actions, and stop them.
- **Memory consolidates, lightly.** Facts are learned at three points by the model that already has the context: `remember` calls, naps (when a conversation gets long) and handoffs (when a run ends). Nothing re-reads transcripts unless a run was surprising. Changed facts reach other threads immediately. Repeated successful work becomes skills.

## Quick start

```bash
cargo build --release && ln -sf "$PWD/target/release/thread" ~/.local/bin/thread

thread login claude        # or: codex, grok — uses your subscription via that vendor's CLI
thread status              # what's installed / logged in, and the default
cd ~/code/some-repo
thread                     # opens main
```

## Providers: subscriptions first, API key if you must

`thread` drives each vendor's **official CLI headlessly**, so your Claude Pro/Max, ChatGPT, or SuperGrok login is what gets used. The CLIs keep their own auth; `thread` never touches tokens.

| provider | login | headless call | harness tools | approvals |
|---|---|---|---|---|
| `claude` | `thread login claude` → `claude auth login` | `claude -p --output-format stream-json`, `--resume <session>` | MCP (`thread mcp`, passed per call) | Claude's permission prompts → your y/n queue. Reading and web search (Read, Grep, Glob, WebSearch, WebFetch…), the harness's own tools and `/always` tools never ask. Edits auto-accepted inside a thread's own worktree |
| `codex` | `thread login codex` → `codex login` | `codex exec --json`, `codex exec resume <id>` | MCP via `-c mcp_servers.thread…`, pre-approved (`default_tools_approval_mode="approve"`) | can't prompt headlessly: runs in the `workspace-write` sandbox (writes limited to its dir) |
| `grok` | `thread login grok` → `grok login`, also registers the MCP server | `grok -p --output-format json`, `-r <id>` | MCP (`grok mcp add thread`) | `--sandbox workspace` |
| `api` | `thread login api`: any **OpenAI-compatible** endpoint (base URL + key, saved mode 600), or `OPENAI_BASE_URL` / `OPENAI_API_KEY` | built-in tool loop over `/chat/completions` | native (function calling) | native y/n |
| `ollama` | none: a running [Ollama](https://ollama.com) (`OLLAMA_HOST`, default `127.0.0.1:11434`) | built-in tool loop over Ollama's native `/api/chat` | native | native y/n |
| `fm` | none: macOS with Apple Intelligence and the `fm` CLI; `thread login fm` accepts Apple's terms once | built-in tool loop over `/chat/completions` on `fm serve`, started on demand | native | native y/n |
| `local` | none: any local OpenAI-compatible server (LM Studio, llama.cpp `llama-server`, vLLM…); `thread login local` saves its URL and default model | built-in tool loop over `/chat/completions` | native | native y/n |
| `ollama` | none: a running Ollama (`OLLAMA_HOST`, default `127.0.0.1:11434`) | built-in tool loop over Ollama's native `/api/chat` | native (function calling) | native y/n |
| `fm` | `thread login fm` accepts Apple's terms once (`fm license`) | built-in tool loop over `fm serve` (`/v1/chat/completions`), started on demand | native (function calling) | native y/n |
| `local` | none, or `thread login local` to save a base URL and default model | built-in tool loop over `/chat/completions` | native (function calling) | native y/n |

- **Default:** the `--provider` flag, then `$THREAD_PROVIDER`, then `thread use <provider>`, then the first logged-in CLI (claude, codex, grok), then `api` if a key exists, then a running Ollama with models installed.
- **Per thread:** every thread runs on its own provider and model. Ask main in plain words ("have codex fix the tests", "review it with opus"): it spawns with `spawn_thread(..., provider, model)` and can move a running thread with `set_thread_model`. You can move the focused thread yourself with `/provider codex [model]` and `/model <name>`. A new provider starts a fresh session from the thread's next turn; thread state and memory carry over. `list_threads` / `read_thread` show each thread as `provider/model`.
- **`api` is OpenAI-compatible.** One base URL and key covers OpenAI, OpenRouter, xAI, Groq, Together, Ollama, LM Studio, vLLM and others. `thread login api` asks for the URL and key (no key needed for localhost), lists the server's models, and lets you pick a default. Requests go to `/chat/completions` with function calling. Structured outputs use `response_format` json_schema, falling back to JSON mode on servers that don't support it. `--effort` is sent as `reasoning_effort` only when you set it.
- **Local models** (`ollama`, `fm`, `local`) run through the same built-in loop as `api`, with no key and nothing billed. Pick one with `thread use ollama` (or `--provider ollama`, `/provider ollama qwen3`, or ask main to "have ollama do it").
  - **ollama:** status lists installed models and which can call tools; the default is the newest installed model that can. `--model qwen3` resolves to the installed `qwen3:8b`. thread talks to `/api/chat` rather than `/v1` because only the native API can set the context size: Ollama's default 4k window silently truncates the system prompt and tools, so thread asks for `THREAD_OLLAMA_NUM_CTX` (default 32768) tokens. Lower it on a small machine. Every request in a thread (turns, naps, handoffs) uses the thread's model, so Ollama keeps one model loaded.
  - **fm:** Apple's on-device Foundation Model through the `fm` CLI that ships with macOS. thread starts `fm serve` on `127.0.0.1:1976` when first needed and stops it on exit. To use another OpenAI-compatible bridge, set `THREAD_FM_URL` (or `fm_url` in `config.json`). The model is `system` (on-device; `pcc` is Private Cloud Compute). Its window is about 8k tokens, so fm threads nap early to stay inside it, and an overflow says so plainly. Keep fm tasks small.
  - **local:** `THREAD_LOCAL_URL`, else `thread login local`'s choice, else the first of LM Studio `:1234`, llama.cpp `:8080` or vLLM `:8000` that answers `GET /v1/models`. `THREAD_LOCAL_MODEL` and `THREAD_LOCAL_API_KEY` are optional.
  - **Quirks handled:** a model that can't call tools fails with an error naming one that can (naps and handoffs only need JSON, so they retry without tools). Missing `usage` is estimated so naps still trigger. Inline `<think>…</think>` becomes a thinking step. Local servers aren't retried on 500s, which they return for deterministic failures like context overflow. The request timeout is 30 minutes for local providers and 10 for the rest (`THREAD_LLM_TIMEOUT`, seconds). `THREAD_CONTEXT_WINDOW` sets the window a local thread plans its naps around.
- **Local models** (`ollama`, `fm`, `local`) run through the same built-in loop as `api`, with no key and no cost. Pick one with `thread use ollama`, `--provider fm`, `/provider local`, or `spawn_thread(..., provider: "ollama")`; if no cloud provider is logged in, a running Ollama with models becomes the default. `thread status` shows whether each one is reachable.
  - **ollama**: models come from `GET /api/tags`; `--model qwen3` picks the installed `qwen3:8b`. The default is `THREAD_OLLAMA_MODEL`, else the newest installed model that supports tools. thread talks to Ollama's native `/api/chat` and asks for a `THREAD_OLLAMA_NUM_CTX` (default 32768) token window, because Ollama's `/v1` endpoint is stuck at 4096 tokens and silently drops the start of the prompt. A model without tool calling is rejected with a clear error (threads need tools); `/model` marks which installed models have them.
  - **fm**: Apple's on-device Foundation Model, via the `fm` CLI (macOS with Apple Intelligence on). thread starts `fm serve --port 1976` the first time it's needed and stops it on exit; point `THREAD_FM_URL` at another OpenAI-compatible bridge to use that instead. Models are `system` (on-device, the default) and `pcc` (Private Cloud Compute). The window is ~8k tokens, so naps (context distillation) start early; a long task can still overflow, and you get an error saying so. Keep fm threads to small tasks.
  - **local**: any OpenAI-compatible server on this machine: LM Studio (`:1234`), llama.cpp `llama-server` (`:8080`) or vLLM (`:8000`) are found automatically; otherwise set `THREAD_LOCAL_URL` or run `thread login local`. `THREAD_LOCAL_API_KEY` if your server wants one.
  - Quirks handled: no usage numbers (estimated from request size so naps still trigger), `<think>…</think>` in the text shown as thinking, no retries on a local server's deterministic 500s, and a 30-minute request timeout (`THREAD_LLM_TIMEOUT`, seconds) for slow first loads. `THREAD_CONTEXT_WINDOW` overrides the window thread plans around.
- **One delegation path.** main runs with the CLI's own subagents turned off (Codex `multi_agent`) and is told that background threads exist only through `spawn_thread`. If a spawn fails you see the error instead of the CLI working around it with agents you can't switch to.
- **Models are pulled live:**
  - claude: Claude Code reports which model each alias (`opus`, `sonnet`, `haiku`, `fable`) currently resolves to, e.g. `opus → claude-opus-5-5`. Picking an alias means the thread follows the newest model in that tier.
  - codex: `codex debug models`, which refreshes your ChatGPT account's catalog.
  - grok: `grok models`.
  - api: `GET {base}/models` from your endpoint, newest first.
  - ollama: `GET /api/tags`, newest first, marked `tools` or `no tool calling`. fm and local: `GET /v1/models`.

  Claude and api lists are cached for 12h in `~/.config/thread/models.json`, and `/model refresh` re-pulls them. Pick by number, name, or a unique partial match. A name the provider can't run is rejected with its options. `--model` applies only to the default provider, and spawned threads on the same provider inherit their parent's model.
- **Memory calls** (replay, skills) run on the default provider as one-shot ephemeral calls, so they also use the subscription. Override with `THREAD_MEMORY_PROVIDER` and `--memory-model`.
- **Same capabilities as running the CLI yourself:** Claude threads load your full config (`user,project,local`): your MCP servers, plugins, skills and hooks, plus the `thread` server. Codex keeps the servers in `~/.codex/config.toml` and adds `thread`. To run Claude without your personal setup, set `THREAD_CLAUDE_SETTINGS=project,local`.
- `--yolo` maps to each CLI's skip-approvals mode: `--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`, `--always-approve`.

Then talk to it:

```
main› spawn a thread to find why the search tests are flaky, and another to add rate limiting to /api/login
• spawn_thread {"name":"flaky-search-tests", ...}
◆ spawned flaky-search-tests on thread/flaky-search-tests — /switch flaky-search-tests to watch
• spawn_thread {"name":"login-rate-limit", ...}
◆ spawned login-rate-limit on thread/login-rate-limit
Started both. I'll tell you when they report back.
main› ⚠ [login-rate-limit] wants to bash `cargo test -p api` — /y login-rate-limit or /n login-rate-limit
main› /y login
main› /switch flaky
── flaky-search-tests [running] · thread/flaky-search-tests
flaky-search-tests› also check whether it only fails on CI      ← steers it mid-run
flaky-search-tests› /main
main› ✓ [flaky-search-tests] finished (success): race in index warmup — /switch flaky-search-tests
flaky-search-tests finished: the index warmup races the first query. Fix is on thread/flaky-search-tests; want me to merge it?
```

## Help

### CLI

| command | what it does |
|---|---|
| `thread` | open main for the current repo |
| `thread login [claude\|codex\|grok\|api\|ollama\|fm\|local]` | log in through the vendor CLI, store an API key, or set up a local model; no argument shows status |
| `thread status` | providers: installed, logged in, default |
| `thread use <provider>` | set the default provider |
| `thread --resume` / `thread -r` | pick any of this repo's threads to open, closed ones included |
| `thread <name>` / `thread open <name>` | open the REPL focused on a thread. Names match exactly, then by prefix, then by substring (`thread flaky`) |
| `thread ls [--all]` | list threads: status, age, branch, where each stands |
| `thread show <name>` | task, state, latest output, runs with surprise scores |
| `thread ask "<msg>" [--to <name>]` | non-interactive: send one message, wait for everything (including spawned threads) to settle, exit |
| `thread stop <name>` | stop a thread |
| `thread rm <name>` | close a thread and remove its worktree (the branch is kept) |
| `thread recall "<query>"` | search memory |
| `thread facts [--all]` | list facts (`--all` includes superseded and cold) |
| `thread skills` | list learned skills |
| `thread remember <entity> <attribute> <value>` | assert a trusted fact |
| `thread consolidate` | run consolidation now (it normally runs after every handoff) |
| `thread log` | audit log of memory changes |

Global flags: `--provider`, `--yolo` (run all tools without asking), `--model`, `--memory-model`, `--effort` (api only).

### In the REPL

| input | |
|---|---|
| plain text | talk to the focused thread (start, stop and steer threads by asking main) |
| `y` / `n` / `a` | answer the focused thread's approval request (`a`: allow, and never ask about that tool again) |
| `/y` `/n` `/always` `[name\|all]` | answer approvals from any thread without switching: a named thread's, every pending one (`all`), or with no name the focused thread's, else the oldest. `/always` remembers the tool in `~/.config/thread/config.json` for every thread |
| `/model [number\|name]` | live model menu for this thread's provider, or pick one (`default` resets, `refresh` re-pulls) |
| `/provider [name [model]]` | providers with login status, or move this thread (claude, codex, grok, api, ollama, fm, local), optionally straight to a model |
| `/mcp` · `/mcp add <name> <command…\|url>` · `/mcp rm <name>` | your MCP servers, given to every claude, codex and grok thread |
| `/threads` | list threads (`▸` focus, `⚠` waiting for approval) |
| `/switch [name]` | focus another thread (`main` to go back). No name: pick from a list (↑/↓, a number, Enter; Esc cancels). `/thread` and `/t` are aliases. Other threads' approvals are answered after switching to them |
| `/clear` | clear the screen and scrollback, leaving just the prompt |
| `/expand [n\|all]` · `/collapse` | thinking steps (claude thinking, codex reasoning, `reasoning_content` from api servers) print as one dim `∴` line. `/expand` opens the latest in full, `/expand 3` the third-latest, `/expand all` prints every step in full from now on; `/collapse` goes back. Steps are kept per thread, so you can expand a background thread's after switching to it |
| `! <command>` | run a shell command in the focused thread's directory; the output prints live and is attached to your next message to that thread |
| no slash | `switch probe`, `thread probe`, `threads`, `main`, `clear`, `help` work as commands when typed alone (or with an existing thread's name). Anything longer, like `switch to plan B`, is a message |
| `ctrl-c` | in a background thread: back to main. On main: the same as `exit`. Once the hand-off has started, a second ctrl-c quits at once (unfinished work resumes next launch). Threads' commands run in their own process group, so ctrl-c never kills their work |
| `exit` | quit. Idle threads hand off. If some are still running you choose: leave them (they resume next launch) or wait |

### What the agent can do

- **Every thread:** `bash`, the text editor, `recall`, `remember`, `checkpoint`, `list_threads`
- **main:** `spawn_thread(name, task, worktree)`, `message_thread`, `read_thread`, `stop_thread`
- **background threads:** `notify_main` for something main needs before the thread finishes

### Approvals

`bash` and file writes ask first, as `y/n` in the focused thread or a `⚠` notice for others. Exceptions:
- writes inside a thread's own worktree never ask
- `--yolo` turns asking off
- `thread ask` has no one to ask, so it denies unless you pass `--yolo`

## How it works

```
 you ──► REPL ──► focused thread's inbox
                      │
   ┌──────────────────┼──────────────────────────────────────┐
   │ main (worker)    │       background threads (workers)    │
   │  conversation ◄──┘        own conversation, own worktree │
   │  spawn/message/read/stop ───────────────► inbox          │
   │  inbox ◄──────────── report on finish, notify_main ──────│
   └───────────────────────────────────────────────────────────┘
          every step persisted to <repo>/.thread/thread.db
```

- Each thread runs on its own OS thread. Only the focused thread prints. Others print one-line notices: spawned, finished, failed, needs approval.
- A **run** is one stretch of work on a thread, from a request until idle. A background thread's run ends when it goes idle unfocused, or when you switch away from it. Its handoff saves state and facts, and its report goes to main's inbox, which wakes main to tell you.
- Conversations are append-only and saved after every step. Quit with threads running and they resume on the next `thread` launch. An interrupted tool call is reported to the model as interrupted rather than replayed.
- State lives in `<repo>/.thread/`, which is added to `.git/info/exclude` automatically. Worktrees go in `.thread/worktrees/<name>`, on branch `thread/<name>`.

### Memory

| when | what | cost |
|---|---|---|
| `remember` (any time) | fact stored as tentative | none |
| nap (context > 120k) | summary + facts + state; the conversation restarts from the summary | 1 call |
| handoff (end of run) | state, outcome, task type, summary, facts | 1 call |
| consolidate (after each handoff) | surprise score per run from signals; replay only runs scoring ≥ 0.6 (max 3); skill once a task type has 3 successes; decay | usually 0 calls |

Rules:
- Facts from agents start tentative and become active once a second run agrees.
- A single run can't overturn a fact two runs agree on; the new value waits as a tentative challenger.
- Superseded facts are kept, and every other open thread that used the old value gets a `[from memory]` notice.
- Skills start tentative, promote after 2 successful uses, and retire if they mostly fail.
- Facts that go unused go cold: kept, but no longer recalled.
- Surprise comes from external signals only: outcome against the task type's track record, duration, retries after failed tools, tool error rate, user corrections ("no, …", "instead", "revert"), and whether the task type is new.

## Testing

### 1. BDD acceptance tests: the user requirements

`tests/bdd/features/*.feature` holds Gherkin scenarios, each with an "As a / I want / So that" story. They cover:
- the main thread
- named background threads (spawning, unique names, switching)
- live model menus
- providers
- MCP servers
- approvals (including read-only auto-approval)
- memory (tentative → active, cross-thread notices)
- resuming running threads
- the minimal command set
- one-shot runs

`tests/bdd/run.py` is a small stdlib-only Gherkin runner. It drives the real binary in a fresh git repo and config dir per scenario, with its own scriptable OpenAI-compatible mock model (`tests/bdd/mock_llm.py`).

```bash
cargo build --release
python3 tests/bdd/run.py                       # 43 offline scenarios, ~40s, no accounts needed
python3 tests/bdd/run.py --tags live           # real Claude + Codex subscriptions (uses haiku / your Codex default)
python3 tests/bdd/run.py --tags claude -k fix  # filter by tag and scenario name; -v prints output for failures
```

Status on 2026-10-26:
- offline: 43/43, stable across repeated parallel runs
- live: 3/3. Claude main delegates a fix that a worker commits in its worktree. Codex main spawns through MCP without `--yolo`, and the worker writes its file. Claude calls the harness tools.

### 2. Unit tests

`cargo test`: 32 tests against a scripted fake model. They cover:
- the memory policies
- naming
- inbox delivery
- naps and handoffs
- stream parsing for each CLI
- the Chat Completions translation
- command lines
- approvals
- the read-only allowlist
- a concurrent runtime run

### 3. Try it by hand

```bash
python3 tests/bdd/mock_llm.py 8765 &          # offline: "spawn thread x: task", "run: cmd", "remember e a v"
OPENAI_BASE_URL=http://127.0.0.1:8765/v1 thread --provider api
thread --provider claude --model haiku         # or with your subscription
```

## Config

| env / flag | default |
|---|---|
| `--provider` / `THREAD_PROVIDER` / `thread use` | first logged-in CLI |
| `THREAD_MEMORY_PROVIDER` | the default provider |
| `THREAD_CLAUDE_SETTINGS` | `user,project,local` |
| `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `THREAD_API_MODEL` (or `thread login api`) | api provider only (default base `https://api.openai.com/v1`) |
| `OLLAMA_HOST`, `THREAD_OLLAMA_MODEL`, `THREAD_OLLAMA_NUM_CTX` | ollama: `127.0.0.1:11434`, newest installed model with tool calling, 32768 |
| `THREAD_FM_URL`, `THREAD_FM_MODEL` | fm: `http://127.0.0.1:1976/v1` (`fm serve`, started on demand), `system` |
| `THREAD_LOCAL_URL`, `THREAD_LOCAL_MODEL`, `THREAD_LOCAL_API_KEY` | local: `thread login local`'s choice, else the first well-known port that answers |
| `THREAD_LLM_TIMEOUT` | request timeout in seconds: 1800 for local providers, 600 otherwise |
| `THREAD_CONTEXT_WINDOW` | fm: 8192, ollama: its num_ctx; sets how early local threads nap |
| `OLLAMA_HOST`, `THREAD_OLLAMA_MODEL`, `THREAD_OLLAMA_NUM_CTX` | `127.0.0.1:11434`, newest installed model with tools, `32768` |
| `THREAD_FM_URL`, `THREAD_FM_MODEL` | `http://127.0.0.1:1976/v1` (`fm serve`, started on demand), `system` |
| `THREAD_LOCAL_URL`, `THREAD_LOCAL_MODEL`, `THREAD_LOCAL_API_KEY` (or `thread login local`) | first of LM Studio / llama.cpp / vLLM that answers, the server's first model, none |
| `THREAD_LLM_TIMEOUT` | `600` seconds (`1800` for local models) |
| `THREAD_CONTEXT_WINDOW` | fm 8192, ollama its `num_ctx` |
| `THREAD_MODEL` / `--model` | the provider's own default (api: the model picked at login) |
| `THREAD_MEMORY_MODEL` / `--memory-model` | the thread model |
| `THREAD_EFFORT` / `--effort` | unset (api only: `reasoning_effort`) |

Requests use adaptive thinking, automatic prompt caching, and server-side refusal fallback.

## Layout

```
src/main.rs         CLI + REPL (focus, switching, approvals)
src/runtime.rs      workers, inbox delivery, approvals, worktrees, resume, shutdown
src/agent.rs        conversation engine: tool loop, steering, nap, handoff
src/consolidate.rs  learn + propagate, surprise scoring, replay, skills, decay
src/memory.rs       facts (corroborate/supersede/contest), skills, recall, decay
src/threads.rs      threads, names, inbox, runs, events
src/tools.rs        bash, editor, memory and thread tools
src/provider.rs     providers: login, status, default, key storage
src/cli.rs          headless claude / codex / grok turns + stream parsers + CliLlm
src/mcp.rs          `thread mcp`: harness tools + approval prompts for the CLIs
src/llm.rs          OpenAI-compatible client (api and local providers), Ollama native translation, scripted fake
src/db.rs           schema
tests/bdd/          Gherkin features, runner, scriptable mock model
```

## Known gaps

- Messages to a CLI-backed thread that is mid-turn are delivered when the turn ends, not at the next tool call (api threads take them at the next tool call).
- Output arrives per turn, not streamed, and printing is plain line output rather than a TUI. Notices can land in the middle of what you're typing.
- Threads only run while some `thread` process is open; there is no daemon.
- Merging a thread's branch is up to you or main (`git merge thread/<name>`).
- Recall is keyword-only (SQLite FTS5 with BM25), with no embeddings.
