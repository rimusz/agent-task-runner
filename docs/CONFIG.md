# Configuration

All policy lives in `~/.config/atr/config.toml`. Override the path with
`--config` or `ATR_CONFIG`. Unknown keys are ignored with an info log line,
and `atr doctor` warns about them (`config keys`, with the nearest valid name),
so a config written for a newer atr still loads after a rollback. Keys that look
like secrets (`api_key`, `token`, `password`, …) are still refused, and
`atr config set` refuses to add a key it does not know. `atr serve` refuses to
start if a required setting is empty. `repo_path` (or `[[repo]]`) is required
only when GitHub intake is on.

Shipped example: [`config/example.toml`](../config/example.toml).
`atr init` writes this file (or merges missing keys). Reload a running daemon
with SIGHUP (`atr providers` edits do that) or `atr service restart`.

## Where files live

| What | Path |
|---|---|
| Config | `~/.config/atr/config.toml` (+ `system-prompt.md`, `atr-serve.sh`) |
| Secrets | `~/.config/atr/shadow.env` (mode 0600). Never in the plist or config |
| State DB | `~/.local/share/atr/state.db` (an existing `~/.config/atr/state.db` is reused) |
| Logs | `[log] path` (macOS init uses `~/Library/Logs/atr/atr.log`) |
| Worktrees | `{worktree_root}/{repo}/.atr/wt/<provider>-<n>` (pool; or `{repo}/{task-key}` if `worktree_pool = false`) |
| LaunchAgent | `~/Library/LaunchAgents/dev.atr.serve.plist` |
| systemd | `~/.config/systemd/user/atr.service` |

```bash
atr --config ~/.config/atr/config.toml status
```

`atr config get github.label` and `atr config set github.label for:atr` (or
Telegram `/config get` / `/config set`, which Confirm first) edit the same file
with the same validation: secret keys are refused, the new file must parse,
comments are kept, `config.toml.bak` is written and the running service is
reloaded with SIGHUP. `atr config migrate` converts shorthand
`repo_path` + `[github].repo` into `[[repo]]` and leaves the old keys in place.

## Useful keys

`driver`, `[planner]`, `[review]`, `[plugin]`, `[github] enabled / repo / label`
(default `for:atr`), `[dashboard] enabled / bind`, `[telegram]` and
`[telegram.assistant]`, `[[provider]]` order and `enabled`, `[mcp]`, `[cache]`,
`[[source]]`, `max_prompt_bytes` (default `256k`; CLI/HTTP/MCP prompts and
inbox files).

### Cache

```toml
[cache]
worktree_pool = true
headline_min_input = 40000
```

Per provider: `cache_window` (Claude default `5m`, others `60m`),
`system_prompt_flag`, `keep_alive` (Ollama default `24h`). See
[HOW-IT-WORKS.md](HOW-IT-WORKS.md#token-caching).

### MCP

```toml
[mcp]
enabled = true
token_env = "ATR_MCP_TOKEN"
token_file = "~/.config/atr/mcp.token"
```

Token setup, client configs and rotation: [REMOTE-ACCESS.md](REMOTE-ACCESS.md#mcp-over-http).

### Dashboard

`atr ui` opens the dashboard on this machine: it takes the port from `listen`, uses `127.0.0.1` when `listen` is a wildcard (`0.0.0.0`, `::`), checks `/health`, then runs `open` (macOS) or `xdg-open` (Linux desktop). Over SSH or without a display it only prints the URL; `--print` always only prints.

```toml
[dashboard]
enabled = true
bind = "127.0.0.1"
```

Default page: `http://127.0.0.1:7420/`. On Tailscale:

```toml
listen = "<tailscale-ip>:7420"

[dashboard]
enabled = true
bind = "<tailscale-ip>"
```

`public_url` (optional) is the link Telegram `/ui` sends, e.g.
`public_url = "https://ai-stack.<tailnet>.ts.net/"`. Unset: atr tries
`tailscale serve status --json`, then falls back to the local URL.

If `bind` is not the listen host, serve also tries an extra read-only listener
on `{bind}:{port}` so submit/cancel stay on loopback.

The recommended way to reach the dashboard and `/mcp` from other devices is Tailscale Serve with `listen` left on loopback. See [REMOTE-ACCESS.md](REMOTE-ACCESS.md). The dashboard has no auth of its own.

### Repos

**One repo** — top-level shorthand (what `atr init` writes):

```toml
repo_path = "~/src/my-repo"
task_key_prefix = "task"
[github]
enabled = true
repo = "owner/my-repo"
label = "for:atr"
```

**Several repos** — repeatable `[[repo]]` blocks. Task keys are `{name}-{issue}`.

```toml
[github]
enabled = true

[[repo]]
name = "demo-1"
path = "~/src/demo-1"
github = "owner/demo-1"
label = "for:atr"

[[repo]]
name = "website"
path = "~/src/website"
github = "owner/website"
label = "for:atr"
```

Optional per-repo: `branch_prefix`, `base`, `system_prompt_file`, `max_parallel`.

```bash
atr repos list
atr repos add owner/website --path ~/src/website
atr repos enable website
atr config migrate
```

### CLI versions

atr does not pin CLI versions. `atr doctor` shows `ok cli <name> <path>
<version>`. An old `[[provider]] expected_version` key is still accepted but
ignored (doctor prints a one-line note); delete it.

### Notify hook

```toml
[notify]
command = "~/bin/atr-alert.sh"  # sh -c; empty = off
timeout = "10s"
```

Runs for operator alerts (GitHub auth failed / recovered / token expiring,
Telegram unreachable 15+ min / recovered)
with `ATR_ALERT_KIND`, `ATR_ALERT_MESSAGE`, `ATR_ALERT_SOURCE`,
`ATR_ALERT_DAYS_LEFT`, `ATR_ALERT_TOKEN_ENV`, `ATR_ALERT_OWNER`
(`telegram_unreachable` / `telegram_recovered` for Telegram health). Telegram
still fires too for GitHub alerts. See
[TROUBLESHOOTING.md](TROUBLESHOOTING.md#7-notify-hook).

### GitHub token

Put `ATR_GH_TOKEN=<fine-grained PAT>` in `~/.config/atr/shadow.env` (0600),
never in config. See [GITHUB.md](GITHUB.md#one-token-for-atr).

Repos from several owners: fine-grained PATs are single-owner, so map each
owner to its own env var (or use `atr github token set <owner>`):

```toml
[github.tokens]
your-user = "ATR_GH_TOKEN_YOUR_USER"
YourOrg = "ATR_GH_TOKEN_YOURORG"
```

See [GITHUB.md](GITHUB.md#per-owner-tokens).

## Sources

Omit `[[source]]` to keep current defaults: GitHub follows `[github].enabled`,
MCP/CLI/HTTP are on, Telegram follows `[telegram].enabled`, inbox is off.
Listing a kind overrides **only** that kind.

```toml
[[source]]
kind = "inbox"
enabled = true
dir = "~/.local/share/atr/inbox"
poll_interval = "5s"
# max_bytes = "256k"   # inbox file cap; default max_prompt_bytes

[[source]]
kind = "github"
enabled = false
```

### Inbox folder

Off unless a `[[source]] kind = "inbox"` entry with `enabled = true` (the
default for a listed entry) and a `dir` exists. `dir` is required, `~` is
expanded, and the folder plus `accepted/`, `failed/`, `results/` are created on
first poll. `poll_interval` defaults to `5s` (minimum 1s). Inbox files larger
than `max_bytes` (default `max_prompt_bytes`, 256 KiB) are moved to `failed/`
with an `.error.txt` reason. CLI, HTTP, and MCP prompts use the same cap.
Reload with SIGHUP or restart the service.

atr reads files ending `.toml` or `.json` (names starting with `.` are skipped,
so write `.x.toml` then rename). Fields (TOML shown; JSON uses the same names):

```toml
# ~/.local/share/atr/inbox/fix-typo.toml
title = "Fix typo in notes"      # optional; default is the first prompt line
prompt = "Fix the spelling mistakes in notes.txt. Change nothing else."
                                 # required; `body` (alias `text`, `message`) also works
dir = "~/notes"                  # local folder workspace (edit in place)
# repo = "my-repo"               # git workspace: watch repo name, owner/name, or path
# workspace = "none"             # git | dir | none; default none when no dir/repo
output = "reply"                 # reply (default) | text | webhook | issue_comment (GitHub tasks)
                                 # pr | branch | patch need a git workspace
# webhook = "https://example.com/hook"   # for output = "webhook"
# patch = "/tmp/fix.patch"       # for output = "patch"
# key = "notes-typo"             # optional task key; auto-allocated otherwise
# provider = "claude"            # optional provider/driver hint
# shadow = false
```

After pickup the file moves to `accepted/` if it was queued, or to `failed/`
with the reason in `<name>.error.txt` if it could not be parsed or queued
(missing prompt, bad dir/repo, git-only output without a repo). With
`output = "reply"` the finished result is written to `results/{key}.json`.

`atr add "…"` (optional `--dir` / `--repo` / `--output`) and `POST /tasks`
enqueue a generic task. `GET /tasks/{key}` and `GET /tasks/{key}/result` read
it back. Output kinds: `reply` (default, answer the source), `pr`,
`issue_comment`, `branch`, `patch`, `text`, `webhook`.

## Submit and the local API

`atr submit` talks to the local HTTP API (`127.0.0.1:7420`). If serve is down
it writes SQLite directly. Exit `0` = accepted, `75` = the task was busy so
the message was queued, `1` = error. `atr add` uses `POST /tasks` the same way.

## Service and edits

`atr service install` writes **`dev.atr.serve`** (macOS LaunchAgent) or
`atr.service` (systemd `--user`). `atr init` will not replace a loaded
`dev.atr.serve` job, and will not install a service when `ATR_CONFIG` or the
config path is not the default, unless you pass `--force-service`. A
non-default config gets a distinct LaunchAgent label derived from the config
path. `atr providers` and Telegram `/config set`
keep comments, back up `config.toml.bak`, and SIGHUP the daemon. If an older
LaunchAgent is still loaded, pass its label with
`atr service install --legacy-label <old-label>` so install can stop that job
before writing the current one. Two copies of `atr serve` must not run.

On Linux, atr runs as a systemd user service, which stops when you log out.
To keep it running after logout or reboot without logging in, run
`sudo loginctl enable-linger $USER` once (needs sudo). Check it with
`loginctl show-user $USER -p Linger`.

`atr tasks rm` and `atr gc --tasks` delete finished, failed, or cancelled
task rows only. Running tasks are refused.

## Choosing the driver provider and model

atr picks providers in three separate places:

| What | Config | Default when empty |
|------|--------|--------------------|
| **Driver** (planning + review) | top-level `driver = "<provider name>"` | `grok` if that provider is enabled, else the first enabled `[[provider]]` |
| **Workers** (normal tasks) | `[[provider]]` order + `enabled` | first available provider in order |
| **Telegram assistant** (free-text chat) | `[telegram.assistant] provider` | first available provider in order (it does **not** use `driver`) |

The model is always per provider: `[[provider]] model` (empty = the CLI's own
default). There is no per-task model override; to run a task on another model,
add a second `[[provider]]` with that model and send the task to it.

```toml
driver = "claude"            # or "grok", "cursor", "codex", or any [[provider]] name

[[provider]]                 # order = worker preference
name = "claude"
kind = "claude"
model = "opus"

[[provider]]
name = "grok"
kind = "grok"

[telegram.assistant]
enabled = true
provider = "grok"            # optional; empty = first available in order
```

Set or change it:

- CLI: `atr driver` (show), `atr driver set claude --model opus`,
  `atr driver set grok`, `atr driver clear` (back to the default). Order and
  models: `atr providers move <name> --to 1`, `atr providers model <name> <model>`.
- Telegram: `/config set driver codex`, `/providers model cursor <model>`,
  `/providers move <name> <n>`, `/config set telegram.assistant.provider claude`.
- Per task (provider only): `atr add --provider claude "…"`, `"provider"` in
  `POST /tasks` JSON, or `provider` on the MCP `add_task` tool. It's a
  preference; the provider still needs the task's capabilities.

Examples: `atr driver set grok` (recommended; enables Grok extras),
`atr driver set claude --model opus`, `atr driver set cursor`,
`atr driver set codex --model gpt-5-codex`.

Fallback: a preferred provider (driver, or a per-task `provider`) that is
disabled or has no CLI is skipped and the next provider in order is used; if
it's busy (`max_parallel`) the task waits. Quota or rate-limit failures during
a run fail over to the next provider like any worker.

Verify: `atr driver`, `atr providers` (order + models), the dashboard meta
line (`driver <name>`), `atr status --json | jq .driver`, and `atr doctor`
(each `cli` line shows path and version).

## Other commands

`atr gc --dry-run`, `atr shadow report --since 7d`, `atr tasks rm`,
`atr usage --period month --by provider`.

## Provider capabilities

`capabilities = ["browser", "desktop"]` on a `[[provider]]` marks which
providers may take tasks with matching `needs`. Top-level `desktop_check`
overrides graphical-session detection. See [NEEDS.md](NEEDS.md).
