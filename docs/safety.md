# Safety

[← README](../README.md) · [Guide](guide.md) · [Commands](commands.md) · [Providers](providers.md) · **Safety** · [Memory](memory.md) · [Configuration](configuration.md) · [Architecture](architecture.md)

Two layers keep agents in check. **Approvals** decide whether an action needs your OK. **The zone** decides where a thread can write at all, and approvals can't override it.

## Approvals

`bash` and file writes ask first: a `y/n` prompt in the focused thread, or a `⚠` notice from other threads. Answer from anywhere with `/y`, `/n` or `/always` (see [Commands](commands.md#in-the-repl)).

What never asks:
- writes inside a thread's own worktree
- reading and web search on Claude (Read, Grep, Glob, WebSearch, WebFetch…)
- the harness's own tools (`recall`, `list_threads`, …)
- tools you've approved with `/always` (saved in `~/.config/thread/config.json`)

Modes:
- **`--yolo`** turns asking off and maps to each CLI's skip-approvals mode. The zone still applies.
- **`thread ask`** has nobody to ask, so it denies anything that would ask, unless you pass `--yolo`.

How each provider asks depends on what its CLI supports headlessly. See the approvals column in [Providers](providers.md).

## The zone

The directory you launch `thread` in is the **zone**. Each thread is locked to the zone it was created in, plus its own worktree.

- The zone is stored with the thread, so a resumed thread keeps its original zone wherever you reopen thread.
- Spawned threads inherit their parent's zone.
- A thread's own worktree counts as inside, wherever it lives.

### How it's enforced

| layer | enforcement |
|---|---|
| file tools | Paths are fully resolved (`..`, symlinks, dangling links, missing tails) and refused outside the zone *before* any approval prompt. `--yolo` and worktree auto-accept can't get around it |
| `bash` | Runs in the zone. On macOS it runs under `sandbox-exec`, which denies writes outside the zone, the git common dir and temp dirs |
| `claude`, `fx` (and `grok` under `--yolo`) | Run under the same sandbox plus their own state dirs, with your user settings files read-only. Claude's approval hook also refuses out-of-zone file paths |
| `codex` | Uses its own `workspace-write` sandbox with the zone as its writable roots instead of the bypass, even under `--yolo` |

### Limits and knobs

- Only **writes** are confined. Reads aren't restricted at the OS level.
- The OS sandbox is **macOS-only**. On Linux, the file-tool checks still apply, but `bash` and the CLIs aren't OS-sandboxed.
- `THREAD_ZONE_WRITABLE`: a `:`-separated list of absolute paths to add as writable, e.g. package caches.
- `THREAD_NO_SANDBOX=1` turns the OS sandbox off.

If a thread needs to work somewhere else, launch `thread` from a directory that contains both locations, or add the path with `THREAD_ZONE_WRITABLE`.
