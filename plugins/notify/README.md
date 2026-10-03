# Agent Notify

A [herdr](https://herdr.dev) plugin that sends a Telegram message when an
agent turns `blocked` or `done`, so you can leave the terminal and still
catch the moments that need you.

**Before** - the agent finishes (or stalls) while you are in another tab,
another workspace, or away from the machine. You find out when you happen to
look.

**After** - your phone buzzes:

```text
✅ opencode is done (reported idle)
webshop / api: fix checkout redirect loop
/home/peter/code/webshop
```

```bash
herdr plugin install peteretelej/herdr-plugins/plugins/notify
```

Requires herdr >= 0.9.3. Linux and macOS.

## Why another notify plugin

Two things make naive `done`-only hooks feel broken; both are fixed here.

- **Watched panes.** herdr reports a finished turn as `done` only while the
  completion is unseen. When the pane sits in the tab you are looking at,
  the same moment arrives as `idle`, and a plain status filter silently
  drops it. This plugin keeps the previous status per pane and treats
  `working|blocked -> idle` as a completion too.
- **Slow hooks break herdr.** Blocking on a network call inside an event
  hook delays event dispatch (herdr reports `events_lost` on 0.9.2+). The
  hook here only decides, debounces, and formats; delivery runs in a
  detached process, so a slow Telegram API can never stall your session.

On top of that: flapping agents (working -> blocked -> working) are debounced
per pane and state, quiet hours keep the night calm (`blocked` always gets
through), and per-workspace filters decide which projects may interrupt you.

## Setup

```bash
herdr plugin config-dir peteretelej.notify    # prints the config directory
```

Write `notify.toml` there:

```toml
# Optional; defaults shown.
debounce_secs = 30
notify = ["blocked", "done"]
# Empty include list = all workspaces. Exclude wins over include.
include_workspaces = []
exclude_workspaces = []
# "HH:MM-HH:MM", local time, overnight ranges wrap. blocked ignores these.
quiet_hours = ["22:00-07:00"]

[telegram]
bot_token = "123456:ABC-DEF..."
chat_id = "42"
# Deliver without sound (disable_notification).
silent = false
```

Then verify credentials end to end:

```bash
herdr plugin action invoke peteretelej.notify.test
```

## Messages

Plain text, everything you need to triage from your phone:

| Field | Example | Source |
|-------|---------|--------|
| agent + state | `⛔ opencode is blocked` | pane event, enriched via `herdr pane get` |
| workspace / tab | `webshop / api` | invocation context |
| topic | `fix checkout redirect loop` | pane terminal title |
| cwd | `/home/peter/code/webshop` | pane info, falling back to workspace |

The reported state is included when it differs from the meaning (e.g.
`is done (reported idle)`), so watched-pane completions are never confusing.
Telegram shows the receive time itself, so no timestamp is added.

## Actions

| Action | What it does |
|--------|--------------|
| `test` | Sends a test message; fails loudly if credentials are missing. |
| `mute` | Silences all notifications (state survives restarts). |
| `resume` | Ends a mute. |

```bash
herdr plugin action invoke peteretelej.notify.mute
herdr plugin action invoke peteretelej.notify.resume
```

## How it works

1. The `pane.agent_status_changed` hook recognizes attention states
   (`blocked`, `done`, and active -> `idle` completions), applies filters,
   quiet hours, and a per-pane debounce, then renders the message.
2. It hands the message to a detached `notify send` process and exits 0
   immediately. The event hook never touches the network.
3. The sender calls `sendMessage` with a 5s timeout and retries once,
   honoring Telegram's `retry_after` on 429. Failures land in
   `notify.log` inside the plugin state dir; herdr is never disturbed.

## Development

```bash
cargo build --release -p peteretelej-notify
herdr plugin link $PWD/plugins/notify
cargo test -p peteretelej-notify
```

## License

MIT - see the repo root [LICENSE](../../LICENSE).
