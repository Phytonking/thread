# Memory

[← README](../README.md) · [Guide](guide.md) · [Commands](commands.md) · [Providers](providers.md) · [Safety](safety.md) · **Memory** · [Configuration](configuration.md) · [Architecture](architecture.md)

thread keeps a per-repo memory of **facts** (entity, attribute, value) and **skills** (procedures for task types that keep succeeding). It's designed to be cheap: facts are learned by the model that already has the context, at moments when it's already making a call. Nothing re-reads a transcript unless a run was surprising.

## When memory is written

| when | what | cost |
|---|---|---|
| `remember` (any time) | fact stored as tentative | free |
| nap (context > 120k) | summary, facts and state saved; the conversation restarts from the summary | 1 call |
| handoff (end of run) | state, outcome, task type, summary, facts | 1 call |
| consolidate (after each handoff) | surprise score per run; replay only for runs scoring ≥ 0.6 (max 3); a skill once a task type has 3 successes; decay | usually 0 calls |

Local providers with small windows nap earlier. See [Providers](providers.md#local-models).

## Trust rules

- Facts from agents start **tentative**. They become **active** once a second run agrees.
- A single run can't overturn a fact that two runs agree on. The new value waits as a tentative **challenger**.
- **Superseded** facts are kept, and every open thread that used the old value gets a `[from memory]` notice right away.
- Facts you assert with `thread remember` are trusted immediately.
- Unused facts go **cold**: they're kept but no longer recalled.

## Skills

- A skill is proposed once a task type has 3 successful runs.
- Skills start tentative, are promoted after 2 successful uses, and retire if they mostly fail.

## Surprise

Surprise decides which runs get replayed. It uses outside signals only:
- outcome compared with the task type's track record
- duration
- retries after failed tools
- tool error rate
- user corrections ("no, …", "instead", "revert")
- whether the task type is new

## Recall

Recall is keyword search (SQLite FTS5 with BM25), with no embeddings. Threads call `recall` themselves. You can search with `thread recall "<query>"`.

## Commands

`thread recall`, `thread facts [--all]`, `thread skills`, `thread remember`, `thread consolidate`, `thread log`. See [Commands](commands.md#cli).
