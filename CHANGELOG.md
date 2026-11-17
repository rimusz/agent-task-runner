# Changelog

## Unreleased

### Fixed

- Hitting `[[provider]] run_timeout` (the per-run wall-clock cap, default
  `90m`) is no longer reported as a stall / CLI hang / heartbeat, and no
  longer cools the provider as a usage `limit`. Telegram now sends
  `cap reached <task> provider=<p> after <dur>, resuming same session (n/max)`
  or, when same-session resumes are exhausted,
  `cap reached <task>, same-session retries exhausted (n/max), <next action>`.
  Real heartbeat hangs still use `stalled … (CLI hang / heartbeat)`.
- Cap exhaustion on a task that pinned a provider (`--provider`,
  `provider:` label, `--model`, or a compare-arm strict pin) no longer
  overflows to the next provider. The task is `needs_attention` unless
  `overflow_on_cap` is true. Unpinned tasks still overflow. Real
  usage-limit overflow is unchanged.

### Added

- Per-task and per-label overrides for the run cap and same-session resume
  budget. Precedence: task (`atr add --run-timeout` / `--stall-resumes` /
  `--overflow-on-cap` / `--labels`, HTTP/MCP fields) > `[labels.<name>]` >
  `[[provider]] run_timeout` / `stall_resumes`. Global defaults are
  unchanged.

  ```toml
  [labels.proof]
  run_timeout = "3h"
  stall_resumes = 4
  ```
