# Contributing

[← README](README.md) · [Architecture](docs/architecture.md)

Thanks for helping out. thread is small on purpose: one binary, a short dependency list, plain terminal output. Please keep it that way.

## Setup

You need macOS or Linux, a Rust toolchain (edition 2024, so a recent stable), git, and Python 3 for the acceptance tests.

```bash
git clone https://github.com/Phytonking/thread && cd thread
cargo build --release
```

If `cargo` isn't on your PATH after installing with Homebrew's `rustup`, run `rustup default stable` or add `~/.cargo/bin` to your PATH.

## Before you open a PR

All of these must pass:

```bash
cargo fmt --check
cargo clippy                      # no warnings
cargo test                        # unit tests
cargo build --release
python3 tests/bdd/run.py          # offline acceptance scenarios, no accounts needed
```

If your change touches a CLI provider and you have that subscription, also run the live scenarios:

```bash
python3 tests/bdd/run.py --tags live            # or --tags claude, etc.
```

Useful runner options: `-k <text>` runs scenarios whose name matches, `-v` prints thread's output for failures, and `BDD_WAIT=<seconds>` changes the per-step timeout (default 20). `THREAD_BIN` points the runner at another binary.

## Tests

thread has two layers of tests, and most changes need both.

**Acceptance tests** (`tests/bdd/features/*.feature`) are the user requirements. Each feature opens with an "As a / I want / So that" story, and each scenario describes behavior a user can see:

```gherkin
@offline
Feature: Simple commands
  As a developer
  I want a small, predictable set of commands
  So that I don't have to learn a manual to use thread

  Background:
    Given a git repository
    And the model server is running
    And I open thread

  Scenario: Unknown commands point to help
    When I type "/frobnicate"
    Then I see "unknown command /frobnicate"
```

- `tests/bdd/run.py` is a stdlib-only runner. It drives the real binary in a fresh git repo and config dir per scenario.
- `tests/bdd/mock_llm.py` is a scriptable OpenAI-compatible model server, so scenarios need no accounts.
- Step definitions are `@step(r"...")` functions in `run.py`. Reuse existing steps before adding new ones.
- Tag scenarios `@offline` (runs by default) or `@live` plus the provider tag (needs a real subscription).
- Every user-visible change needs a scenario. Every bug fix needs a scenario that failed before the fix.

**Unit tests** live next to the code (`#[cfg(test)]` modules) and run against a scripted fake model (`src/llm.rs`). Use them for parsing, policies, command-line construction and anything with edge cases. For example, when you change how a CLI is invoked, assert the exact arguments.

**Try it by hand, offline:**

```bash
python3 tests/bdd/mock_llm.py 8765 &          # understands "spawn thread x: task", "run: cmd", "remember e a v"
OPENAI_BASE_URL=http://127.0.0.1:8765/v1 thread --provider api
```

## Code conventions

- **rustfmt and clippy clean.** No exceptions without a comment saying why.
- **Dependencies are a last resort.** The current list is seven crates. Prefer a small hand-written module (see `markdown.rs`, `picker.rs`) over a new crate, and explain any new dependency in the PR.
- **Plain terminal output.** No TUI frameworks and no alternate screen. Output must stay readable when piped (`markdown.rs` passes text through untouched) and respect `NO_COLOR`.
- **Errors say what to do.** User-facing errors name the fix, e.g. "not logged in — thread login fx".
- **Safety is not optional.** Anything that writes files or runs commands must go through the zone (`src/zone.rs`) and the approval path. `--yolo` may skip approvals but never the zone.
- **Module docs.** Start a new file with a `//!` comment explaining what it owns and why.

See [Architecture](docs/architecture.md) for where things live.

## Adding a provider

Use the fx commit (`a7eb535`) as a worked example. A new CLI provider touches:

1. `src/provider.rs`: `Provider` variant and aliases, install and login checks, `thread login`, model catalog, default-provider order.
2. `src/cli.rs`: headless invocation, session resume, output parsing into events, permission-mode mapping (Ask / Deny / Yolo). Add unit tests that assert the exact arguments and env for each mode.
3. MCP registration so the CLI can reach `thread mcp`, and `/mcp add|rm` mirroring if the CLI keeps its own config.
4. `src/zone.rs`: run it under the sandbox with its state dirs writable.
5. Acceptance scenarios: offline where the mock can cover it, plus `@live @<provider>` ones.
6. Docs: [Providers](docs/providers.md), [Configuration](docs/configuration.md), [Commands](docs/commands.md) and the README if needed.

## Documentation

Docs are part of the change. If you add or change a command, flag, env var, provider or behavior, update the matching page in `docs/` in the same PR:

| change | update |
|---|---|
| command, flag, REPL input, agent tool | [docs/commands.md](docs/commands.md) |
| provider behavior | [docs/providers.md](docs/providers.md) |
| approvals, zone, sandbox | [docs/safety.md](docs/safety.md) |
| memory policy | [docs/memory.md](docs/memory.md) |
| env var or config file | [docs/configuration.md](docs/configuration.md) |
| new module, known gap | [docs/architecture.md](docs/architecture.md) |
| workflow or UX | [docs/guide.md](docs/guide.md) |

Keep the README short: pitch, quick start, links.

## Commits and pull requests

- Branch off `main`. One logical change per PR.
- Commit subjects are imperative and describe the user-visible effect, e.g. "Close runs whose handoff fails so ask and shutdown finish promptly".
- The commit body explains *why* and lists notable behavior, like the zone and fx commits do.
- In the PR description, say what you tested: unit, offline BDD, and which live providers, if any.
