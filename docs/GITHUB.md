# GitHub authentication

atr talks to GitHub in two places. **The poller** (`atr serve`) reads open
issues with your label and later comments on those issues. **The agent** (not
atr itself) pushes a branch, opens a pull request, and posts comments. You
therefore need:

- **API access** via `gh` — list issues, read comments, `gh issue comment`,
  `gh pr create`
- **`git push` access** — HTTPS (token or `gh`) or SSH

`atr init` detects which of the three setups below is already present and
prints a guide. `atr doctor` checks each. **Nothing in this page, in doctor,
or in init ever prints a token or a key.**

## Token required, SSH key optional

A GitHub token is **required**. Watching labels, reading issues and posting
comments all use the GitHub API, which needs a token; an SSH key cannot do
that. An SSH key is **optional** and covers only `git clone` / `git push`, as
an alternative to using the token for git.

| What atr does | What it needs |
|---|---|
| Watch `for:atr` labels, read issues | GitHub token (required) |
| Post comments | GitHub token (required) |
| Open pull requests | GitHub token (required) |
| `git clone` / `git push` | The same token via `gh auth setup-git`, **or** an SSH key (optional) |

## What to pick

| Setup | Use when | API (`gh`) | `git push` |
|---|---|---|---|
| [`gh auth login`](#1-gh-auth-login) | You sit at this desktop | yes (keyring) | HTTPS via `gh` |
| [Token in `shadow.env`](#2-token-in-shadowenv) | Headless Mini, LaunchAgent, SSH session | yes (`GH_TOKEN`) | HTTPS via token |
| [SSH key](#3-ssh-key-for-git-push) + gh/token | Remotes are `git@github.com:…` | gh or token | SSH |

A token plus SSH is the usual Mini combo. GitHub App installation tokens can
occupy the same `GH_TOKEN` slot; atr does not mint or refresh them.

## 1. `gh auth login`

**When:** a Mac or Linux machine you log into with a browser (or a device-code
flow). Simplest for a newcomer.

**Pros:** no token file; `gh` stores the credential in the OS keyring; `git`
HTTPS can use the same login.
**Cons:** on macOS the token lives in the **login keychain**. A headless SSH
session, LaunchAgent, or `atr serve` started without an unlocked desktop often
cannot read it (`gh auth status` fails). Use a [token](#2-token-in-shadowenv)
on a Mini.

```bash
gh auth login
# GitHub.com → HTTPS → authenticate Git with GitHub credentials → Yes
gh auth status          # must succeed; atr doctor runs this
```

Required access is whatever `gh` granted. Prefer the `repo` scope (classic) or
the fine-grained permissions below.

## One token for atr

atr itself (issue poller, shadow poller, `atr doctor`) uses **one** token from
the environment and hands it to `gh` as `GH_TOKEN`, so it never relies on the
keychain-backed `gh auth login` (which is locked over SSH / in daemons on
macOS). Lookup order, process env first then `shadow.env`:

1. `ATR_GH_TOKEN` (recommended)
2. `[shadow] gh_token_env` (default `ATR_SHADOW_GH_TOKEN`) — existing setups keep working
3. `GH_TOKEN`, then `GITHUB_TOKEN`

Use a **fine-grained PAT** scoped to the target repos with **Contents**,
**Issues**, **Pull requests**: Read and write, and **Metadata**: Read.

```bash
umask 077
printf 'ATR_GH_TOKEN=%s\n' 'paste-the-token-here' >> ~/.config/atr/shadow.env
chmod 600 ~/.config/atr/shadow.env
atr service restart && atr doctor
```

Health monitoring:

- A 401 (or non-rate-limit 403) logs `ERROR` and sends a Telegram alert to the
  owner chat (first `allowed_chat_ids`) and the `[notify] command` hook. At
  most once per 6h while failing, plus a "recovered" notice.
- atr probes `GET /user` hourly and reads `Github-Authentication-Token-Expiration`;
  within 14 days of expiry it warns once a day.
- `atr doctor` → `github token api`: token present, valid (login), access to
  each watched repo, days to expiry (warn under 14, FAIL on 401).
- `atr status` and the dashboard show the state (red = failing, amber = expiring).

More: [TROUBLESHOOTING.md](TROUBLESHOOTING.md#5-github-token).

## Per-owner tokens

Fine-grained PATs belong to **one resource owner** (a user or one org). A
token made for `your-user` cannot see `your-org/app`, so watching repos from
several owners needs one token per owner. Map each owner to an env var
**name** in config; the values stay in `shadow.env`:

```toml
[github.tokens]
your-user = "ATR_GH_TOKEN_YOUR_USER"
YourOrg   = "ATR_GH_TOKEN_YOURORG"
```

Every GitHub call (issue poller, shadow, doctor) picks the token by the repo's
owner (case-insensitive). Owners without a mapping, or whose variable is unset,
use the default chain (`ATR_GH_TOKEN` → shadow token → `GH_TOKEN` →
`GITHUB_TOKEN`). Values are read from the process env, then `shadow.env`.

Manage them with the CLI (values are never echoed or printed):

```bash
atr github token set YourOrg            # hidden prompt; or: pass-cmd | atr github token set YourOrg
atr github token set YourOrg --env-var MY_ORG_TOKEN
atr github token list                   # owner, env var, ok/warn/fail, days left, repos
atr github token remove YourOrg         # drops mapping + value; owner falls back to default
```

`set` writes `ATR_GH_TOKEN_<OWNER>` to `shadow.env` atomically (0600, replaces
an existing value), adds `[github.tokens]` in `config.toml` keeping comments,
checks the token (login, access to that owner's watched repos, expiry), and
sends SIGHUP to a running `atr serve`. If serve was started with that variable
already in its environment, run `atr service restart` after rotating it.

Auth alerts, recovery notices, expiry warnings, `github-auth.json`,
`atr status`, the dashboard banner and `atr doctor` (`github token api <owner>`)
are all per token and name the owner and env var, never the value.

## 2. Token in `shadow.env`

**When:** LaunchAgent / systemd, a Mini you only reach over SSH, or any host
where the keyring is locked.

**Never put a token in `config.toml`.** Store it in `~/.config/atr/shadow.env`
(mode `0600`) as `GH_TOKEN` (or `GITHUB_TOKEN` — `gh` accepts both).
`atr-serve.sh` sources that file before `atr serve`. Shadow mode uses a
**different** env name (`shadow.gh_token_env`, default `ATR_SHADOW_GH_TOKEN`).

### Fine-grained PAT (preferred)

1. GitHub → **Settings** → **Developer settings** → **Fine-grained personal
   access tokens** → **Generate new token**.
2. Resource owner / repository access: **only** the repos atr watches.
3. Permissions (this repo):
   - **Contents** — Read and write (`git clone`, `git push` of task branches)
   - **Issues** — Read and write (`gh issue list/view/comment/edit`, `gh label create`)
   - **Pull requests** — Read and write (`gh pr create`, `gh pr comment`)
   - **Workflows** — Read and write, only if tasks push changes under `.github/workflows`
   - **Metadata** — Read (always required)

   That is the minimal set. This one token covers both watching labels and
   pushing: run `gh auth setup-git` so git uses it too. No SSH key needed.
4. Generate. Copy once. Do not paste it into chat, config, or the plist.

```bash
umask 077
printf 'GH_TOKEN=%s\n' 'paste-the-token-here' >> ~/.config/atr/shadow.env
chmod 600 ~/.config/atr/shadow.env
atr service restart
atr doctor
```

**Pros:** works in daemons; repo-scoped; you can see exact permissions.
**Cons:** you rotate it; GitHub expires fine-grained tokens; a too-narrow
token fails `git push` or `gh pr create`.

### Classic PAT

1. **Settings** → **Developer settings** → **Personal access tokens** →
   **Tokens (classic)**.
2. Scopes: `repo`. Add `workflow` only if agents will edit `.github/workflows`.
3. Store it the same way (`GH_TOKEN=` in `shadow.env`, mode `0600`).

### GitHub App token

atr has **no** App integration. If you already have a short-lived installation
token (`ghs_…`), put it in `GH_TOKEN` the same way as a PAT. It expires
(often ~1 hour). For a long-running `atr serve`, use a PAT or `gh auth login`.

## 3. SSH key for `git push`

Optional. A single token (§1 or §2) plus `gh auth setup-git` already covers
git. An SSH key is only an alternative for git clone and push; if the host has
one GitHub accepts, git uses it and atr never reads or stores it.

**When:** `git remote -v` shows `git@github.com:owner/repo.git`. SSH does
**not** replace `gh` — you still need [§1](#1-gh-auth-login) or
[§2](#2-token-in-shadowenv) for issues, comments, and PRs.

```bash
ssh-keygen -t ed25519 -C "atr@$(hostname)" -f ~/.ssh/id_ed25519_atr -N ""
# User key: GitHub → Settings → SSH and GPG keys → New SSH key
# Or a repo deploy key: Settings → Deploy keys → Add deploy key → Allow write access
```

`~/.ssh/config`:

```
Host github.com
  HostName github.com
  User git
  IdentityFile ~/.ssh/id_ed25519_atr
  IdentitiesOnly yes
```

```bash
ssh -T git@github.com
# expect: Hi <you>! You've successfully authenticated…
```

## Shadow mode (read-only token)

`[shadow]` clones and polls with a **read-only** token named by
`shadow.gh_token_env` (default `ATR_SHADOW_GH_TOKEN`). Put it in the same
`shadow.env` file. The poller/clone uses it; the agent never sees it.

Minimum fine-grained permissions for shadow: **Contents** read, **Issues**
read, **Metadata** read, **Pull requests** read. Do not give that token write.

On a shadow run atr:

1. Removes `GH_TOKEN`, `GITHUB_TOKEN`, and `shadow.gh_token_env` from the
   **agent** process.
2. Wraps `gh` so writes (`gh pr create`, `gh issue comment`, `gh auth token`,
   …) are refused.
3. Sets `remote.origin.pushurl=no_push` and a `pre-push` hook on the worktree.

Regular (non-shadow) agents **keep** `GH_TOKEN` so they can comment and open
PRs. atr itself never logs the value.

## What `atr doctor` checks

| Check | Command / source | Ok when |
|---|---|---|
| `gh auth` | `gh auth status` | logged in, or skipped if GitHub intake is off |
| `gh token` | env + `shadow.env` (name only) and scopes from `gh auth status` / `gh api -i /user` headers | `GH_TOKEN` set, or unset because `gh auth login` works; warns if Contents/Issues/PRs/Metadata (or classic `repo`) are missing |
| `ssh github` | `ssh -T git@github.com` | authenticated; **warn** (not fail) if remotes are HTTPS |
| `github token api` | `GET /user` + `GET /repos/<repo>` with the atr token | valid, repo access, ≥14 days to expiry (warn <14d, FAIL on 401 / no access) |
| `shadow token` | configured env name in env or `shadow.env` | set when `[shadow] enabled` |
| `shadow.env mode` | file bits | `0600` (warns if group/other can read) |

Doctor prints `GH_TOKEN set` / `in shadow.env` / `unset` — never the value,
never `--show-token`. Tokens are never printed.

## Troubleshooting

| Symptom | What to check |
|---|---|
| `atr doctor` — `gh auth` FAIL | `gh auth login`, or put `GH_TOKEN` in `shadow.env`. macOS over SSH / LaunchAgent: keychain locked — use a token. |
| `gh auth` warn + token in `shadow.env` | This shell has no `GH_TOKEN`; serve will after `atr service restart`. |
| `gh token` missing permissions | Fine-grained: Contents RW, Issues RW, Pull requests RW, Metadata R. Classic: `repo`. |
| `ssh github` warn | Only needed for SSH remotes. `ssh-keygen`; add the key; `ssh -T git@github.com`. Or switch the remote to HTTPS. |
| `git push` Permission denied (publickey) | SSH key not loaded / not a write deploy key. Or HTTPS remote and token lacks Contents write. |
| Agent comments / `gh pr create` 401 | Token missing in the **serve** environment. Edit `shadow.env`, then restart. Do not paste the token into the terminal to “check” it. |
| Shadow agent tried to push or comment | Expected. Tokens are stripped; `gh` writes are wrapped. |
| Token leaked into a log or chat | Rotate it on GitHub. atr never prints secrets; something else did. |
