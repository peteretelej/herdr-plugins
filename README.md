# herdr-plugins

Open-source suite of [herdr](https://github.com/herdrdev/herdr) plugins - one
directory per plugin under `plugins/`, installable individually.

## Plugins

### [agent-tab-name](plugins/agent-tab-name/)

Names herdr tabs after a short project-feature slug built from the running
agent's session topic - e.g. a tab that would otherwise just say `1` becomes
`webshop-fix-checkout-redirect`. Manual names are respected; writes are
state-gated so labels only change when the topic actually changes.

```bash
herdr plugin install peteretelej/herdr-plugins/plugins/agent-tab-name
```

Requires herdr >= 0.9.3 and a recognized agent whose terminal title carries a
topic (OpenCode does).

### [opencode-cost-tokens](plugins/opencode-cost-tokens/)

Live OpenCode session telemetry next to every agent pane in herdr's sidebar:
context-window usage (amber past 60%, red past 85%), last-turn generation
rate, and session cost when it exists. Three numbers; session titles and
content are never read or rendered.

```bash
herdr plugin install peteretelej/herdr-plugins/plugins/opencode-cost-tokens
```

Requires herdr >= 0.9.3, OpenCode >= 2.0.22, and herdr's OpenCode integration.

## Tab-bar status widgets (scripts)

Two shell scripts that turn herdr's built-in `ui.tab_bar_right` status area
into a live fleet dashboard - no plugin install, just the scripts and config
entries. A tab row that only showed a clock becomes:

```text
1:webshop-fix-checkout-redirect   10:04 · AI: 6 idle, 2 active, 2 blocked, 1 done · AAPL 333.69
```

### Setup

Reference the scripts from your herdr `config.toml` (adjust the path to where
you keep this repo):

```toml
[ui]
tab_bar_right = [
  { type = "datetime", format = "%a %H:%M" },
  { type = "command", command = "~/code/peteretelej/herdr-plugins/scripts/fleet-status", interval_seconds = 10, timeout_seconds = 10 },
  { type = "command", command = "~/code/peteretelej/herdr-plugins/scripts/ticker AAPL MSFT", interval_seconds = 60, timeout_seconds = 20 },
]
tab_bar_right_separator = " · "
```

- `scripts/fleet-status` counts agents by state via `herdr agent list`
  (`working` is shown as `active`; zero-count states are omitted).
- `scripts/ticker` shows stock prices. Keyless by default (Yahoo's public
  chart endpoint), or export `FINNHUB_API_KEY` to use the official Finnhub
  quote API instead. Symbols come from the command line and default to
  `AAPL`.

Both scripts print nothing on failure, and herdr clears a command entry whose
output is empty - so the status area never shows stale counts or prices.
Entries resolve on the herdr server, so the values follow the server when you
attach with `herdr --remote`. Requires `jq` (and `curl` for the ticker).

## Development

The plugins share a cargo workspace:

```bash
cargo build --release          # builds all plugins to ./target/release/
herdr plugin link $PWD/plugins/tab-topic
herdr server reload-config
```

Event hooks and actions in each `herdr-plugin.toml` reference the built binary
relative to the plugin directory.

In case of compatibility issues with older versions (eg of herdr or opencode), prefer supporting the newer stable versions.

## Suite principles

- Track current stable herdr; no legacy-version support
- Idempotent reconciles, state-file gating, manual names always respected
- Per-plugin MIT license

## License

MIT - see [LICENSE](LICENSE).
