# peteretelej.tab-topic

Name herdr tabs (and optionally panes) after the live topic of the agent
running inside them - the same session title OpenCode already reports via its
terminal title. No more tabs named `1`, `2`, `3`.

Part of [peteretelej/herdr-plugins](https://github.com/peteretelej/herdr-plugins).

## How it works

One idempotent reconcile pass runs on agent events (`pane.agent_status_changed`,
`pane.agent_detected`) and on a manual `sync` action. For every tab with an
agent pane, it sets the tab label to the agent's `terminal_title_stripped`.
Writes are gated through a state file, so a rename only happens when the topic
actually changed. Labels you set manually are never touched.

## Install (local development)

```bash
herdr plugin link /path/to/herdr-plugins/plugins/tab-topic
herdr server reload-config
```

Build first with `cargo build --release` from the repo root.

## Actions

- `sync` - run a reconcile pass now:
  `herdr plugin action invoke peteretelej.tab-topic.sync`
- `release` - stop managing labels and forget state:
  `herdr plugin action invoke peteretelej.tab-topic.release`

## Configuration

Optional TOML at `$(herdr plugin config-dir peteretelej.tab-topic)/config.toml`:

```toml
# Rename panes as well as tabs (default: tabs only)
rename_panes = false
# Label template. Tokens: {topic} {agent} {n} (tab switch number)
tab_format = "{n}. {topic}"
# Truncate labels to this many characters
max_len = 60
# Topic prefixes to strip (e.g. OpenCode's "OC | ")
strip_prefixes = ["OC | "]
```

Manual renames are always respected: a label is adopted only if it is unset, a
value this plugin wrote earlier, or exactly what the plugin would write now.

## Requirements

- herdr >= 0.9.3 (this suite tracks current stable; no legacy support)
- A recognized agent whose terminal title carries the topic (OpenCode does)

## License

MIT
