# Changelog

## Unreleased

### Fixed

- Dashboard provider rows no longer show an extra-credits column or billing text (`no billing reply`, `_x.ai/billing`). Spark (including a Codex-kind Spark provider) shows `-` and no `plan_line`. A Codex API-key login shows `codex auth is an API key; usage not supported`. Claude `plan_line` dates use the short form (`Nov 28`). A passed stored five-hour window (`not a current meter`) is left off that line; current meters stay, for example `seven_day 4.0% resets Nov 28`. `atr usage --plan` still reports the stale window as `unknown`.

- `atr usage --plan` Cursor no longer reports `unknown` / "Cursor session has no user id" when `cursorAuth/userId` is missing and `cursorAuth/accessToken` in `state.vscdb` is not a JWT. The WorkOS user id is read from `stripeMembershipAuthId`, `cachedScopedProfile.authId`, `glass.lastSignedInAuthId`, or `~/.cursor/cli-config.json` `authInfo.authId`. On macOS an encrypted desktop token is read from the keychain item `cursor-access-token` (account `cursor-user`). If that item cannot be read, the row stays `unknown` and names that item.
- A stored Claude `claude_stream` window whose `resets_at` is already past is not shown as the current meter. It is `unknown` with source `stored claude_stream (stale)`. A window that has not reset, or has no `resets_at`, is unchanged. The old utilization is not rolled forward.

- `--output branch` commits uncommitted worktree edits before
  `git push` (same message and author rules as `--output pr`). A clean
  tree does not get an empty commit. When HEAD already matches its
  upstream, the push is skipped. The branch prompt tells the agent that
  atr commits and pushes, and not to run `git commit` or `git push`.

- `atr doctor` no longer caps `tincan ping` at 5s. The ping waits at least
  20s, or `notify.tincan.timeout` when that is longer (real pongs take
  about 10–12s, so the 5s cap warned on a healthy relay). A ping that
  exceeds the budget is warn `slow`; a non-zero exit or a missing spawn
  is warn `failed`.

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

- Read-only vendor plan meters: MCP `plan_usage`, `atr usage --plan` (`--json`), and a dashboard `plan_line`. One adapter per supported provider (claude, grok, cursor, codex, gemini, opencode). Local spark and Ollama, and any provider without a proven meter, return `status: unknown` and a reason. HTTP is GET-only against a tested allow-list. Session tokens are not logged, returned, or stored. One failure does not drop the other providers. Cached in memory for about five minutes. `ATR_PLAN_USAGE=off` skips the reads.
- `[notify.tincan]` pushes task-lifecycle events to an Agent Tincan teammate
  (`tincan ask <agent> --notify --json`). Same `Event` names as Telegram;
  empty `events` is a default subset (finished, failed, needs_attention,
  stalled, cap_reached, review, held, cancelled). Owner is `default_agent`
  or `[labels.<name>] notify_agent`. Fire-and-forget (10s timeout covering
  message build + CLI); `atr doctor` checks the binary and `tincan ping
  <agent>` when enabled. One event is one ping; `status=` is the
  post-transition row; `finished` waits until output delivery so `pr=` is set.
  A timeout kills the process group and reaps it. `default_agent` /
  `notify_agent` must be a single name. `atr doctor` pings run off the
  async worker.

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
