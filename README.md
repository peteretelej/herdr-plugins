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

### [herdr-fleet](plugins/herdr-fleet/)

Democratized subagents: any lead agent dispatches kind-agnostic consultation
workers as real herdr panes and collects their answers. One command spawns a
worker fleet into a scratch workspace, delivers a self-contained brief, and
folds the worker-written answers into one summary - visible, persistent, and
managed by herdr rather than locked inside any single AI's runtime.

```bash
herdr plugin install peteretelej/herdr-plugins/plugins/herdr-fleet
```

Requires herdr >= 0.9.3. `dispatch` is a shell tool (invoke the built binary
from a shell pane); the `status`/`last` actions re-print run state.

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
