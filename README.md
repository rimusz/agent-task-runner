# atr

## What atr is

atr is a small program that lives on your computer and runs coding agents for you. It talks to the official CLIs you already pay for — Claude Code, Grok Build, Cursor, Codex, and OpenCode — on those subscriptions. It does not call vendor APIs itself and it does not bill per run. You sign in to each CLI. atr takes a **task** — a prompt plus an optional workspace — from GitHub, MCP, the CLI, HTTP, Telegram, or a drop folder, starts the next available agent, and lets you see and steer the run from Telegram, a local web page, or the terminal.

## How you use it

Four steps: install and log in once, give it work, watch it, and keep it updated.

### Setup once

Install the binary (pick the tarball for this machine), sign in to each CLI **yourself**, then run `atr init`. atr never handles vendor login keys — those stay with Claude / Grok / Cursor / Codex / OpenCode. Or paste a prompt into your coding agent: [Install with your coding agent](#install-with-your-coding-agent).

```bash
ver=0.1.1
target=aarch64-apple-darwin   # or x86_64-unknown-linux-gnu / aarch64-unknown-linux-gnu / x86_64-unknown-linux-musl
curl -fsSL -o /tmp/atr.tgz \
  "https://github.com/rimusz/agent-task-runner/releases/download/v${ver}/atr-${ver}-${target}.tar.gz"
tar -xzf /tmp/atr.tgz -C /tmp
install -m 755 /tmp/atr ~/.local/bin/atr
```

Or from this repo: `cargo install --path .`

Sign in to the CLIs you want (on a Mac, do this at the desktop so Cursor can use the login keychain):

```bash
claude                 # Claude Code
grok                   # Grok Build
codex login            # Codex
opencode auth login    # OpenCode (Kimi, Qwen, GLM, MiniMax, DeepSeek, …)
# Cursor: sign in to the Cursor app
```

Then:

```bash
atr init          # or: atr init --yes
atr doctor
```

`atr init` writes your config, finds the CLIs you already installed, lets you pick models, installs the user service, and runs doctor. `--yes` enables every detected **and signed-in** CLI. Same check later: `atr providers detect`. It does not overwrite existing files unless you pass `--force`. For GitHub you also need `git` and `gh` — see [docs/GITHUB.md](docs/GITHUB.md). Secrets are never printed.

### Giving it work

A task is a prompt plus an optional workspace. It does not need a GitHub issue. The workspace is a git repo, a local directory, or none (answer only). In a non-git directory the agent may edit files but must stay inside that directory. With no workspace it only answers in text.

The usual GitHub path is unchanged. Label an open issue `for:atr`. atr picks it up and starts a task keyed `{prefix}-{issue}`. Later comments on the same issue are follow-ups. See [Writing a good task issue](#writing-a-good-task-issue).

```bash
gh issue edit 12 --add-label for:atr
```

Turn GitHub watching on during `atr init`, or later in config.

You can also hand it work without GitHub:

```bash
atr add "what is 2+2?"
atr add "fix the typo in notes.txt" --dir ~/notes
atr add "open a PR for the README typo" --repo ~/src/my-repo --output pr
atr submit my-task --body "also fix the footer"
```

`--dir` edits in place. `--repo` uses a git worktree. Neither flag means answer-only. `--output` is `reply` (default: answer the source), `pr`, `issue-comment`, `branch`, `patch`, `text`, or `webhook`.

Other agents and scripts can also hand it work through MCP, the HTTP API, Telegram, or an inbox folder. See [Hand work to atr](#hand-work-to-atr).

`atr serve` (or the user service) must be running for queued work to start.

### Hand work to atr

Every way in creates the same kind of task. Pick whichever suits the caller. Full field list: [docs/CONFIG.md](docs/CONFIG.md#sources).

**CLI.** `atr add` queues a task on this machine.

```bash
atr add "what is 2+2?"                                  # answer only, no workspace
atr add "fix the typo in notes.txt" --dir ~/notes       # edit a plain folder in place
atr add "fix the README typo" --repo ~/src/my-repo --output pr   # git worktree, opens a PR
```

**MCP.** Run `atr mcp` as an MCP server in your agent. `add_task` (needs `title` and `body`; optional `dir`, `repo`, `workspace`, `output`, `provider`) returns a task key. `wait_task` waits for that key to finish (`timeout_seconds`, default 120, max 3600). `get_result` returns the answer text, diff, and PR URL.

**HTTP.** While atr is running, POST JSON to the local API (default `127.0.0.1:7420`). It returns the new task `key`; read the result from `GET /tasks/{key}/result`.

```bash
curl -s -X POST http://127.0.0.1:7420/tasks \
  -H 'Content-Type: application/json' \
  -d '{"title":"Sum","body":"what is 2+2?","workspace":"none","output":"text"}'
```

**Telegram.** Send `/add` followed by a prompt, for example `/add what is 2+2?`. Telegram tasks are answer-only and the reply comes back to the chat.

**Inbox folder.** A folder atr watches. Any script or agent can drop a `.toml` or `.json` task file into it, and atr runs it like any other task. It is **off by default**. To turn it on, add this to your config and restart atr:

```toml
[[source]]
kind = "inbox"
dir = "~/.local/share/atr/inbox"   # any folder; created if missing
poll_interval = "5s"               # optional, default 5s
```

A task file needs `prompt` (or `body`). Everything else is optional:

```toml
# ~/.local/share/atr/inbox/fix-typo.toml
title = "Fix typo in notes"
prompt = "Fix the spelling mistakes in notes.txt. Change nothing else."
dir = "~/notes"          # or repo = "my-repo" for a git worktree; leave both out for answer-only
output = "reply"         # reply (default), text, or webhook; pr / branch / patch need repo
```

The same fields work as JSON in a `.json` file. atr checks the folder every `poll_interval`. Files whose names start with `.` are ignored, so write to `.name.toml` and rename when done. After pickup:

- A file that queued fine moves to `accepted/`.
- A file that could not be read or queued (bad TOML/JSON, missing prompt, bad folder) moves to `failed/`, with the reason next to it in `<name>.error.txt`.
- With the default `reply` output, the result is written to `results/<task key>.json` inside the inbox folder.

### Watching repos

One install can watch one git repo or several.

```bash
atr repos list
atr repos add owner/website --path ~/src/website
atr repos enable website
atr repos disable website
```

`atr init` can offer the folder you are already in. Status, the dashboard, usage, and Telegram show which repo a task belongs to (`atr status --repo demo-1`, `/status demo-1`). How to write several repos in config: [docs/CONFIG.md](docs/CONFIG.md).

### Watching and steering

**Telegram.** After `atr telegram setup`, you get commands and a plain-language assistant. Anything that would change config asks you to Confirm first.

```bash
atr telegram setup
atr telegram test
```

```
/status
/tasks
/repos
/task my-task
/add what is 2+2?
/usage month
/cancel my-task
```

If you type a normal sentence (not a `/command`), the assistant answers. It can only propose the same commands; each write still needs Confirm.

**Dashboard.** While atr is running, open `http://127.0.0.1:7420/`. It is read-only: what is running, what is queued, recent results, and usage. No buttons.

**Terminal.**

```bash
atr status
atr status --repo demo-1
atr tasks
atr tasks my-task
atr usage --repo demo-1 --period 7d
atr logs -f
atr doctor
atr cancel my-task --force
```

### Using atr from other agents

Other coding agents can give atr work too.

- **On this machine:** run `atr mcp` and add it as an MCP server in Claude Code, Cursor, Codex, or Grok Build. `add_task` accepts workspace (`git` / `dir` / `none`) and output; `wait_task` blocks until the task finishes (or times out); `get_result` returns text, diff, and PR URL. Snippets and the tool list: [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md#mcp-tool-reference).
- **GitHub-label method:** label an issue `for:atr`. Comments become follow-ups. Use this when the other agent lives on GitHub.

`atr serve` must be running either way.

### Maintenance

```bash
atr upgrade
atr rollback
atr providers detect
atr providers add kimi
atr providers add claude --kind claude
atr providers move claude --to 1
atr providers enable cursor
atr providers disable grok
atr service restart
atr tasks rm my-task --dry-run
atr gc --tasks --dry-run
```

`atr tasks rm` only deletes finished or failed work, never a running task. Full flag list: `atr --help`. Service files and config edits: [docs/CONFIG.md](docs/CONFIG.md).

## Where to run it and how to reach it

**Hosts.** atr runs on:

- a local Mac (Apple Silicon),
- any Linux distro (the Linux build is a static musl binary),
- a cloud VM or headless server. Run `loginctl enable-linger "$USER"` so the service keeps running after you log out.

The coding CLIs atr drives (Claude, Codex, Cursor, Grok, OpenCode and others) must be installed and signed in on that same host.

**Control.** AI agent apps such as Claude Code, Codex, Cursor, Grok Build, Grok Bot and OpenCode control atr through its MCP server:

- on the same machine, with local stdio: `atr mcp`
- from another machine, with HTTP `/mcp` over Tailscale and a bearer token

Other ways in are the GitHub `for:atr` label, `atr add` / `atr submit` over SSH, `POST /tasks`, Telegram `/add`, an inbox directory of TOML/JSON files, and the dashboard on port 7420. MCP can add, wait, fetch results, steer and cancel tasks, but it cannot change config. Details: [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md#mcp-tool-reference) and [docs/CONFIG.md](docs/CONFIG.md#mcp).

**GitHub access.** A GitHub token is **required**. Watching labels, reading issues and posting comments all go through the GitHub API, which needs a token. An SSH key cannot do that. One token is enough: your `gh auth login`, or a fine-grained PAT in `GH_TOKEN` (Issues, Contents and Pull requests read/write, plus Workflows if the repo has CI files to push). Run `gh auth setup-git` and git uses that same token for clone and push. An SSH key is **optional** and only covers git clone and push, as an alternative to the token. atr stores no credentials itself.

| What atr does | What it needs |
|---|---|
| Watch `for:atr` labels, read issues | GitHub token (required) |
| Post comments | GitHub token (required) |
| Open pull requests | GitHub token (required) |
| `git clone` / `git push` | The same token via `gh auth setup-git`, **or** an SSH key (optional) |

Details: [docs/GITHUB.md](docs/GITHUB.md).

## Install with your coding agent

Paste **one** of the prompts below into Claude Code, Codex, Cursor, or Grok on the machine that will run atr. The agent installs the binary and the user service. **You** still sign in to each vendor CLI. The prompts never ask for or handle API keys.

Release binaries: `aarch64-apple-darwin` (macOS Apple Silicon only), `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`. There is no Intel Mac (`x86_64-apple-darwin`) build — on Intel macOS use `cargo install --git`.

### macOS (Apple Silicon)

```
Install atr from https://github.com/rimusz/agent-task-runner on this Mac.

1. Confirm this is macOS Apple Silicon (arm64/aarch64). Releases are Apple Silicon only (aarch64-apple-darwin). Do not look for an Intel x86_64-apple-darwin build.
2. Download the latest GitHub release tarball atr-<ver>-aarch64-apple-darwin.tar.gz (verify the .sha256 if present). Unpack and install the atr binary to ~/.local/bin/atr (mode 755). Create ~/.local/bin if needed and put it on PATH (zsh: ~/.zprofile or ~/.zshrc).
3. If the download fails, fall back to: cargo install --git https://github.com/rimusz/agent-task-runner
4. Run atr init
5. Install the LaunchAgent (dev.atr.serve): atr service install. atr serve must run in the desktop (Aqua) session so Cursor can use the login keychain — not over SSH.
6. Run atr doctor
7. Report the binary path, the output of atr --version and the init / service / doctor results (ok/warn/FAIL). Never print secrets, tokens, or keys.

Never ask for or handle vendor API keys. I sign in to Claude, Grok, Cursor, Codex, and OpenCode myself.
```

### Linux (x86_64 / aarch64)

```
Install atr from https://github.com/rimusz/agent-task-runner on this Linux machine.

1. Detect OS and arch. Pick a release target: x86_64 + glibc → x86_64-unknown-linux-gnu; x86_64 + musl (Alpine) → x86_64-unknown-linux-musl; aarch64 + glibc → aarch64-unknown-linux-gnu.
2. Download the matching atr-<ver>-<target>.tar.gz from the latest GitHub release (verify the .sha256 if present). Install the atr binary to ~/.local/bin/atr (mode 755) and put ~/.local/bin on PATH (~/.profile or ~/.bashrc).
3. If the download fails, fall back to: cargo install --git https://github.com/rimusz/agent-task-runner
4. Run atr init
5. Install the systemd user unit: atr service install. If this host is headless, also run: loginctl enable-linger "$USER"
6. Run atr doctor
7. Report the binary path, the output of atr --version and the target used plus init / service / doctor results. Never print secrets, tokens, or keys.

Never ask for or handle vendor API keys. I sign in to each CLI myself.
```

**Arch / Omarchy.** The `x86_64-unknown-linux-gnu` or `x86_64-unknown-linux-musl` binary works. Or `pacman -S rust` and `cargo install --git https://github.com/rimusz/agent-task-runner`.

**Asahi** (Apple Silicon Mac running Linux). Use `aarch64-unknown-linux-gnu`. Asahi uses 16K pages — if that binary does not run, build from source (`cargo install --git` or `cargo install --path .`).

## Writing a good task issue

Keep the issue short and checkable. GitHub’s **atr task** template (label `for:atr`) lives in `contrib/.github/ISSUE_TEMPLATE/atr-task.md`. `atr init` can copy it into your repo after you confirm.

The agent may ask questions as comments on the issue. Reply on **that same issue**. Your reply continues the same task. Do not open a new issue just to answer.

```md
## Goal
## Where in the code, if known
## Done when
## Notes
```

Example (title: `Show (none) when the dashboard has no running tasks`):

```md
## Goal
When there are no running tasks, the dashboard Running section should say "(none)" instead of a blank table.

## Where in the code, if known
The dashboard Running empty state.

## Done when
- The page with no running tasks shows "(none)" in that section
- A live run still shows as a row

## Notes
Read-only page. Do not add buttons. Ask in a comment if the empty copy should match Queued.
```

## Telegram

1. In Telegram, open [@BotFather](https://t.me/BotFather) and send `/newbot`. Copy the token.
2. Run `atr telegram setup` (or say yes when `atr init` asks).
3. Paste the token. atr stores it privately — never in config.
4. Open the bot and send `/start`.
5. atr remembers that chat and sends a test message.

Later: `atr telegram test` to ping. Unknown chats get no reply. Full command list: `/help`.

## Dashboard

While atr is running, open `http://127.0.0.1:7420/`. It is read-only and refreshes on its own. Reaching it from another machine: [docs/CONFIG.md](docs/CONFIG.md#dashboard).

## Troubleshooting

| Symptom | What to check |
|---|---|
| `atr doctor` says a CLI is missing | Install that vendor CLI, then `atr providers detect`. |
| `atr doctor` says you are not signed in | Sign in to that CLI, then restart atr. |
| `atr submit` / `atr add` does nothing | atr is not running. Try `atr status`, then `atr service restart`. |
| GitHub issues ignored | Watching is off, or the label is not `for:atr`. See [docs/GITHUB.md](docs/GITHUB.md). |
| `atr doctor` — `gh auth` FAIL | Run `gh auth login`, or put `GH_TOKEN` in `shadow.env`. |
| Cursor / Claude fail over SSH | Run atr on the desktop, not over SSH. |
| `git push` or opening a PR fails | GitHub login is missing in the running service. Never print the token. |

More detail: [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) and [docs/GITHUB.md](docs/GITHUB.md).

## More documentation

| Page | What it covers |
|---|---|
| [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) | How providers, caching, planning, MCP, and CI fit together |
| [docs/CONFIG.md](docs/CONFIG.md) | Config file, paths, several repos, dashboard bind |
| [docs/GITHUB.md](docs/GITHUB.md) | Signing in to GitHub (`gh`, token, SSH) |
| [docs/DESIGN.md](docs/DESIGN.md) | Internals for people changing atr |

## Trademarks and terms

atr is not affiliated with or endorsed by Anthropic, xAI, Anysphere, OpenAI, or
the OpenCode authors. Claude, Grok, Cursor, Codex, ChatGPT, and OpenCode are
trademarks of their owners.

You must follow each provider’s terms. atr is for personal use of **your own**
CLI accounts and subscriptions. Do not resell access or share accounts.

atr is licensed under MIT OR Apache-2.0. See `LICENSE-MIT` and `LICENSE-APACHE`.
Dependency licenses are in `THIRD_PARTY_LICENSES`.
