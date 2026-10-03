# peteretelej.broadcast

Sends one prompt to N running herdr agents and collects the answers when they
settle - tmux `synchronize-panes` for agents. Every agent gets its own
outcome, so one blocked pane never blocks the fleet.

## Before / after

| Without broadcast | With broadcast |
|---|---|
| Paste the same prompt into 5 panes, watch each one, scroll back and copy each answer by hand | `broadcast run "Review the auth middleware for races" --squad review`, then read one summary |

Part of [peteretelej/herdr-plugins](https://github.com/peteretelej/herdr-plugins).

## How it works

One `run` invocation: lists agents (`herdr agent list`), filters to the ones
that can take a prompt right now (idle or done), prompts the rest of your
selection in parallel (`agent prompt --wait`, one process per agent), and
classifies each outcome. Settled agents get their transcript tail collected
(`agent read --source recent-unwrapped`) and everything lands in a per-run
summary you can re-open with the `last` action.

Agents that are working or blocked are reported as skipped - their prompts
are never queued behind another turn. A stalled or timed-out prompt is
flagged as unconfirmed instead of silently retried, so this plugin can never
send the same prompt twice.

## Usage

```bash
broadcast run "<prompt>" --agents reviewer,auditor      # by name or pane id
broadcast run "<prompt>" --workspace api                # a whole workspace (id or label)
broadcast run "<prompt>" --all                          # every promptable agent
broadcast run "<prompt>" --squad review                 # a named squad
broadcast run "<prompt>" --no-collect --timeout 120000  # outcomes only, 2 min wait
```

Per-agent outcomes:

| Outcome | Meaning |
|---|---|
| `completed` | Settled idle/done; answer collected into `answers/` |
| `blocked` | Needs attention: already blocked (prompt not sent) or settled blocked after delivery - the detail says which |
| `stalled` | No activity within 5s of submission; delivery unconfirmed - check the pane before re-sending |
| `timeout` | No settled state within `--timeout` (default 10 min) |
| `failed` | Rejected for another reason (detail in the run record) |
| `skipped` | Not prompted: working, blocked, or unknown at run time |

Exit code is 0 when at least one prompt reached an agent.

Each run writes `summary.md` (markdown report with every answer) and
`status.jsonl` (one machine-readable record per agent) under the run state
directory; `--json` prints the same record to stdout.

## Squads

`$(herdr plugin config-dir peteretelej.broadcast)/squads.toml`, one squad per
line:

```toml
review = "reviewer, auditor"
docs = "docs-writer, api-docs"
```

## Install (local development)

```bash
herdr plugin link /path/to/herdr-plugins/plugins/broadcast
herdr server reload-config
```

Build first with `cargo build --release` from the repo root, then run the
binary directly: `./target/release/broadcast run "..." --agents a,b`. Bare
shell runs resolve herdr's plugin state/config directories the same way the
manifest-invoked action does; `--state-dir` overrides the run directory root.

## Actions

- `last` - print the most recent run's summary:
  `herdr plugin action invoke peteretelej.broadcast.last`

## Requirements

- herdr >= 0.9.3 (this suite tracks current stable; no legacy support)
- Full-screen agents (OpenCode, Claude Code) must be idle for answer
  collection - herdr pages their scroll UI, which is only safe when idle

## License

MIT
