# Commands

[← README](../README.md) · [Guide](guide.md) · **Commands** · [Providers](providers.md) · [Safety](safety.md) · [Memory](memory.md) · [Configuration](configuration.md) · [Architecture](architecture.md)

## CLI

| command | what it does |
|---|---|
| `thread` | open main for the current repo |
| `thread <name>` / `thread open <name>` | open focused on a thread. Names match exactly, then by prefix, then by substring (`thread flaky`) |
| `thread --resume` / `-r` | pick any of this repo's threads to open, closed ones included |
| `thread ask "<msg>" [--to <name>]` | non-interactive: send one message, wait for everything (spawned threads too) to settle, exit |
| `thread login [provider]` | log in through the vendor CLI, store an API key, or set up a local model. No argument shows status |
| `thread status` | providers: installed, logged in, default |
| `thread use <provider>` | set the default provider |
| `thread ls [--all]` | threads with status, age, branch and progress |
| `thread show <name>` | task, state, latest output, runs with surprise scores |
| `thread stop <name>` | stop a thread |
| `thread rm <name>` | close a thread and remove its worktree (the branch is kept) |
| `thread recall "<query>"` | search memory |
| `thread facts [--all]` | list facts (`--all` adds superseded and cold ones) |
| `thread skills` | list learned skills |
| `thread remember <entity> <attribute> <value>` | assert a trusted fact |
| `thread consolidate` | run consolidation now (normally runs after every handoff) |
| `thread log` | audit log of memory changes |
| `thread mcp` | the harness's MCP server. CLI providers launch it; you don't run it yourself |

### Global flags

| flag | |
|---|---|
| `--provider <name>` | provider for this session (see [Providers](providers.md)) |
| `--model <name>` | model for the default provider |
| `--memory-model <name>` | model for memory calls |
| `--effort <level>` | api only, sent as `reasoning_effort` |
| `--yolo` | run every tool without asking. Maps to each CLI's skip-approvals mode. The zone still applies (see [Safety](safety.md)) |

## In the REPL

| input | |
|---|---|
| plain text | talk to the focused thread. Ask main to start, stop or steer threads |
| `y` / `n` / `a` | answer the focused thread's approval (`a`: allow, and never ask about that tool again) |
| `/y` `/n` `/always` `[name\|all]` | answer approvals from any thread without switching: a named thread, every pending one (`all`), or with no name the focused thread's, else the oldest. `/always` saves the tool to `~/.config/thread/config.json` for every thread |
| `/switch [name]` | focus another thread (`main` goes back). With no name, pick from a list (↑/↓, j/k, a number, Enter; Esc cancels). Aliases: `/thread`, `/t` |
| `/main` | back to main |
| `/threads` | list threads (`▸` focus, `⚠` waiting for approval) |
| `/model [number\|name]` | live model menu for this thread's provider, or pick one (`default` resets, `refresh` re-pulls) |
| `/provider [name [model]]` | list providers with login status, or move this thread to another provider, optionally straight to a model |
| `/mcp` · `/mcp add <name> <command…\|url>` · `/mcp rm <name>` | your MCP servers, given to every claude, codex, grok and fx thread |
| `/expand [n\|all]` · `/collapse` | thinking steps print as one dim `∴` line. `/expand` opens the latest, `/expand 3` the third-latest, `/expand all` prints every step in full from now on. Steps are kept per thread |
| `! <command>` | run a shell command in the focused thread's directory. Output prints live and is attached to your next message |
| `/clear` | clear the screen and scrollback |
| `/help` | the command list |
| no slash | `switch probe`, `threads`, `main`, `clear`, `help` work as commands when typed alone. Anything longer, like `switch to plan B`, is sent as a message |
| `ctrl-c` | in a background thread: back to main. On main: same as `exit`. A second ctrl-c during handoff quits at once, and unfinished work resumes next launch |
| `exit` | quit. Idle threads hand off. If some are still running, choose to leave them (they resume next launch) or wait |

## Tools the agent can call

| who | tools |
|---|---|
| every thread | `bash`, the text editor, `recall`, `remember`, `checkpoint`, `list_threads` |
| main | `spawn_thread(name, task, worktree, provider, model)`, `message_thread`, `read_thread`, `stop_thread`, `set_thread_model` |
| background threads | `notify_main`, for something main needs before the thread finishes |

API and local providers get these as native function calls. CLI providers (claude, codex, grok, fx) get them via the `thread mcp` server.
