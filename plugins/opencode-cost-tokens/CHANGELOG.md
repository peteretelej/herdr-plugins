# Changelog

## 0.1.0 - 2026-10-03

Initial release.

- Per-pane OpenCode sidebar tokens: context % (with warn/hot severity
  variants), last-turn effective tok/s, session cost, output tokens, model.
- Attribution via herdr's OpenCode integration (`herdr pane list` session ids).
- Read-only SQLite access to OpenCode's v2 local store.
- Event-triggered reconcile plus a detached watcher that polls only while
  panes are working (2s cadence, 20s settle tail, idle exit).
- Actions: `refresh`, `stop`.
