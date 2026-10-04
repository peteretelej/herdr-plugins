# peteretelej.herdr-fleet

Democratized subagents for [herdr](https://github.com/herdrdev/herdr): any
lead agent dispatches kind-agnostic **consultation workers** as real herdr
panes and collects their answers. Every AI harness has subagents built in -
locked to its own runtime, invisible, ephemeral. herdr-fleet makes the same
pattern work across agents, in panes you can watch and jump into, managed by
herdr itself.

## Before / after

| Without herdr-fleet | With herdr-fleet |
|---|---|
| Open 3 panes yourself, paste the brief 3 times, watch each one, copy answers by hand - only works with agents you already have running | `herdr-fleet dispatch --count 3 --brief "Review the auth middleware for races"` - herdr spawns, runs, and manages the workers; you read one summary |

Part of [peteretelej/herdr-plugins](https://github.com/peteretelej/herdr-plugins).

## How it works

One `dispatch`: herdr-fleet creates a scratch workspace with your working
directory (`workspace create --cwd`), one tab per worker (`tab create`),
starts the requested agent kind in each (`agent start`), and delivers the
brief plus a worker protocol - work independently, write your answer to this
exact file, then stop. Each worker is watched to a settled state, classified
into an outcome, and its answer file collected into a per-run summary.

Workers are consultation-flavored: they read, reason, and report. They are
not an isolation story - parallel file-editing workers need worktree
machinery (see herdr-swarm for that lane). Herdr owns the worker lifecycle:
when the run is over the panes stay, and you close them whenever.

## Usage

```bash
herdr-fleet dispatch --brief "Review the auth middleware for races" --count 3
herdr-fleet dispatch --brief "What gates does this repo's CI run?" --cwd ~/code/api
herdr-fleet dispatch --kind claude --count 2 --brief "Rate this API design 1-10 and say why"
herdr-fleet dispatch --brief "..." --no-collect --timeout 120000 --json

herdr-fleet status        # per-worker state of the latest run
herdr-fleet last          # the latest run's full summary
```

Flags: `--kind` (default `opencode`), `--count` (default 3, max 8), `--cwd`,
`--timeout` (default 600000 ms per worker), `--lines` (fallback transcript
tail, default 200), `--no-collect`, `--json`, `--state-dir`.

Per-worker outcomes: `completed` / `blocked` / `stalled` / `timeout` /
`failed` / `skipped` - same taxonomy and guarantees as the run record: a
stalled or timed-out brief is never auto-retried (herdr does not guarantee
non-delivery, so a retry could send it twice), and the exit code is 0 only
when at least one worker received the brief.

Each run writes `summary.md` (the report with every answer) and
`status.jsonl` (one machine-readable record per worker) under
`~/.local/state/herdr/plugins/peteretelej.herdr-fleet/runs/<stamp>/`.

## Using it from a lead agent

The CLI *is* the subagent API for any agent that can run shell commands.
Ship one of the adapters in [`adapters/`](adapters/) to your lead:

- [`claude-code-agent.md`](adapters/claude-code-agent.md) - a Claude Code
  subagent definition
- [`opencode-agent.json`](adapters/opencode-agent.json) - an OpenCode agent
  config snippet
- [`instructions.md`](adapters/instructions.md) - generic paste-into-prompt
  version for anything else

The lead learns to `herdr-fleet dispatch ...` for consultations and
`herdr-fleet status` to collect - no native agent-teams feature required on
either side.

## Install (local development)

```bash
herdr plugin link /path/to/herdr-plugins/plugins/herdr-fleet
herdr server reload-config
```

Build first with `cargo build --release` from the repo root, then run the
binary directly (bare shell runs resolve herdr's plugin state dirs the same
way the manifest actions do; `--state-dir` overrides).

## Actions

- `status` - latest run, per-worker state:
  `herdr plugin action invoke peteretelej.herdr-fleet.status`
- `last` - latest run's summary:
  `herdr plugin action invoke peteretelej.herdr-fleet.last`

## Requirements

- herdr >= 0.9.3 (this suite tracks current stable; no legacy support)
- Workers run real agents (token spend scales with `--count`); the binary is
  the suite build output, so put it on PATH or alias it for lead adapters

## License

MIT
