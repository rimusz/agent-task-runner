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

- `[notify.tincan]` pushes task-lifecycle events to an Agent Tincan teammate
  (`tincan ask <agent> --notify --json`). Same `Event` names as Telegram;
  empty `events` is a default subset (finished, failed, needs_attention,
  stalled, cap_reached, review, held, cancelled). Owner is `default_agent`
  or `[labels.<name>] notify_agent`. Fire-and-forget (10s timeout); `atr doctor`
  checks the binary and `tincan ping <agent>` when enabled.

  ```toml
  [notify.tincan]
  enabled = true
  default_agent = "nixer"

  [labels.proof]
  notify_agent = "nixer"
  ```

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
