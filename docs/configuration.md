# Configuration

[← README](../README.md) · [Guide](guide.md) · [Commands](commands.md) · [Providers](providers.md) · [Safety](safety.md) · [Memory](memory.md) · **Configuration** · [Architecture](architecture.md)

## Environment variables and flags

### General

| env / flag | default |
|---|---|
| `--provider` / `THREAD_PROVIDER` / `thread use` | first logged-in CLI (see [Providers](providers.md#choosing-a-provider)) |
| `THREAD_MODEL` / `--model` | the provider's own default (api: the model picked at login) |
| `THREAD_MEMORY_PROVIDER` | the default provider |
| `THREAD_MEMORY_MODEL` / `--memory-model` | the thread's model |
| `THREAD_EFFORT` / `--effort` | unset (api only, sent as `reasoning_effort`) |
| `THREAD_LLM_TIMEOUT` | seconds: `1800` for local providers, `600` otherwise |
| `NO_COLOR` | unset; any value turns off ANSI styling |

### Safety

| env | default |
|---|---|
| `THREAD_ZONE_WRITABLE` | extra writable dirs for sandboxed commands (`:`-separated absolute paths) |
| `THREAD_NO_SANDBOX` | unset; `1` turns off the macOS sandbox |

### Claude

| env | default |
|---|---|
| `THREAD_CLAUDE_SETTINGS` | `user,project,local` (setting sources Claude threads load) |

### api

| env | default |
|---|---|
| `OPENAI_BASE_URL` | `https://api.openai.com/v1`, or what `thread login api` saved |
| `OPENAI_API_KEY` | what `thread login api` saved |
| `THREAD_API_MODEL` | the model picked at login |

### Local models

| env | default |
|---|---|
| `OLLAMA_HOST` | `127.0.0.1:11434` |
| `THREAD_OLLAMA_MODEL` | newest installed model with tool calling |
| `THREAD_OLLAMA_NUM_CTX` | `32768` |
| `THREAD_FM_URL` | `http://127.0.0.1:1976/v1` (`fm serve`, started on demand) |
| `THREAD_FM_MODEL` | `system` |
| `THREAD_LOCAL_URL` | what `thread login local` saved, else the first of `:1234`, `:8080`, `:8000` that answers |
| `THREAD_LOCAL_MODEL`, `THREAD_LOCAL_API_KEY` | optional |
| `THREAD_CONTEXT_WINDOW` | fm: `8192`; ollama: its num_ctx. Sets how early local threads nap |

## Files

| path | contents |
|---|---|
| `~/.config/thread/config.json` | default provider (`thread use`), `/always` tools, `fm_url`, `local_url` |
| `~/.config/thread/credentials.json` | api base URL, key and default model from `thread login api` (mode 600) |
| `~/.config/thread/models.json` | model catalog cache (12h) |
| `<repo>/.thread/thread.db` | threads, conversations, runs, events, memory (SQLite) |
| `<repo>/.thread/worktrees/<name>` | each thread's worktree, on branch `thread/<name>` |

`.thread/` is added to `.git/info/exclude` automatically, so it never shows up in `git status`. API keys saved by `thread login api` are written with mode 600.

## Request defaults

API requests use adaptive thinking, automatic prompt caching, and server-side refusal fallback.
