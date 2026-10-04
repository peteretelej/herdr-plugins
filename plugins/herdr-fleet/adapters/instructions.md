# Fleet consultation (generic lead adapter)

Paste this into any lead agent's instructions/system prompt when it should be
able to consult a fleet of worker agents. Requires the `herdr-fleet` binary
(the herdr-plugins suite build output - put it on PATH or use the full path)
and a running herdr server.

---

## Consulting the fleet

You can spawn a fleet of independent worker agents through the `herdr-fleet`
CLI. Workers are real herdr panes: they receive your brief, work
independently, and write their answer to a per-run result file.

Dispatch:

    herdr-fleet dispatch --count 3 --brief "<self-contained brief>" [--cwd DIR] [--kind KIND]

- `--count` (1-8): number of workers. `--kind` (default `opencode`): the
  worker agent kind. `--cwd`: where workers work (default: current directory).
- The command blocks until every worker settles (or its timeout expires) and
  prints one outcome line per worker plus the run directory.

Collect:

    herdr-fleet status    # per-worker state of the latest run
    herdr-fleet last      # full summary with every worker's answer

Each worker's answer lands in the run's `answers/<worker>.md`; `last` prints
the summary that folds them together. Read it and synthesize for the user:
agreements, disagreements, your consolidated view.

Rules:

- Briefs must be self-contained: workers see only the brief and the
  repository, never your conversation.
- Workers are read-biased consultants: do not dispatch them to edit files.
- Never re-dispatch to retry a `stalled` or `timeout` worker without checking
  its pane first; a stall does not prove the brief was not delivered.
