# atr design

Day-to-day use: [README](../README.md). Providers, caching, MCP, CI:
[HOW-IT-WORKS.md](HOW-IT-WORKS.md). Config keys: [CONFIG.md](CONFIG.md).
GitHub auth: [GITHUB.md](GITHUB.md).

atr is a local daemon that drives official coding-agent CLIs on existing
subscription plans. It does not call vendor HTTP APIs itself and it does not
bill per run. Providers today: Claude Code, Grok Build, Cursor CLI, Codex, and
OpenCode (including named presets). Configuration is TOML. Nothing is hardcoded
except built-in OpenCode presets.

## Operator UX

`atr init` is the whole setup: write config / prompt / launcher, detect CLIs,
install `dev.atr.serve` (macOS LaunchAgent) or the systemd user unit, run
doctor. Example config is neutral. Unknown TOML keys are ignored (doctor warns
with the nearest valid name) so rollbacks work. `[[provider]]` can be name+kind; kind fills CLI flags.
`atr providers` edits TOML with comments kept and SIGHUPs serve. `atr doctor`
prints one ok/warn/FAIL line per check. `atr upgrade` / `rollback` / `service`
/ `logs` cover ops (`upgrade`/`rollback` also refresh the atr skill). Existing configs keep their values; init never auto-rewrites
them.

**Auth:** Claude / Codex / Cursor / Grok keep their own CLI logins. OpenCode
presets (Kimi, Qwen, GLM, DeepSeek, MiniMax, MiMo, Ollama, spark) store the
API key in `shadow.env` only and write base URL + `{env:VAR}` into
`opencode.json`. `[[preset]]` is name → kind + model (no keys).

## Paths and names

- Binary and service: `atr`. LaunchAgent label: `dev.atr.serve`.
- Config default: `~/.config/atr/config.toml` on every OS. Override with
  `--config` or `ATR_CONFIG`.
- State DB defaults to `~/.local/share/atr/state.db` (an existing
  `~/.config/atr/state.db` is reused if the default is missing).
- Worktrees under a configured root, on branch `{worktree_branch_prefix}/{key}`
  (default prefix `atr`).
- GitHub watch label defaults to `for:atr`. Task keys are configurable
  (`task_key_prefix`); examples in docs use `demo-1`.

## 1. Intake

A task is a prompt plus an optional workspace (`git` / `dir` / `none`). Issue
numbers are optional. GitHub-label intake is one source; it still keys tasks
as `{prefix}-{issue}` and still posts via the agent prompt (`gh issue comment`
/ `gh pr create`). atr does not add its own GitHub comments on `output = reply`.

- **GitHub:** poll a configured repo for issues labelled `for:atr` (`gh` API,
  interval from TOML). One task = one issue. Later comments queue as the next
  turn if a run is already live. Workspace `git`, output `reply`.
- **HTTP:** `POST /tasks` creates a generic task; `POST /tasks/{key}/messages`
  is a follow-up. Exit-code contract: 0 accepted, 75 = task busy → message
  **queued** (not resent, not superseding), 1 = error. `GET /tasks/{key}` and
  `GET /tasks/{key}/result` read status and the stored result.
  Optional header `Idempotency-Key` (JSON field fallback) persists the create
  so a retry after a successful enqueue returns the same task; the same key
  with a different request is 409. Optional `files` are stored under the state
  directory and copied into the worktree by filename once the tree exists.
- **CLI:** `atr add` (prompt + `--dir` / `--repo` / neither) and `atr submit`.
- **TUI / ask:** a bare `atr` (or `atr tui`) is an interactive in-process
  chat TUI (ratatui transcript + composer + under-input status strip). `atr ask "prompt"` is
  the one-shot / CI entrypoint on the same engine (no alternate screen).
  The TUI is the provider CLI; atr proxies the turn and keeps the prompt
  cache / session. A session is a normal task keyed `cli-<name>` (default
  `cli-default` — the in-process TUI session, not a model; chrome omits it),
  separate from Telegram `tg<chat>`. Serve is not required and never
  auto-dispatches `cli-*` keys — serve owns daemon work; the TUI / `ask`
  process owns those sessions. Coexistence: `owners` (pid, start token,
  kind `serve`/`tui`) plus `runs.owner_*` / `tasks.owner_*`. The start
  token is whitespace-free (Linux `/proc` starttime, macOS `ps` lstart as
  epoch seconds) so the pidfile `{pid} {started}` keeps the whole token.
  Serve must
  not reap or reconcile a run whose owner is still alive. `max_parallel`
  is enforced in `try_start_run` (`BEGIN IMMEDIATE`) across live owners;
  TUI turns share the cap. Provider: session pin → `--provider` →
  optional `[tui] providers` → global `[[provider]]` order.   `/provider`
  and `/model` are atr-local (list/pick). `/repo` is the mid-session git
  workspace picker (config `[[repo]]` roots; `/repo .`; `/repo none`).
  `/model` sets a per-session,
  per-provider `--model` override from that provider's session / config /
  detect / last-run / a cached CLI `models` catalog
  (Grok / OpenCode / Cursor `id - Label`; Claude documented full ids
  only, no bare aliases; Codex reads
  `~/.codex/models_cache.json` slugs — never spawn its TUI). Host
  `/provider` `/model` `/effort` `/context` `/repo` do not append to the transcript.
  `/effort` / `/context` are per-provider session knobs applied on the next
  spawn (grok `--effort`, claude `--effort`/`--autocompact`, cursor
  parameterized `--model`, codex `-c model_reasoning_effort`). Empty
  `/effort` opens the list only when that kind has a real effort flag.
  Bare `/context` always opens the shared size catalog; apply is claude
  `--autocompact` / cursor brackets only — grok/codex get an honesty
  note (no invented argv). Refresh is off the UI thread — never
  forwarded as a headless chat prompt. The TUI overlay does not dump the
  catalog into the transcript. The under-input strip shows the same
  session `provider / model` as `/status` (plus tok/cache and honest
  workspace kind: `answer-only` / `dir …` / `git …`). Transcript user rows are `>` plus a right-aligned local clock;
  assistant text has no `You` / `atr` chip. `/` opens an atr-local command menu; ↑/↓
  in the composer walk input history. PgUp/PgDn page the transcript by the
  viewport (mouse capture stays off). Enter while a turn is busy parks
  composer lines locally (FIFO); host slashes still run. After idle, the
  oldest parked line sends like idle Enter — not to the live CLI stdin.
  Ctrl+C quits atr (same as `/quit`);
  Esc dismisses overlays; with none open and a turn busy, Esc interrupts it
  and resumes the same session with any parked lines (§8). Other `/…` lines may be proxied. `/cli`
  opens the provider's own interactive TUI. A full PTY host of the vendor
  TUI is out of scope: atr still spawns print-mode / `exec` / `run` per
  turn.
- **MCP:** `atr mcp` is stdio (local agents or SSH). Streamable HTTP is
  `POST /mcp` on the existing listener (localhost by default; Tailscale bind
  is optional). HTTP requires a bearer token from `mcp.token_env` (default
  `ATR_MCP_TOKEN`) or `mcp.token_file` (0600). atr never prints the token.
  Tools enqueue (`add_task` with optional title, workspace git/dir/none, output, `idempotency_key`, `files`), `wait_task`, `get_result`
  (text, diff, PR URL), list, status (timeline + last output), steer, cancel,
  list repos/providers, usage, doctor, and read-only `config_get` (secrets
  redacted). Resources: `atr://status`, `atr://dashboard`. Config cannot be
  changed over MCP. Serve must be running for work to dispatch.
- **Telegram** lives inside `atr serve` (long-poll, no webhook): `/status`
  `/tasks` `/task` `/add` `/cancel` `/retry` `/usage` `/shadow` `/pause` `/resume`
  `/providers` `/enable` `/disable` `/config` `/repos`. `allowed_chat_ids` is required.
  Destructive commands confirm via an inline button. `/pause` stops GitHub,
  shadow, and inbox intake. `/add` is answer-only.
- **Inbox:** drop a `.toml` / `.json` file into a `[[source]] kind = "inbox"`
  directory. atr moves it to `accepted/` or `failed/` and writes
  `results/{key}.json`. Files larger than `max_prompt_bytes` (default 256 KiB)
  go to `failed/` with an error.

`[[source]]` entries override the default for that kind only. Empty
`[[source]]` keeps current behaviour: GitHub if `[github].enabled`, MCP/CLI/HTTP
always, Telegram if enabled, inbox off.

Per-task **output**: `reply` (default, deliver to the source), `pr`,
`issue_comment`, `branch`, `patch`, `text`, `webhook`. Git workspaces use a
worktree. Dir workspaces edit in place; atr asks the CLI to stay inside the
folder where the CLI supports it (best-effort; prompt files live in the state
dir, not `.atr/` in the user's folder). None workspaces answer in text only.

A new message for a running task is queued and never cancels or supersedes the
live run, unless the sender asks to interrupt (see §8).

## 2. State (SQLite, WAL)

`tasks(key, repo, issue, status[queued|running|waiting|done|failed|held], provider, session_id, worktree, created, updated, workspace_kind, output_kind, output_spec, source_kind, result_text, result_diff, result_pr_url, deps, retry_attempt, retry_next_at, retry_reason, hold_reason, hold_notified)`
Opening an older DB `ALTER`s these columns in and backfills: empty `repo` →
`workspace_kind=none`, else `git`; `output_kind=reply`; `source_kind=github`
when `issue` is set, else `cli`. New retry/hold columns default to 0 / empty.
Existing rows keep their keys and status. `held` means an `after` dependency
failed or vanished; it is not started until a human `atr retry`s or sends a
new message. Transient provider retries keep `status=queued` and persist
`retry_next_at` so a serve restart still waits.
`task_sessions(task_key, provider, session_id, updated)` — resume id for this provider only.
`shadow_results(...)` — local-only comparison rows (diff, diffstat of every changed/untracked file, tests, verify, tokens, cost, cache hit). Tasks have a `shadow` flag.
`verify_results(run_id, command, passed, exit_code, output)` — post-success check commands from `[verify]`.
`provider_limits` / `provider_state.quota_reset_at` — 429/limit events and CLI-reported reset.
`runs(id, task_key, provider, model, pid, started, ended, outcome, last_event_at, exit_code, session_id, handoff, owner_pid, owner_started)`
`owners(pid, started, kind, heartbeat)` — live `serve` / `tui` processes. Opening an older DB `ALTER`s owner and retry/hold columns in additively (`CREATE TABLE IF NOT EXISTS` / `ADD COLUMN` with defaults); either feature's DDL can run first on a v0.1.31 `state.db`.
`messages(id, task_key, body, source, received, consumed_by_run)` — FIFO queue per task.
`events(id, run_id, recorded, kind, raw_json)` — assistant / tool / result lines used to build a handoff.
`usage(run_id, input, cache_read, cache_write, output, cost_usd, cost_kind[est|plan], raw_json)` — one row per run (upsert). Subscription CLIs with no dollar figure use `plan`; a reported estimate is `est`. Opening the DB backfills empty `cost_kind`, recovers a Grok cost from `raw_json` when possible, and canonicalizes Grok usage-window names.

`atr tasks rm` and `atr gc --tasks` delete finished, failed, or cancelled task
rows and their sessions, runs, turns, events, and messages. Running tasks are
refused.

## 3. Workspace

By default (`[cache] worktree_pool = true`) each repo+provider reuses a fixed
slot `{worktree_root}/{repo}/.atr/wt/{provider}-{n}` (`n` is
`0..max_parallel-1`). The CLIs embed cwd and git info in their system prompt,
so a stable path keeps the first-run prefix cacheable. Between different tasks
the slot is reset (`checkout -B`, `reset --hard`, `clean -fd`). Follow-ups on
the same task keep the branch and skip the reset. `atr gc` never deletes pool
slots.

`[cache] worktree_pool = false` falls back to one worktree per task key at
`{worktree_root}/{repo}/{key}`. `.atr/` is in each worktree's
`.git/info/exclude`. Results can be posted as an issue comment + PR when
GitHub intake is on.

## 4. Providers, fallback, parallelism

Provider order lives in config. It is **not hardcoded**. Edits apply on
`SIGHUP`/reload or restart. Running tasks keep the provider they started on.
Shipped example order: claude, grok, cursor, codex. Codex must select the
ChatGPT-plan provider via a profile or `-c` overrides when the user's Codex
config points at a local custom-model gateway.

| Provider | Headless | Resume | Notes |
|---|---|---|---|
| Claude Code | `claude -p --output-format stream-json --verbose --permission-mode <mode>` | `--resume <sid>` | `permission_mode` is TOML (example: `bypassPermissions`). Without it, headless Claude cannot edit files. |
| Grok Build | `grok --prompt-file=… --output-format streaming-json --always-approve --cwd <wt>` | `-r <sid>` | New sessions use `--session-id <uuid>` so a stall can resume. |
| Cursor CLI | `cursor-agent -p --force --output-format stream-json` | `--resume <chatId>` | Needs the desktop session (login keychain). |
| Codex CLI | `codex exec --json -C <wt> -s workspace-write -m <model>` | `codex exec resume <uuid>` | ChatGPT sign-in. Example override: `-c model_provider="openai"`. |
| OpenCode | file prompt + `opencode` | session id | Preset keys stay in `shadow.env`. |

Selection: walk the list. Skip a provider that is disabled, in cooldown, or
full (`max_parallel`). The first match takes the task. Extra parallel tasks
spill to the next provider. Fail over on: limit or 429, auth error, 5xx after
retries, a stall kill, or a result with non-empty `permission_denials` /
unfinished work (`needs_attention`, not success). A task sticks to one
provider and **that provider's** session (`task_sessions`). Switching never
passes `--resume <other-provider-id>`.

A provider switch (or a lost session) gets a config-capped handoff after the
stable prefix and task header: last assistant text from stored events, git
status / diffstat / truncated diff (including untracked via
`git diff --no-index`, no leftover `git add -N`), commands and failing test
output. Stored per run; `atr tasks <id> --verbose` prints it.
`[handoff] max_bytes` / `include_diff` are TOML.

At startup serve logs each CLI's version and records it in
`cli-versions.json`; nothing is pinned or blocked. Over SSH a locked login
keychain makes doctor show that recorded version instead.

## 5. Caching

atr never talks to vendor HTTP APIs. Cache hits come from official CLIs
only. The CLIs do prefix reuse natively (Claude Code sets cache_control;
Grok/Cursor cache server-side; OpenCode/Ollama via a stable prefix +
`keepAlive`). Runner rules:

1. **Resume, send only the delta.** Store `session_id`; follow-ups use
   `--resume` and `render_resume` (new messages + optional handoff). The
   system prompt, `AGENTS.md`, and provider preamble are **not** re-sent
   on resume. Grok's id only appears on the final result, so a new Grok
   session is started with `--session-id <uuid>` and that id is recorded
   immediately.
2. **Byte-stable prefix** (enforced by `atr doctor --cache` and a per-run
   hash), written into each CLI's system-prompt *slot* when one exists
   (`{worktree}/.atr/system.md`, never argv/`ps`):
   - Claude: `--append-system-prompt-file`
   - Grok: `--rules` (override with `system_prompt_flag`)
   - Codex: `-c instructions_file="…"`
   - OpenCode: first `--file` (OpenCode 2.x has no `--system`)
   - Cursor: no slot; prefix stays in the prompt file
   - **Stable, hashed, must be byte-identical for every task on a provider:**
     system/append-prompt file + repo `AGENTS.md` + the provider contract
     block + the task header *template* (field order only).
   - **Variable, after `----- atr variable -----`:** Task key, Worktree,
     Repo, Issue, timeout, optional Shadow mode, optional Provider handoff,
     then Messages. First runs with a slot send this tail only.
   - Hash (SHA-256) is stored on the run. A change between runs of the
     **same session** is a cache miss: warn in the log and `atr shadow report`.
   - Same tool set and CLI flags for every run of a provider.
3. **Provider affinity + cache window.** Retries and follow-ups stay on
   the same provider and session. Runnable tasks whose last run is still
   inside `cache_window` (Claude 5m, others 60m unless overridden) are
   scheduled first. Remaining queued work for the last-used provider is
   batched next.
4. **Worktree pool.** Stable cwd/git in the CLI system prompt. See §3.
5. **Ollama via OpenCode.** Long `keepAlive` (default `24h`) is written
   to `opencode.json`. At most one inflight task per Ollama model.
6. **Honest metrics.** Headline hit% is `cache_read / denom` for runs
   with prompt tokens ≥ `[cache] headline_min_input` (default 40k).
   First-run and resume rates are reported separately. Short runs are
   shown but excluded from the headline. Claude-style denom =
   `input + cache_read + cache_write`. Codex-kind `input` is stored
   uncached; denom = `input + cache_read`.

`atr doctor --cache` renders two tasks/handoffs and asserts the prefix is
byte-identical and before any variable content.

## 5b. Cleanup

`[log] path / max_bytes / keep` rotates atr's own log by size and count (no
ANSI colour in the file). `[gc]` (and `atr gc [--dry-run] [--force]
[--older-than <dur>] [--history-older-than <dur>] [--tasks]`):

- Worktrees of tasks `waiting`/`failed`/`done`/`cancelled` older than
  `worktree_retention`. Skip dirty trees unless `--force`. Never prune a
  shadow worktree younger than `shadow_min_age`.
- Delete `events` older than `event_retention` and finished `runs` older than
  `run_retention`. **Keep `usage` rows** so aggregates survive.
- `--tasks` deletes finished/failed/cancelled task rows (and their history).
  `--status failed|done` and `--repo` narrow the set. Running tasks are
  skipped.
- CLI session files only when `session_files = true`, only under known
  per-kind directories, and only files whose name contains a finished
  session id.

Daily automatic gc inside `atr serve` at `[gc] daily` (local `HH:MM`).

Once `[log] path` is set, the LaunchAgent template must **not** capture
stdout/stderr to unrotated `~/Library/Logs/atr*.log`. The shipped plist runs
`~/.config/atr/atr-serve.sh` (sources `shadow.env`, then `exec atr serve`),
sends stdout to `/dev/null`, and keeps only a boot-error file for crashes
before `init_tracing`.

## 6. Usage logging

- Claude `result` event: `total_cost_usd` (estimate on a subscription),
  `usage.input_tokens`, `cache_read_input_tokens`,
  `cache_creation_input_tokens`, `output_tokens`, `num_turns`, `session_id`.
- Grok result: `usage.{input_tokens (uncached), cache_read_input_tokens,
  cache_creation_input_tokens, output_tokens, reasoning_tokens}`,
  `modelUsage.*.costUSD`, `total_cost_usd`.
- Cursor: `usage.{inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens}`.
- Codex `--json`: `thread.started{thread_id}` (session id), then
  `turn.completed{usage:{input_tokens, cached_input_tokens, output_tokens}}`.
  `input_tokens` includes cached; store uncached = input - cached. No cost
  field on a ChatGPT plan.

Reported usage is optional and per-provider (`claude_stream` / `grok_billing`).
Manual `[[provider.quota]]` still applies. `atr status` shows both.

**Quota-aware choice:** `[[provider.quota]]` rolling windows (usd/tokens/runs);
skip new tasks at threshold or in a reported limit/reset; existing tasks stay.
`display_timezone` labels times and groups usage days.

## 7. Watchdog

`[provider.health]`: no first stdout within `startup_timeout` is Unavailable
(handoff, no resume). After output, no event for `heartbeat_timeout` is a
stall: resume up to `stall_resumes`, then handoff + `unhealthy_cooldown`.
SIGTERM, grace, SIGKILL the process group. Spawn failure and connection
errors are Unavailable. If every provider is cooling, the task stays queued.

A stall marks the provider degraded for a short configurable window; the flag
clears when that window expires and on the next successful run.
`atr cancel --force` does not cool the provider.

Stall, cancel, **and a normal CLI exit** kill leftover descendants.

Wall-clock cap (`run_timeout` per provider); per-task cap from TOML. Orphan
sweep at startup: runs marked running whose pid is gone → failed.

## 8. Message queueing

New message for a running task is appended to `messages` and, by default,
never interrupts. When the run ends, all pending messages are joined into one
`--resume` turn. `--force` (explicit, logged) is the only way to cancel a live run.

**Interrupt and continue** is opt-in: `steer_task` with `interrupt: true`,
`POST /tasks/:key/messages` with `"interrupt": true`, `atr submit --interrupt`,
or Esc in the TUI. atr enqueues the message, marks the live run `interrupted`,
and kills its process group (in-process slot, else the open run's pid from the
DB, so stdio `atr mcp` can interrupt a run owned by `atr serve`). The scheduler
then applies no cooldown and no failover: it keeps the provider session and
queues a resume turn whose first message is a fixed "your previous turn was
interrupted … continue your unfinished task" preamble, followed by the pending
messages. With no pending message the task goes to `waiting`.
Graceful cases: a provider with no resume support (`resume_flag` empty and not
codex/opencode) is never killed (`not_resumable`; the message just waits); a
run that never reported a session id (or a fresh atr-chosen Grok id the CLI
never confirmed) has its messages unconsumed, so the fresh run sees the
original request plus the new message, with the usual handoff.

## 9. Notifications

Telegram bot inside `atr serve` (long-poll, no webhook). Batched, rate-limited
alerts for done / needs_attention / failover / quota skip. Daily summary at a
configured local time. Never send tokens, env vars, or full prompts.
`display_timezone` labels every clock. The agent posts on the GitHub issue
when GitHub intake is on.

## 10. Shadow mode and verify

**Shadow mode** (`[shadow]` + per-task flag): no GitHub writes, no
notifications; optional read-only poller; `atr shadow report` (includes a
short diffstat). Concurrent shadow cap defaults to 1. Agents do not see
`GH_TOKEN` / `GITHUB_TOKEN`; the poller/clone uses `shadow.gh_token_env`
(default `ATR_SHADOW_GH_TOKEN`). Shadow worktrees set `pre-push` +
`pushurl=no_push`. `shadow.repo_path` is cloned or fetched from `shadow.repo`
when missing.

**Verify before done** (`[verify]`): after success, run configured (or
optionally detected) checks in the worktree; one same-session follow-up on
failure (`max_retries`, default 1; a no-shell provider gets
`no_shell_extra_retries` more, default 1); otherwise `needs_attention`.
Triage tasks skip verify (suggest-only).
No-shell providers (`Provider::has_shell() == false`: OpenCode without
`allow_shell`) also get `[scratch]` cleanup (`scope = "no_shell"` by default),
a blocking main-checkout guard (other providers: warning only) and a fresh
session on `atr retry` (other providers resume). Every provider gets `PWD` =
worktree, atr-owned commit/push/PR, honest no-ops, and sanitized
`issue_comment` output capped at GitHub's 65,000-character limit.
`[verify] protect_tests` restores base test files before checks.
Opt-out: `atr:tests-ok` or `--tests-ok`. `[checklist]` keeps
`.atr/checklist.md` in the variable prompt (prefix unchanged).

## 11. Service

LaunchAgent `dev.atr.serve` in the **GUI (Aqua) session** so Cursor can use the
unlocked login keychain. The plist execs `atr-serve.sh`, which sources
`~/.config/atr/shadow.env` so tokens stay out of the unit. Linux: systemd
`--user` unit `atr.service`. `atr service install` writes and loads these
names. Pass `--legacy-label <old-label>` to boot out any previous LaunchAgent
before installing, so two copies of `atr serve` do not run.

## 12. Tests and CI

Tests use fake CLI scripts that emit the real JSON shapes. CI: rustfmt,
clippy, test, cargo-deny, and gitleaks. Prompts go through a file or stdin,
never argv/`ps`.

## 13. Alternatives considered

**pi** (coding agent, MIT). Lean harness with JSON/RPC modes and many
providers. **Rejected as the core.** Third-party harnesses that reuse Claude
Pro/Max OAuth are billed as extra per-token usage, and copying Claude Code's
keychain token is outside Anthropic's terms. Cursor has no login route in pi.

**DeepSeek Harness** (`dsh`, MIT). Plugin-based, JSONL sessions. **Rejected.**
Headless mode is one-shot; Claude needs an API key; there is no Cursor
subscription route.

**Verdict:** only the official CLIs (claude, grok, cursor-agent, codex, plus
OpenCode for extra models) stay inside the plans, so atr drives them
directly. Ideas worth borrowing: a small tool set and a stable system prompt
(cache warmth); per-session JSONL persistence.
