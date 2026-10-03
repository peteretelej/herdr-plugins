# peteretelej.opencode-cost-tokens

Live [OpenCode](https://opencode.ai) session telemetry next to every agent pane
in herdr's sidebar: context-window usage, effective generation rate, session
cost, output tokens, and the model - per pane, refreshed while the agent works.

## Before / after

A sidebar Agent row before:

```text
● webshop | fix-checkout-redirect
```

With the plugin's tokens added to your Agent rows:

```text
● webshop | fix-checkout-redirect
  glm-5.3-flash  cx 13%  10 t/s  $0.00  27.7k out
```

Context turns amber past 60% and red past 85% (severity variants you style in
config), so you see a compaction coming before it interrupts the agent.

Part of [peteretelej/herdr-plugins](https://github.com/peteretelej/herdr-plugins).

## The tokens

| Token | Shows | Example |
|---|---|---|
| `$oct_cx` | Context window used (percentage; absolute tokens when the model's limit is unknown). Warn/hot variants below. | `13%` / `137.1k` |
| `$oct_cx_warn` | Same value, reported instead of `$oct_cx` at 60-84% used. | `64%` |
| `$oct_cx_hot` | Same value, reported at 85%+ used. | `91%` |
| `$oct_tps` | Effective rate of the last completed turn: output tokens over the turn's wall time. | `10 t/s` |
| `$oct_cost` | Accumulated session cost in dollars. | `$1.23` |
| `$oct_out` | Session output tokens. | `52.3k` |
| `$oct_model` | Model id with provider path stripped. | `glm-5.3-flash` |

Exactly one context variant is reported at a time; unreported tokens vanish
from the row, which is what makes the severity swap work.

## Sidebar setup

Add the tokens to `ui.sidebar.agents.rows` in your herdr `config.toml`:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", "workspace", "tab"],
  [
    { token = "$oct_model", dim = true },
    { token = "$oct_cx" },
    { token = "$oct_cx_warn", fg = "#fc0" },
    { token = "$oct_cx_hot", fg = "#f55", bold = true },
    { token = "$oct_tps", dim = true },
    { token = "$oct_cost" },
    { token = "$oct_out", dim = true },
  ],
]
```

## How it works

- **Attribution:** herdr's OpenCode integration reports each pane's session id
  (`agent_session` in `herdr pane list`); the plugin maps panes to sessions
  with one call.
- **Data:** read-only SQLite against OpenCode's local store
  (`~/.local/share/opencode/opencode.db`, v2 schema). Session totals (cost,
  tokens) are aggregate columns; the rate and context numbers come from the
  newest completed assistant message. Nothing is written to OpenCode.
- **Liveness:** hooks reconcile on agent status events, and a detached watcher
  polls working panes every 2s plus a short settle tail, then exits when the
  fleet goes idle. Unchanged panes cost zero writes.
- **Context %:** OpenCode's own formula (input + output + reasoning + cache
  reads/writes of the last completed turn) over the model's context limit from
  OpenCode's cached model catalog.

## Good to know

- **Subscription costs read `$0.00`.** OpenCode records real dollars only for
  metered/API-billed providers; subscription usage (e.g. plan-included models)
  is free at the margin and shows as zero. That is OpenCode's accounting, not
  a bug.
- **`tok/s` is effective, not instantaneous.** OpenCode's store marks the end
  of streaming with a final flush timestamp, so the plugin divides output by
  the turn's full wall time (created to completed). Thinking-heavy turns read
  slower than raw decode speed; that is the rate you actually experienced.
- Mid-stream live deltas would need OpenCode's SSE event stream; deliberately
  out of scope.

## Requirements

- herdr >= 0.9.3
- OpenCode >= 2.0.22 (v2 database schema)
- herdr's OpenCode integration active (it reports the pane-to-session mapping)

## Install

```bash
herdr plugin install peteretelej/herdr-plugins/plugins/opencode-cost-tokens
```

For local development:

```bash
herdr plugin link /path/to/herdr-plugins/plugins/opencode-cost-tokens
herdr server reload-config
```

Build first with `cargo build --release` from the repo root.

## Actions

- `refresh` - reconcile every OpenCode pane now:
  `herdr plugin action invoke peteretelej.opencode-cost-tokens.refresh`
- `stop` - stop the live watcher:
  `herdr plugin action invoke peteretelej.opencode-cost-tokens.stop`

## Privacy

The plugin publishes numbers and model names only. It never reads, stores, or
renders session titles or content.

## License

MIT
