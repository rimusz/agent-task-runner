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
| `github token api` | token expires within 14 days, or api.github.com unreachable | 401 (revoked/expired), or no access to a watched repo, or no token | Make a new PAT, set `ATR_GH_TOKEN` in `~/.config/atr/shadow.env`, `atr service restart` |
| `ssh github` | no SSH key (fine for HTTPS remotes) | — | Only needed for SSH remotes |
| `shadow token` | — | shadow on, no token | Any of `ATR_GH_TOKEN` / `ATR_SHADOW_GH_TOKEN` in `shadow.env` |
| `shadow.env mode` | group/other can read it | — | `chmod 600 ~/.config/atr/shadow.env` |
| `cli <name>` | version doesn't contain `expected_version` | CLI path missing | Update the CLI or the pin; install it or set `cli` |
| `repo_path`, `listen`, `system_prompt` | — | path missing / port busy | Fix the path, or stop whatever holds the port |
| `telegram`, `telegram token`, `telegram chat` | config incomplete | token set but unusable / no chat | `atr telegram setup`, then `atr telegram test` |
| `mcp token`, `mcp http` | — | MCP on but no token | create `~/.config/atr/mcp.token` (0600) |
| `atr on PATH` | `atr` not on PATH | — | re-run `install.sh` or add `~/.local/bin` to PATH |

### `cli cursor pin` over SSH on macOS

Cursor (and some other CLIs) read the login keychain, which is locked in an SSH
session. Doctor can't run `--version` there, so it uses the result `atr serve`
records at startup in `~/.config/atr/cli-pins.json` (serve runs in the desktop
session, where the keychain is unlocked):

- `pin=ok … (verified by service at …)` — fine.
- `pin=MISMATCH … (service check at …)` — real warning; update the CLI or pin.
- `pin skipped over SSH (keychain locked); service session verifies at start` —
  no record yet (serve not started since upgrade). Not a problem; it shows `ok`.

## 2. `atr status` and the dashboard

`atr status` (or `--json`) shows providers, running/queued tasks, and a
`github auth:` line (`ok`, `warn`, `FAIL`). The dashboard
(`atr ui`, or your Tailscale Serve URL) shows a **red banner** when GitHub auth
is failing and an **amber banner** when the token expires within 14 days.
Both read `~/.config/atr/github-auth.json`, which the running service updates.

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

| Symptom | Meaning | Fix |
|---|---|---|
| Telegram/hook "GitHub auth failing" | GitHub returned 401 (or 403 that isn't a rate limit). Intake is stalled. Repeats every 6h until fixed, then "recovered". | New fine-grained PAT → `ATR_GH_TOKEN` in `shadow.env` → `atr service restart` → `atr doctor` |
| "GitHub token expires in N day(s)" | From the `Github-Authentication-Token-Expiration` header; daily within 14 days | Rotate before it expires |
| `gh auth status` says "failed to log in" over SSH | macOS keychain is locked in SSH sessions. **False alarm** — atr doesn't use the `gh` login. | Check `atr doctor` → `github token api` instead |
| `no access: owner/repo=404` | PAT not granted that repo | Add the repo to the token's repository access |

## 6. Telegram

- `atr doctor` → `telegram*` lines; `atr telegram setup` redoes token + chat id; `atr telegram test` sends a test message.
- No alerts arriving: `[telegram] enabled = true` and `allowed_chat_ids` set;
  first id is the owner chat that gets alerts. Logs show `telegram notify failed`.
- Free text not answered: `[telegram.assistant] enabled = true`.

## 7. Notify hook

`[notify] command` runs (via `sh -c`) for alerts, alongside Telegram:

```toml
[notify]
command = "~/bin/atr-alert.sh"   # empty = off
timeout = "10s"                   # killed after this
```

Env: `ATR_ALERT_KIND` (`github_auth_failed`, `github_auth_recovered`,
`github_token_expiring`), `ATR_ALERT_MESSAGE`, `ATR_ALERT_SOURCE=github`,
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
