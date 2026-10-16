# Troubleshooting atr

Where to look when something is wrong, in the order to look. atr never prints
tokens; don't paste them into a terminal or chat to "check" them.

## 1. `atr doctor`

```bash
atr doctor            # one ok / warn / FAIL line per check, then "N problems"
atr doctor --json     # same, machine-readable
```

`ok` is fine. `warn` works but needs attention. `FAIL` means a feature is
broken. Each non-ok line prints a `fix:` hint.

| Check | warn means | FAIL means | Fix |
|---|---|---|---|
| `config` | — | config could not load | `atr init`, or fix the TOML error it prints |
| `gh auth` | `gh auth status` fails in this shell but a token is in `shadow.env` | no `gh` login and no token | Use a token (below). A failed `gh auth status` over SSH on macOS is usually just the locked keychain. |
| `gh token` | token lacks Contents/Issues/PRs/Metadata | no token anywhere | Fine-grained PAT, see [GITHUB.md](GITHUB.md) |
| `github token api [owner]` (one per token) | token expires within 14 days, or api.github.com unreachable | 401 (revoked/expired), or no access to a watched repo, or no token | Make a new PAT, set `ATR_GH_TOKEN` in `~/.config/atr/shadow.env`, `atr service restart` |
| `ssh github` | no SSH key (fine for HTTPS remotes) | — | Only needed for SSH remotes |
| `shadow token` | — | shadow on, no token | Any of `ATR_GH_TOKEN` / `ATR_SHADOW_GH_TOKEN` in `shadow.env` |
| `shadow.env mode` | group/other can read it | — | `chmod 600 ~/.config/atr/shadow.env` |
| `cli <name>` | `--version` failed or needs login | CLI path missing | Sign in to the CLI / check it runs; install it or set `cli`. `ok` lines just show path and version. |
| `expected_version` | — (info only) | — | Old config key, ignored since CLI version pins were removed; delete it from `[[provider]]` |
| `repo_path`, `listen`, `system_prompt` | — | path missing / port busy | Fix the path, or stop whatever holds the port |
| `telegram reachability` | 3+ failed polls | unreachable 15+ min | network/DNS/proxy to api.telegram.org |
| `telegram`, `telegram token`, `telegram chat` | config incomplete | token set but unusable / no chat | `atr telegram setup`, then `atr telegram test` |
| `mcp token`, `mcp http` | — | MCP on but no token | create `~/.config/atr/mcp.token` (0600) |
| `atr on PATH` | `atr` not on PATH | — | re-run `install.sh` or add `~/.local/bin` to PATH |

### `cli cursor` version over SSH on macOS

Cursor (and some other CLIs) read the login keychain, which is locked in an SSH
session. Doctor can't run `--version` there, so it shows the version `atr serve`
recorded at startup in `~/.config/atr/cli-versions.json` (serve runs in the
desktop session, where the keychain is unlocked; the old `cli-pins.json` name is
still read):

- `<path> <version> (seen by service at …)` — fine.
- `version skipped over SSH (keychain locked); service session records it at start` —
  no record yet (serve not started since upgrade). Not a problem; it shows `ok`.

## 2. `atr status` and the dashboard

`atr status` (or `--json`) shows providers, running/queued tasks, and a
`github auth:` line (`ok`, `warn`, `FAIL`). The dashboard
(`atr ui`, or your Tailscale Serve URL) shows a **red banner** when GitHub auth
is failing and an **amber banner** when the token expires within 14 days.
Both read `~/.config/atr/github-auth.json`, which the running service updates.

The dashboard header shows `atr v<version> (<sha>)`, today's runs and success
rate, and a compact health strip: `GitHub ok`, `Telegram ok`, `N CLIs ok` and
uptime, with details (token expiry, CLI versions) on hover. A badge expands to
show its problem only when it is amber or red (expiring token, missing CLI). Repos lists watched repos (`direct`), the shadow
repo (`shadow`) and the Telegram assistant workspace (`assistant`), with the
token env var NAME used; the top-level default repo is hidden while GitHub
intake is off. Finished rows link to `tasks/<key>/result`, show the finish
time as `HH:MM` (full date on hover) and `provider / model`. Telegram assistant
tasks (`tg<chat id>`) show as `Telegram` with the chat id on hover. Today shows
uncached input, cached and cache-write tokens separately so totals add up.
Providers show `-` for usage when the CLI reports none, and Grok extra credits
in their own column. Token savings shows a per-provider bar table for a single
day and the daily chart for longer periods. Filters (repo,
status, provider, time window) and column sorting are read-only.

Wrong provider planning or answering? `atr driver` shows the driver; see
[Choosing the driver provider and model](CONFIG.md#choosing-the-driver-provider-and-model).

## 3. Logs

| OS | Where |
|---|---|
| macOS | `~/Library/Logs/atr/atr.log` (`[log] path`), startup errors in `~/Library/Logs/atr.boot.err` |
| Linux | `journalctl --user -u atr -f`, plus `[log] path` (default `~/.local/share/atr/atr.log`) |
| Both | `atr logs -f` follows `[log] path` |

Search for `ERROR`, `GitHub auth failure`, `github poll failed`, `telegram`.

## 4. Service

```bash
# macOS
launchctl print gui/$(id -u)/dev.atr.serve | head -30   # state, pid, last exit
# Linux
systemctl --user status atr
# Both
atr service restart
```

On macOS the LaunchAgent must be loaded from the desktop session (not over SSH)
so CLIs can use the login keychain. On headless Linux run
`sudo loginctl enable-linger "$USER"` once.

## 5. GitHub token

atr runs every GitHub call (issue poller, shadow, doctor) with **one token from
the environment**, passed to `gh` as `GH_TOKEN`. It never depends on
`gh auth login`. Order: `ATR_GH_TOKEN`, `[shadow] gh_token_env` (default
`ATR_SHADOW_GH_TOKEN`), `GH_TOKEN`, `GITHUB_TOKEN`; process env first, then
`~/.config/atr/shadow.env`. Setup: [GITHUB.md](GITHUB.md#one-token-for-atr).

Repos from several owners use **per-owner tokens** (`[github.tokens]`, managed
with `atr github token set|list|remove`); see
[GITHUB.md](GITHUB.md#per-owner-tokens). `atr github token list` shows each
owner's env var, ok/warn/fail and days left without printing values.

| Symptom | Meaning | Fix |
|---|---|---|
| Telegram/hook "GitHub auth failing" | GitHub returned 401 (or 403 that isn't a rate limit). Intake is stalled. Repeats every 6h until fixed, then "recovered". | New fine-grained PAT → `ATR_GH_TOKEN` in `shadow.env` → `atr service restart` → `atr doctor` |
| "GitHub token expires in N day(s)" | From the `Github-Authentication-Token-Expiration` header; daily within 14 days | Rotate before it expires |
| `gh auth status` says "failed to log in" over SSH | macOS keychain is locked in SSH sessions. **False alarm** — atr doesn't use the `gh` login. | Check `atr doctor` → `github token api` instead |
| `no access: owner/repo=404` | PAT not granted that repo, or the repo belongs to another owner (fine-grained PATs are single-owner) | Add the repo to the token, or add a per-owner token: `atr github token set <owner>` |
| One owner failing, others fine | Tokens are per owner (`[github.tokens]`); each has its own alert, banner and doctor line | `atr github token list`, then `atr github token set <owner>` |

## 6. Telegram

- `atr doctor` → `telegram*` lines; `atr telegram setup` redoes token + chat id; `atr telegram test` sends a test message.
- No alerts arriving: `[telegram] enabled = true` and `allowed_chat_ids` set;
  first id is the owner chat that gets alerts. Logs show `telegram notify failed`.
- Free text not answered: `[telegram.assistant] enabled = true`.
- `telegram reachability` / `telegram:` status line / dashboard banner:
  `warn` = 3+ getUpdates failures in a row; `FAIL` = unreachable 15+ min
  (the `[notify]` hook got `telegram_unreachable`). Check the host's network,
  DNS, proxy or VPN path to `api.telegram.org`. Single timeouts are normal and
  only logged at debug. State file: `~/.config/atr/telegram-health.json`.
- From the chat: `/doctor` (warn/FAIL only), `/tokens` (GitHub token state, no
  values), `/status`, `/tasks`. Alerts have a **Run doctor** button.
- Commands missing from the `/` menu: atr registers them with setMyCommands at
  start; restart atr (`atr service restart`) and reopen the chat.
- Very long replies arrive as `reply.md` (more than 4 message parts).

## 7. Notify hook

`[notify] command` runs (via `sh -c`) for alerts, alongside Telegram:

```toml
[notify]
command = "~/bin/atr-alert.sh"   # empty = off
timeout = "10s"                   # killed after this
```

Env: `ATR_ALERT_KIND` (`github_auth_failed`, `github_auth_recovered`,
`github_token_expiring`, `telegram_unreachable`, `telegram_recovered`), `ATR_ALERT_MESSAGE`, `ATR_ALERT_SOURCE=github`,
`ATR_ALERT_DAYS_LEFT` (expiry only). GitHub token vars are removed from the
hook's environment. Test the script by hand:

```bash
ATR_ALERT_KIND=github_auth_failed ATR_ALERT_MESSAGE="test" ~/bin/atr-alert.sh; echo $?
```

Non-zero exits and timeouts are logged as warnings.

## 8. MCP / HTTP token

Writes (`POST /tasks`, `/messages`, `/cancel`, `/hooks`) and `/mcp` need
`Authorization: Bearer` with `~/.config/atr/mcp.token` (0600) or
`ATR_MCP_TOKEN`. A 401 means a missing/wrong token in the client. Reads and the
dashboard stay open. See [REMOTE-ACCESS.md](REMOTE-ACCESS.md).
