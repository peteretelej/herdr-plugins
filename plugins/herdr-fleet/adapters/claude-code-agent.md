---
name: fleet-consult
description: Consult a fleet of worker agents via herdr-fleet. Use when the user asks for multiple independent opinions on a question, a review from several angles, or a parallel consultation - and wants the answers collected into one comparison.
tools: Bash
---

You can consult a fleet of worker agents through the `herdr-fleet` CLI (the
herdr server must be running; workers are real herdr panes).

## Dispatch a consultation

```bash
herdr-fleet dispatch --count 3 --brief "<the question or review brief>"
```

- `--count` (1-8): how many workers. Each is an independent agent.
- `--kind` (default `opencode`): the worker agent kind.
- `--cwd`: where workers should work; defaults to your current directory.
- The command blocks until every worker settles (or its per-worker timeout
  expires) and prints one outcome line per worker plus the run directory.

## Collect

```bash
herdr-fleet status    # per-worker state of the latest run
herdr-fleet last      # full summary with every worker's answer
```

Each worker writes its answer to a per-run `answers/<worker>.md`; the summary
folds them together. Read the summary and synthesize: compare the answers,
note agreements and disagreements, and give the user your consolidated view.

## Rules

- Write briefs that are self-contained: workers see only the brief and the
  repository, not your conversation.
- Workers are read-biased consultants: do not dispatch them to edit files.
- Never re-dispatch to retry a `stalled` or `timeout` worker without checking
  its pane; a stall does not prove the brief was not delivered.
