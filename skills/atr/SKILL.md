---
name: atr
description: Set up and operate atr (agent task runner) — init, providers, OpenCode/Chinese CLIs, Ollama/spark, Telegram, doctor, upgrade, troubleshooting.
---

# atr

`atr` is a long-running service that drives official agent CLIs on existing subscription plans — never per-use API billing.

Kinds: `claude`, `grok`, `cursor`, `codex`, `opencode`. A `[[provider]]` block can be just `name` + `kind`. Nothing else is hardcoded — policy lives in `~/.config/atr/config.toml`.

OpenCode covers Ollama and the Chinese providers (Kimi, Qwen, GLM, MiniMax, DeepSeek, MiMo) plus local spark. **Any model OpenCode supports works with atr by its `provider/model` id.** Presets are shortcuts only.

Secrets live in `~/.config/atr/shadow.env` (mode 0600), never in config.

## Driver and models

- `driver` (top-level) = planning/review provider; empty = grok if enabled, else first enabled. Show/set: `atr driver`, `atr driver set <provider> [--model M]`, `atr driver clear`; Telegram `/config set driver <name>`.
- Workers use `[[provider]]` order; the Telegram assistant uses `telegram.assistant.providers` (ordered list: first enabled one not busy/cooling/rate-limited/at quota, falls back down the list), else `telegram.assistant.provider`, else first available (not `driver`). Set the list with `atr config set telegram.assistant.providers '["grok","codex","claude"]'` (`/config set …` in Telegram); `providers` wins over `provider` and doctor warns when both are set. The TUI (bare `atr` / `atr tui`) and `atr ask` use session pin → `--provider` → optional `[tui] providers` → global order. Models are per provider (`atr providers model <name> <model>`); TUI `/model` is a per-session, per-provider override. Per-task provider preference: `atr add --provider`, POST/MCP `provider`.
- Verify: `atr driver`, dashboard `driver <name>`, `atr status --json | jq .driver`. Details: CONFIG.md "Choosing the driver provider and model".

## Terminal TUI

A bare `atr` (or `atr tui`) is an interactive in-process chat TUI (transcript + composer + under-input status strip). It is the provider CLI with atr as the proxy (session reuse + prompt-cache): plain text is a provider turn; atr does not invent chat replies. Mouse is left to the terminal for copy/paste. `atr ask "prompt"` is the one-shot / CI entrypoint on the same engine (no alternate screen). Serve is not required and never auto-dispatches `cli-*` sessions. Sessions are tasks keyed `cli-<name>` (default `cli-default` — internal session id, not a model; title omits it), separate from Telegram. Slash: `/provider`, `/model`, and `/repo` are atr-local pickers (overlay only — not transcript rows). `/model` lists real CLI ids, never `Available` / help prose; Claude seeds documented full ids only (`claude-…`, no `opus`/`sonnet` aliases or `[1m]` crumbs); Grok strip uses a real catalog id; Codex reads `~/.codex/models_cache.json` slugs (chat ids only, not `codex-auto-review`); sets that provider's session `--model` override; never sent as chat. `/repo` picks a config git root (`[[repo]]`; serve optional), `/repo .` the cwd git root, `/repo none` answer-only; bare `atr --repo .` / `atr tui --repo` / `--dir` set the launch workspace (config name or path is enough — full path not required). `/new` `/quit` `/status` `/help` `/cli` stay atr-local; `/cli` opens the vendor TUI. Other `/…` may be proxied. User turns are prefixed `>` with a local clock; replies have no `You` / `atr` chip. The strip and `/status` show the real workspace kind (`answer-only` / `dir …` / `git …`), never process cwd for answer-only. Tracing never paints on the TUI composer (see `[log] path`). First run: bare `atr` → init offer → service ask → TUI. Scripts: `atr init --yes` then `atr ask`. No TTY never auto-inits.

```bash
atr ask "what is 2+2?"       # one-shot / CI
atr                          # interactive TUI; Ctrl+C or /quit to leave
atr --repo .                 # cwd git root; config name or path also fine
atr tui --repo demo --session work --provider grok
```

Mid-session `/provider` sticks and triggers the existing handoff on switch. `/model` / `/model <id>` change atr's session model (under-input strip + next `--model`); they are never forwarded as a Claude/Cursor prompt. `/repo` / `/repo .` / `/repo none` change the session workspace (clears a bound `cli-*` worktree/session like `/new`). Ctrl+C quits atr (Esc only dismisses menus). `atr serve` stays an explicit subcommand.

## Telegram assistant (this chat)

You are answering in Telegram about atr. You already have a read-only snapshot of `atr status`, `atr doctor`, and config (secrets redacted).

- **Never** use the shell, edit files, run `git`/`gh`, or write config yourself.
- You may only **propose** the same commands the Telegram/CLI already support.
- To propose a change, end the reply with one line per command:

  `ATR: /providers add kimi`

  `ATR: /config set telegram.assistant.enabled true`

  `ATR: /retry acme-12`

- atr will show an inline **Confirm** (old → new) before anything is applied.
- Read-only questions (“how do I add kimi?”, “why did task X fail?”) get a short answer from this skill + the snapshot. Do not invent keys or tokens.

Allowed command families: `/status` `/tasks` `/task` `/add` `/usage` `/shadow` `/providers` `/config` `/repos` `/cancel` `/retry` `/pause` `/resume` `/enable` `/disable` `/ui` `/help`.

## Telegram command to CLI

Operators with a shell use the CLI; it shares the same code paths. `atr pause`/`resume`/`retry` go through the running service (fallback: state db, applied at next start). `atr config set` validates, keeps comments, backs up and SIGHUPs.

| Telegram | CLI | HTTP (bearer token) |
|---|---|---|
| `/status [repo]` | `atr status [--repo R]` | `GET /status` |
| `/tasks [repo]` | `atr tasks [--repo R]` | `GET /dashboard.json` |
| `/task <key>` | `atr tasks <key>` | `GET /tasks/<key>` |
| `/add <prompt>` | `atr add <prompt>` | `POST /tasks` |
| `/cancel <key> [force]` | `atr cancel <key> [--force]` | `POST /tasks/<key>/cancel` |
| `/retry <key>` | `atr retry <key>` (fresh attempt: last consumed prompt is unconsumed so the scheduler can dispatch, wall clock restarts, provider re-picked from current config unless strictly pinned) | `POST /tasks/<key>/retry` |
| `/pause` | `atr pause` | `POST /intake/pause` |
| `/resume` | `atr resume` | `POST /intake/resume` |
| `/config get <key>` | `atr config get <key>` | none |
| `/config set <key> <value>` | `atr config set <key> <value>` | none |
| `/providers …`, `/enable`, `/disable` | `atr providers [add\|enable\|disable\|move\|model]` | none |
| `/repos` | `atr repos list` | none |
| `/usage [period] [repo]` | `atr usage [--period P] [--repo R]` | none |
| `/shadow` | `atr shadow report` | none |
| `/doctor` | `atr doctor` | none |
| `/tokens` | `atr github token list` | none |
| `/ui` | `atr ui --print` | `GET /` |

## Init

```bash
atr init          # or: atr init --yes
atr telegram setup
atr doctor
```

`atr init` writes `~/.config/atr` (config, `system-prompt.md`, `atr-serve.sh`), detects CLIs, installs this skill into each CLI’s skill dir, and runs doctor. On a TTY it asks whether to install the user service (LaunchAgent `dev.atr.serve` on macOS, systemd user unit `atr.service` on Linux) so Telegram / GitHub / queued `atr add` keep running; `--yes` installs it, `--no-service` skips. Saying no is not a dead end: `atr tui` / `atr ask` still work; install later with `atr service install` or `atr serve`. It never overwrites existing files without `--force` (it merges missing keys). It will not replace a loaded `dev.atr.serve` job or install a service for a non-default config unless you pass `--force-service`. On macOS the service must run in the desktop session so Cursor can use the login keychain. Headless Linux: `sudo loginctl enable-linger "$USER"` so the user unit survives logout.

Override config with `--config` or `ATR_CONFIG`. SQLite defaults to `~/.local/share/atr/state.db`.

## Providers

```bash
atr providers                          # numbered order
atr providers detect                   # CLIs, versions, signed-in, OpenCode models
atr providers add kimi                 # preset; key → shadow.env
atr providers add my-oc --kind opencode --model moonshotai/kimi-k2.6
atr providers add spark                # local OpenCode URL
atr providers add spark --kind codex --override 'model_provider="openai"'
atr providers enable cursor
atr providers disable grok
atr providers move claude --to 1
atr providers model claude opus
```

Edits preserve comments, back up `config.toml.bak`, and SIGHUP the daemon.

Telegram: `/providers add kimi <key>`, `/providers enable|disable|move|model` — each write needs **Confirm**.

## OpenCode and other CLIs

Install OpenCode, sign in, list models, add by `provider/model`, then `atr doctor` + a tiny task.

```bash
curl -fsSL https://opencode.ai/install | bash   # or npm / brew
opencode auth login
opencode models
atr providers add my-oc --kind opencode --model <provider/model>
atr doctor
atr submit smoke --body "reply with the word ok and do not edit files" --repo ~/src/my-repo
```

`atr serve` must be running. Preset keys: `shadow.env`. OpenCode login: `~/.local/share/opencode/auth.json`.

Vendor coding CLIs (Kimi Code, Qwen Code, ZCode, MiMo Code, …) are interactive only. atr has no kinds for them — use OpenCode.

### Claude, Grok, Cursor, Codex (own CLIs)

| CLI | Add |
|---|---|
| Claude Code | `atr providers add claude --kind claude` |
| Grok Build | `atr providers add grok --kind grok` |
| Cursor CLI | `atr providers add cursor --kind cursor` |
| Codex | `atr providers add codex --kind codex` |

### Chinese providers (via OpenCode)

| Name | Sign-in / key | Example model | Shortcut |
|---|---|---|---|
| Kimi (Moonshot) | `opencode auth login` → Moonshot, or `MOONSHOT_API_KEY` | `moonshotai/kimi-k2.6` | `atr providers add kimi` |
| Qwen (DashScope) | `DASHSCOPE_API_KEY` or custom OpenAI-compatible | `qwen/qwen3.5-plus` | `atr providers add qwen` |
| Z.ai GLM | `opencode auth login` → Z.AI | `zai/glm-4.6` | `atr providers add glm` |
| MiniMax | `opencode auth login` → MiniMax | `minimax/minimax-m2.5` | `atr providers add minimax` |
| DeepSeek | `opencode auth login` → DeepSeek | `deepseek/deepseek-chat` | `atr providers add deepseek` |
| Xiaomi MiMo | `MIMO_API_KEY` + custom `opencode.json` | `xiaomi/mimo-v2-flash` | `atr providers add mimo` |

### Ollama (local)

No API key. `ollama serve` at `http://127.0.0.1:11434`. `atr providers add ollama` writes `opencode.json` with a long `keepAlive` (default `24h`). Model id `ollama/llama3.2`. atr runs at most one task per Ollama model at a time.

### spark (local)

Custom OpenCode provider at `http://127.0.0.1:8091/v1`. `atr providers add spark`. Default model `spark/deepseek-v4-flash-0731`. Older Spark-via-Codex: `atr providers add spark --kind codex --override 'model_provider="openai"'`.

## Telegram

Chat ids only in `telegram.allowed_chat_ids`. Token only `ATR_TELEGRAM_TOKEN` in `shadow.env`.

```bash
atr telegram setup    # BotFather token + /start → allowed_chat_ids
atr telegram test
```

`/help` lists commands; they are also registered in the `/` menu. `/status`, `/tasks`, `/cancel <id>` (Confirm), `/doctor` (warn/FAIL only), `/tokens` (GitHub token owner/env var/state/days left, never values). GitHub auth/expiry alerts have a **Run doctor** button; the "⏳ working" message has **Cancel** (that chat's assistant task only). Long replies are split cleanly; >4 parts arrive as `reply.md`. Telegram reachability: debug on single timeouts, WARN after 3 in a row, `[notify]` hook `telegram_unreachable` after 15 min, shown in doctor (`telegram reachability`), `atr status` and the dashboard. Free-text (not a `/command`) from an allowed chat goes to the Telegram assistant when `[telegram.assistant] enabled = true`: first listed available provider in `telegram.assistant.providers`, else `telegram.assistant.provider`, else first available in order; the "⏳ working" message, `atr status` and the dashboard name the provider actually used, this skill + redacted status/doctor/config, one conversation per chat with resume, `telegram.assistant.timeout` as the run cap. Usage is counted like any other run.

Every change or destructive action still needs inline **Confirm** (old → new).

## Other agents (MCP)

`atr mcp` is stdio. HTTP is `POST /mcp` on `listen` with `Authorization: Bearer $ATR_MCP_TOKEN` (or the 0600 token file). atr never prints the token. Tools: `add_task` (`body` required; `title` optional from the first prompt line; workspace `none` when neither `dir` nor `repo`; output; optional `idempotency_key` replays the same create, conflicts if the body/files differ; optional `files` `[{name, content_base64|text}]` land in the worktree by filename), `wait_task`, `get_result` (text, diff, PR URL), list/steer/cancel, status, repos, providers, usage, doctor, read-only config. `POST /tasks` takes the same `files` JSON and header `Idempotency-Key`. GitHub-label method still works without MCP (`for:atr`). `atr add "prompt"` / `--dir` / `--repo` enqueue a task without an issue. Prompts, inbox files, and each attached file are capped at 256 KiB by default.

```bash
atr mcp
```

## Remote access

Full guide: `docs/REMOTE-ACCESS.md` in the atr repo.

- Recommended: keep `listen = "127.0.0.1:7420"` and run `tailscale serve --bg 7420` (macOS CLI: `/Applications/Tailscale.app/Contents/MacOS/Tailscale`). URL `https://<host>.<tailnet>.ts.net/`, tailnet-only HTTPS. Needs MagicDNS + HTTPS certificates. Check `tailscale serve status`; off `tailscale serve --https=443 off`.
- Reads (dashboard, `GET /status`, `GET /tasks/<key>`) are open. Writes (`POST /tasks`, `/messages`, `/cancel`, `/hooks`) and `/mcp` need `Authorization: Bearer` with the same token; the CLI and hook scripts send it automatically. Never use Funnel or public exposure.
- Telegram `/ui` sends the dashboard link (`[dashboard] public_url`, else detected Serve URL).
- MCP token: 0600 `~/.config/atr/mcp.token` or `ATR_MCP_TOKEN` in `shadow.env`, `[mcp] enabled = true`, `atr service restart`, `atr doctor` shows `ok mcp token`. No token → 401. Never print, paste or commit it.

## Doctor

```bash
atr doctor [--json] [--cache]
```

One `ok` / `warn` / `FAIL` line per check, then `N problems`. Checks CLIs (path + version), GitHub token (`github token api`: valid, repo access, days to expiry), Telegram token + `allowed_chat_ids`, OpenCode preset keys/URLs, repo, listen. Start every diagnosis here, then `atr status` (has a `github auth:` line) and `atr logs -f`. Full guide: `docs/TROUBLESHOOTING.md` in the atr repo.

- GitHub: atr uses one env token (`ATR_GH_TOKEN`, then `ATR_SHADOW_GH_TOKEN`, then `GH_TOKEN`) from `~/.config/atr/shadow.env`, never `gh auth login`. Fine-grained PATs are single-owner, so repos from several owners use per-owner tokens: `[github.tokens] owner = "ENV_NAME"`, managed with `atr github token set <owner>` (hidden prompt or stdin; writes shadow.env 0600 + config, validates, reloads serve), `atr github token list` (status/days, never values), `atr github token remove <owner>`. Alerts, status, dashboard and doctor (`github token api <owner>`) are per token. A failing `gh auth status` over SSH on macOS is the locked keychain, not an expired login. 401 → Telegram + `[notify] command` alert (every 6h until recovered); expiry <14 days → daily warning.
- atr no longer pins CLI versions; doctor shows `ok cli <name> <path> <version>`. An old `expected_version` key is ignored (doctor notes it); delete it.
- Over SSH on macOS, `cli cursor … (seen by service at …)` or `version skipped over SSH` is fine: serve records versions in `cli-versions.json` from the desktop session.

## Upgrade

```bash
atr upgrade [version] [--force]   # download, sha256, backup, swap, restart; no-op if already latest
atr rollback            # previous binary / db / config
```

Release repo is `[update] github_repo` (default `rimusz/agent-task-runner`).

## Repo lessons

Off by default (`[lessons] enabled = true`, or `lessons = true` in one `[[repo]]`). Short notes injected into a task's first-run prompt after the cached prefix (never plan, review or this assistant). Stored in the state DB, max 20 per repo, 4 KiB injected.

| Command | Does |
|---|---|
| `atr lessons list [--repo R] [--json]` | show lessons with evidence and hits |
| `atr lessons add --repo R "text"` | manual lesson (secrets and instruction-like text refused) |
| `atr lessons pin <id>` / `unpin <id>` | pinned lessons are never pruned |
| `atr lessons clear --repo R [--id N]` | delete one or all |
| `atr lessons export --repo R` | markdown to paste into `AGENTS.md` via a normal PR |

With lessons on, atr reflects after informative failures (verify failed, stalled, needs attention, task timeout, success after a verify retry or review): a read-only 3-turn run on `[lessons] provider` (default: the driver) under a hidden `<key>-reflect` task, at most 2 lessons of ≤ 240 chars, near-duplicates bumped instead of added, unpinned lessons expire after `ttl` (30d). A task past `task_timeout` leaves a run with outcome `task_timeout` (see `atr tasks <key>`).

## Compare

`atr add --repo R --output pr --compare grok,claude "…"` runs the task on both providers (arms `K-a`, `K-b`, each pinned to its provider, no failover, no pushes). The arm that passes verify with the smallest diff wins; its branch becomes `atr/K` and the PR comes from `K`. Neither passes → `K` is `needs_attention`, no PR. Needs verify commands for the repo; refused when a provider is disabled or over quota. About 2x tokens. `atr compare show K [--json]` shows arms, verify, diff sizes, decision and PR URL. No Telegram command yet.

## Troubleshooting

| Symptom | What to check |
|---|---|
| `cli … missing` | Binary not on `PATH`. Install the CLI or set `[[provider]] cli`. |
| key unset / `auth` warn | `opencode auth login`, or put `MOONSHOT_API_KEY` / … in `shadow.env`. Restart serve. |
| `opencode models` empty | Not logged in, or custom provider id mismatch. `opencode models --refresh`. |
| Model not found | Use exact `provider/model` from `opencode models`. |
| 401 on a run | Key missing in the **serve** environment. Desktop login does not help a LaunchAgent unless `auth.json` or `shadow.env` is sourced. |
| Ollama / spark connection refused | Local server down. Doctor: URL missing from `opencode.json`. |
| Cursor / Claude fail over SSH | Keychain locked. Run doctor and serve in the desktop session. |
| `atr submit` does nothing | `atr serve` (or the user service) not running. `/status`. |
| Usage `not reported` | Expected for OpenCode, Cursor, Codex. |
| Wanted a vendor CLI (`kimi`, `qwen`, …) | No kind. Drive the model through OpenCode. |

## Where files live

| What | Path |
|---|---|
| Config | `~/.config/atr/config.toml` |
| Secrets | `~/.config/atr/shadow.env` (0600) |
| State | `~/.local/share/atr/state.db` |
| Skill | `~/.claude/skills/atr/SKILL.md` (and Cursor/Codex/Grok/OpenCode equivalents) |
