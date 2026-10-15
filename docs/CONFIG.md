# Configuration

All policy lives in `~/.config/atr/config.toml`. Override the path with
`--config` or `ATR_CONFIG`. Unknown keys are rejected. `atr serve` refuses to
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

Telegram `/config get github.label` and `/config set github.label for:atr` edit
the same file (Confirm, then reload). `atr config migrate` converts shorthand
`repo_path` + `[github].repo` into `[[repo]]` and leaves the old keys in place.

## Useful keys

`driver`, `[planner]`, `[review]`, `[plugin]`, `[github] enabled / repo / label`
(default `for:atr`), `[dashboard] enabled / bind`, `[telegram]` and
`[telegram.assistant]`, `[[provider]]` order and `enabled`, `[mcp]`, `[cache]`,
`[[source]]`.

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

### Dashboard

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

If `bind` is not the listen host, serve also tries an extra read-only listener
on `{bind}:{port}` so submit/cancel stay on loopback.

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

[[source]]
kind = "github"
enabled = false
```

### Inbox folder

Off unless a `[[source]] kind = "inbox"` entry with `enabled = true` (the
default for a listed entry) and a `dir` exists. `dir` is required, `~` is
expanded, and the folder plus `accepted/`, `failed/`, `results/` are created on
first poll. `poll_interval` defaults to `5s` (minimum 1s). Reload with SIGHUP
or restart the service.

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
`atr.service` (systemd `--user`). `atr providers` and Telegram `/config set`
keep comments, back up `config.toml.bak`, and SIGHUP the daemon. If an older
LaunchAgent is still loaded, pass its label with
`atr service install --legacy-label <old-label>` so install can stop that job
before writing the current one. Two copies of `atr serve` must not run.

`atr tasks rm` and `atr gc --tasks` delete finished, failed, or cancelled
task rows only. Running tasks are refused.

## Other commands

`atr gc --dry-run`, `atr shadow report --since 7d`, `atr tasks rm`,
`atr usage --period month --by provider`.
