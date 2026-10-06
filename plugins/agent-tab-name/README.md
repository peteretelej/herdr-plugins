# peteretelej.agent-tab-name

Names herdr tabs (and optionally panes) after a short **topic slug** built
from the running agent's session topic - e.g. `fix-checkout-redirect` instead
of `7. Fix checkout redirect loop`. Manual names are respected; writes are
state-gated so labels only change when the topic actually changed.

## Before / after

| Tab before | Tab after |
|---|---|
| `1` | `fix-checkout-redirect` |
| `2` | `review-auth-middleware` |
| `3` | `add-csv-export` |

The topic part is the slugified session topic, capped to a few words so it
fits herdr's narrow tab strip. Tabs are grouped by workspace and herdr's
agents list already shows the workspace next to each label, so the project
prefix is off by default; to prepend it anyway, set
`tab_format = "{project}-{topic}"` (the `{project}` slot is the workspace
label, or a short form via `project_slugs`).

Part of [peteretelej/herdr-plugins](https://github.com/peteretelej/herdr-plugins).

## How it works

One idempotent reconcile pass runs on agent events (`pane.agent_status_changed`,
`pane.agent_detected`) and on a manual `sync` action. For every tab with an
agent pane, it builds a slug from the agent's `terminal_title_stripped` and
sets the tab label. Writes are gated through a state file, so a rename only
happens when the topic actually changed. Labels you set manually are never
touched.

## Install (local development)

```bash
herdr plugin link /path/to/herdr-plugins/plugins/agent-tab-name
herdr server reload-config
```

Build first with `cargo build --release` from the repo root.

## Actions

- `sync` - run a reconcile pass now:
  `herdr plugin action invoke peteretelej.agent-tab-name.sync`
- `release` - stop managing labels and forget state:
  `herdr plugin action invoke peteretelej.agent-tab-name.release --clear`

## Configuration

Optional TOML at `$(herdr plugin config-dir peteretelej.agent-tab-name)/config.toml`:

```toml
# Rename panes as well as tabs (default: tabs only)
rename_panes = false
# Label template. Tokens: {project} {topic} {agent} {n}
# Default is topic-only; "{project}-{topic}" prepends the workspace label.
tab_format = "{topic}"
# Truncate labels to this many characters (herdr tab strips are narrow)
max_len = 28
# Topic prefixes to strip (e.g. OpenCode's "OC | ")
strip_prefixes = ["OC | "]
# Keep only the first N words of the topic slug
max_topic_words = 3
# Short forms for workspace labels
project_slugs = { "webshop" = "shop" }
```

Example: session "Fix checkout redirect loop" in the `webshop` workspace
produces `fix-checkout-redirect`; with `tab_format = "{project}-{topic}"` and
`project_slugs = { "webshop" = "shop" }` it becomes
`shop-fix-checkout-redirect`.

Manual renames are always respected: a label is adopted only if it is unset, a
value this plugin wrote earlier, or exactly what the plugin would write now.

## Requirements

- herdr >= 0.9.3 (this suite tracks current stable; no legacy support)
- A recognized agent whose terminal title carries the topic (OpenCode does)

## License

MIT
