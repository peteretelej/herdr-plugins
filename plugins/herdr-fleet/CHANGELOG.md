# Changelog

## 0.1.0 - 2026-10-04

- Pivot from the broadcast v0 frame (prompt pre-existing panes) to a
  democratized subagent layer: `dispatch` spawns N kind-agnostic consultation
  workers as herdr panes (workspace create -> tab create -> agent start ->
  brief protocol), collects worker-written result files with a capped
  transcript fallback, and writes per-run `summary.md` / `status.jsonl`;
  `status` and `last` actions; lead adapters for Claude Code, OpenCode, and
  generic agents.
- Renamed from `peteretelej.broadcast`; the existing-pane selection mode is
  dropped (the engine behind it is reused unchanged).
