---
name: atr
description: Set up and operate atr (agent task runner) — init, providers, OpenCode/Chinese CLIs, Ollama/spark, Telegram, doctor, upgrade, troubleshooting.
---

# atr

`atr` is a long-running service that drives official agent CLIs on existing subscription plans — never per-use API billing.

Kinds: `claude`, `grok`, `cursor`, `codex`, `opencode`. A `[[provider]]` block can be just `name` + `kind`. Nothing else is hardcoded — policy lives in `~/.config/atr/config.toml`.

OpenCode covers Ollama and the Chinese providers (Kimi, Qwen, GLM, MiniMax, DeepSeek, MiMo) plus local spark. **Any model OpenCode supports works with atr by its `provider/model` id.** Presets are shortcuts only.

Secrets live in `~/.config/atr/shadow.env` (mode 0600), never in config.

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

## Init

```bash
atr init          # or: atr init --yes
atr telegram setup
atr doctor
```

`atr init` writes `~/.config/atr` (config, `system-prompt.md`, `atr-serve.sh`), detects CLIs, installs this skill into each CLI’s skill dir, installs the user service (LaunchAgent `dev.atr.serve` on macOS, systemd user unit on Linux), and runs doctor. It never overwrites existing files without `--force` (it merges missing keys). It will not replace a loaded `dev.atr.serve` job or install a service for a non-default config unless you pass `--force-service`. On macOS the service must run in the desktop session so Cursor can use the login keychain.

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

`/help` lists commands. Free-text (not a `/command`) from an allowed chat goes to the Telegram assistant when `[telegram.assistant] enabled = true`: first available provider/model in order (or `telegram.assistant.provider`), this skill + redacted status/doctor/config, one conversation per chat with resume, `telegram.assistant.timeout` as the run cap. Usage is counted like any other run.

Every change or destructive action still needs inline **Confirm** (old → new).

## Other agents (MCP)

`atr mcp` is stdio. HTTP is `POST /mcp` on `listen` with `Authorization: Bearer $ATR_MCP_TOKEN` (or the 0600 token file). atr never prints the token. Tools: `add_task` (`body` required; `title` optional from the first prompt line; workspace `none` when neither `dir` nor `repo`; output), `wait_task`, `get_result` (text, diff, PR URL), list/steer/cancel, status, repos, providers, usage, doctor, read-only config. GitHub-label method still works without MCP (`for:atr`). `atr add "prompt"` / `--dir` / `--repo` enqueue a task without an issue. Prompts and inbox files are capped at 256 KiB by default.

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

One `ok` / `warn` / `FAIL` line per check, then `N problems`. Checks CLIs, pins, Telegram token + `allowed_chat_ids`, OpenCode preset keys/URLs, repo, listen.

## Upgrade

```bash
atr upgrade [version] [--force]   # download, sha256, backup, swap, restart; no-op if already latest
atr rollback            # previous binary / db / config
```

Release repo is `[update] github_repo` (default `rimusz/agent-task-runner`).

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
