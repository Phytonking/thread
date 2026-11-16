# Guide

[← README](../README.md) · **Guide** · [Commands](commands.md) · [Providers](providers.md) · [Safety](safety.md) · [Memory](memory.md) · [Configuration](configuration.md) · [Architecture](architecture.md)

This guide walks through how thread is meant to be used. For the full list of inputs, see [Commands](commands.md).

## The mental model

- **A repo has one `main`.** Running `thread` in a repo opens main, a persistent conversation for that repo that picks up where you left off.
- **main delegates.** When you ask for work, main spawns **background threads** with readable names (`flaky-search-tests`, `login-rate-limit`). Each has its own conversation and, when it edits code, its own git worktree on branch `thread/<name>`.
- **You stay in control.** You can switch to any thread, talk to it mid-run, answer its approvals, and stop it. Threads you aren't looking at post one-line notices instead of flooding your screen.
- **Threads report back.** When a background thread finishes, its report goes to main, and main tells you what happened and what to do next (usually: review or merge a branch).

## 1. Set up once

```bash
thread login claude      # uses `claude auth login`; codex, grok and fx work the same way
thread login ollama      # optional: free local models, no key
thread status            # shows what's installed, logged in, and the default
thread use claude        # optional: pin the default provider
```

You can log in to several providers and mix them freely. See [Providers](providers.md).

## 2. Open main and delegate

```bash
cd ~/code/my-app
thread
```

Talk to main the way you'd brief a teammate:

```
main› spawn a thread to find why the search tests are flaky
main› have codex add rate limiting to /api/login, and have opus review it when it's done
main› what's everyone working on?
```

main answers small questions itself and spawns threads for real work. You can name the provider and model in plain words ("with codex", "on haiku", "have ollama do it").

## 3. Watch and steer

```
main› /threads                       # ▸ marks focus, ⚠ marks a pending approval
main› /switch flaky                  # prefix or substring match is enough
flaky-search-tests› also check whether it only fails on CI
flaky-search-tests› /main            # back to main; the thread keeps running
```

`/switch` with no name opens a picker (↑/↓ or j/k, a number, Enter). Messages to a running thread steer it: api and local threads read them at the next tool call, and CLI threads read them when their current turn ends.

Thinking steps print as a single dim `∴` line. `/expand` opens the latest one, and `/expand all` shows every step in full.

## 4. Answer approvals

Shell commands and file writes outside a thread's own worktree ask first:

```
main› ⚠ [login-rate-limit] wants to bash `cargo test -p api` — /y login-rate-limit or /n login-rate-limit
main› /y login           # approve without switching
main› /always login      # approve, and never ask about this tool again
main› /y all             # approve everything pending
```

In the focused thread, plain `y`, `n` and `a` work too. See [Safety](safety.md) for what never asks and what `--yolo` changes.

## 5. Review, merge, clean up

When a thread finishes, its work is on its branch:

```bash
git log thread/flaky-search-tests
git diff main...thread/flaky-search-tests
```

Merge it yourself, or ask main to ("merge flaky-search-tests"). Then `thread rm flaky-search-tests` closes the thread and removes its worktree. The branch is kept.

## 6. Leave and come back

`exit` (or ctrl-c on main) hands off idle threads. If some are still running, thread asks whether to leave them or wait. Leave them and they resume on the next `thread` launch. Every step is persisted, so nothing is lost.

```bash
thread                 # back to main
thread flaky           # open straight onto a thread
thread -r              # pick any thread, closed ones included
thread ls              # status, age, branch and progress of each thread
thread show flaky      # task, state, latest output, run history
```

## Everyday workflows

**Mix models by strength.** Plan with one model, implement with another, review with a third:

```
main› have opus write a plan for the export feature, then have codex implement it on its own branch, then have sonnet review the diff
```

**Move a thread to another model** without losing its state: `/provider codex`, `/model haiku`, or ask main ("move the docs thread to ollama"). Switching provider starts a fresh session, but state and memory carry over.

**Run shell commands alongside.** `! cargo test` runs in the focused thread's directory (its worktree, if it has one). The output is attached to your next message, so you can follow with "fix that".

**Use free local models** for cheap tasks: `thread use ollama`, or "have ollama write the changelog". See [local models](providers.md#local-models).

**Teach it facts.** `thread remember api auth "uses JWT in the Authorization header"` stores a trusted fact every thread can recall. `thread recall jwt` searches memory. See [Memory](memory.md).

## Scripting and CI with `thread ask`

`thread ask` sends one message, waits for everything to settle (spawned threads included), and exits. Nobody is there to approve, so tool calls that would ask are denied unless you pass `--yolo`. The zone still applies, so writes stay inside the directory you ran it in.

```bash
thread ask "summarize what changed on this branch"
thread ask --to flaky-search-tests "status?"
thread ask --yolo "run the tests and fix any failures"
```

When stdout isn't a terminal, replies pass through as raw markdown, ready to pipe into files or other tools.

## Terminal support

thread doesn't use a TUI framework. It prints plain lines and keeps its dependency list short, so it behaves the same wherever a shell runs:

- **Any terminal emulator:** Terminal.app, iTerm2, Ghostty, Kitty, WezTerm, Alacritty, and the VS Code / JetBrains / Pane terminals.
- **Multiplexers and remote:** tmux, screen, and SSH sessions. There's no alternate screen to fight with, and scrollback stays normal.
- **Readable output:** model replies are rendered from markdown (headings, emphasis, code, lists, quotes, links, tables) with ANSI styling, and word-wrapped to your terminal width (100 columns max) with real newlines. Copying text out doesn't glue words together at wrap points.
- **`NO_COLOR`:** set it to get the same layout with no escape codes.
- **Pipes and scripts:** when output isn't a TTY, text passes through unchanged. Pickers fall back to a numbered list and a typed answer.
- **Keys:** arrow keys or j/k in pickers, and ctrl-c to step back. Thread commands run in their own process group, so ctrl-c never kills their work.

**Platforms:** macOS and Linux. Raw input uses `stty`, and the OS write sandbox is macOS-only (see [Safety](safety.md)). Windows isn't supported natively. WSL may work but is untested.

**Known rough edges:** output arrives per turn rather than token-streamed, and a notice from another thread can land in the middle of a line you're typing. See [known gaps](architecture.md#known-gaps).
