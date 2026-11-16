# Architecture

[← README](../README.md) · [Guide](guide.md) · [Commands](commands.md) · [Providers](providers.md) · [Safety](safety.md) · [Memory](memory.md) · [Configuration](configuration.md) · **Architecture**

## Runtime model

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

- Each thread runs on its own OS thread with an inbox. Only the focused thread prints. The others post one-line notices: spawned, finished, failed, needs approval.
- A **run** is one stretch of work on a thread, from a request until the thread goes idle. A background run ends when it goes idle unfocused or when you switch away. Its **handoff** saves state and facts and sends a report to main's inbox, which wakes main to tell you.
- Conversations are append-only and saved after every step. Threads still running at quit resume on the next launch. An interrupted tool call is reported to the model as interrupted, not replayed.
- State lives in `<repo>/.thread/`. Worktrees go in `.thread/worktrees/<name>` on branch `thread/<name>`.

## Providers under the hood

- **CLI providers** (claude, codex, grok, fx): each turn spawns the vendor CLI headlessly, resumes its session by id, and parses its JSON or stream-JSON output into events. The harness tools reach the CLI through `thread mcp`, an MCP server that the CLI launches. It inherits `THREAD_*` env vars, so it knows which thread it serves, and it routes approval prompts back to your REPL.
- **Built-in loop** (api, ollama, fm, local): thread runs the tool loop itself over Chat Completions (or Ollama's native `/api/chat`), with native function calling and native approvals.

## Code layout

```
src/main.rs         CLI + REPL (focus, switching, approvals)
src/input.rs        REPL line input; ctrl-c seen within 100ms via poll on fd 0
src/picker.rs       choose-one list: raw-mode arrows/j/k on a TTY, numbered fallback otherwise
src/markdown.rs     markdown → ANSI terminal text, word-wrapped to width; NO_COLOR and pipe aware
src/runtime.rs      workers, inbox delivery, approvals, worktrees, resume, shutdown
src/agent.rs        conversation engine: tool loop, steering, nap, handoff
src/zone.rs         zone confinement: path resolution, macOS sandbox profiles
src/consolidate.rs  learn + propagate, surprise scoring, replay, skills, decay
src/memory.rs       facts (corroborate/supersede/contest), skills, recall, decay
src/threads.rs      threads, names, inbox, runs, events
src/tools.rs        bash, editor, memory and thread tools
src/provider.rs     providers: login, status, default, model catalogs, key storage
src/cli.rs          headless claude / codex / grok / fx turns, stream parsers, CliLlm
src/mcp.rs          `thread mcp`: harness tools + approval prompts for the CLIs
src/llm.rs          OpenAI-compatible client (api + local), Ollama native translation, scripted fake
src/db.rs           schema
tests/bdd/          Gherkin features, runner, scriptable mock model
```

## Design choices

- **Short dependency list.** Seven crates: `anyhow`, `clap`, `libc`, `rusqlite` (bundled SQLite), `serde`, `serde_json`, `ureq`. Markdown rendering, the picker and raw input are hand-written so the binary stays small.
- **No daemon.** Everything runs inside the `thread` process you opened. Persistence makes that safe.
- **Plain lines, no TUI.** Works in any terminal, multiplexer, SSH session or pipe. See [terminal support](guide.md#terminal-support).
- **Subscriptions first.** Driving official CLIs means thread never handles vendor credentials.

## Known gaps

- Messages sent to a CLI-backed thread mid-turn arrive when the turn ends, not at the next tool call. api threads take them at the next tool call.
- Output arrives per turn rather than streamed, and printing is plain lines rather than a TUI. Notices can land in the middle of what you're typing.
- Threads only run while a `thread` process is open. There's no daemon.
- Merging a thread's branch is up to you or main (`git merge thread/<name>`).
- Recall is keyword-only (SQLite FTS5 with BM25), with no embeddings.
- The OS-level write sandbox is macOS-only. On other platforms only the file tools are confined.
- No native Windows support.
