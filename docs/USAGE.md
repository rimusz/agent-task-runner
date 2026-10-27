# Using atr

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
atr tui "what is 2+2?"
```

`--dir` edits in place. atr asks the CLI to stay inside that folder where the CLI supports it (Claude `--add-dir`, Codex sandbox `workspace-write`, Grok `--cwd`, OpenCode `--dir`). That is best-effort: some CLIs can still write outside, and atr does not snapshot-detect outside writes. Prompt files for dir tasks live in the state directory, not a `.atr/` folder in your notes. `--repo` uses a git worktree. Neither flag means answer-only. `--output` is `reply` (default: answer the source), `pr`, `issue-comment`, `branch`, `patch`, `text`, or `webhook`. Prompts (and inbox files) are capped at 256 KiB by default (`max_prompt_bytes`).

Other agents and scripts can also hand it work through MCP, the HTTP API, Telegram, or an inbox folder. See [Hand work to atr](#hand-work-to-atr).

`atr serve` (or the user service) must be running for queued work to start. `atr tui` is the exception: it runs the engine in-process and does not need serve.

### Terminal TUI (`atr tui`)

A thin terminal session: transcript plus a status footer (provider / model / tokens / cache hit % when known). It does **not** clone the dashboard and does **not** share history with Telegram.

```bash
atr tui                      # interactive; /quit to leave
atr tui "what is 2+2?"       # one turn, print the answer, exit
atr tui --session work --provider grok
```

Sessions are normal atr tasks keyed `cli-<name>` (default `cli-default`), the same idea as Telegram's `tg<chat>` keys but a separate history. Resume, the stable prompt prefix, and provider-switch handoff are the existing task machinery.

Provider for a TUI session: session pin (`/provider` or `--provider`) → `ATR_TUI_PROVIDER` → optional `[tui] providers` list → global `[[provider]]` order. Mid-session `/provider` and `/model` stick for that session; switching provider triggers the existing handoff brief.

| Flag / env | Effect |
|---|---|
| `--provider` / `ATR_TUI_PROVIDER` | Preferred provider at launch |
| `--session <name>` | Task key `cli-<name>` (default `default`) |
| `--dir` / `--repo` | Workspace; omit for answer-only |
| `--new` | Drop the previous `cli-<name>` task |
| `ATR_DEFAULT=tui` | Bare `atr` is `atr tui` (explicit subcommands unchanged) |

Interactive commands: `/help` `/status` `/provider <name>` `/model <name>` `/new` `/quit`.

### Hand work to atr

Every way in creates the same kind of task. Pick whichever suits the caller. Full field list: [CONFIG.md](CONFIG.md#sources).

**CLI.** `atr add` queues a task on this machine.

```bash
atr add "what is 2+2?"                                  # answer only, no workspace
atr add "fix the typo in notes.txt" --dir ~/notes       # edit a plain folder in place
atr add "fix the README typo" --repo ~/src/my-repo --output pr   # git worktree, opens a PR
```

**MCP.** Run `atr mcp` as an MCP server in your agent. `add_task` needs `body` (`title` is optional and defaults to the first prompt line). With neither `dir` nor `repo`, workspace is `none`, matching CLI and HTTP. Optional `dir`, `repo`, `workspace`, `output`, `provider`. Optional `idempotency_key`: a retry with the same key and the same request returns the existing task (`replayed: true`); the same key with a different body, workspace, output, or files is a conflict. Optional `files`: `[{name, content_base64}]` or `[{name, text}]` — each file is written into the task worktree under that filename when the worktree is ready. Returns a task key. `wait_task` waits for that key to finish (`timeout_seconds`, default 120, max 3600). `get_result` returns the answer text, diff, and PR URL.

**HTTP.** While atr is running, POST JSON to the local API (default `127.0.0.1:7420`). It returns the new task `key`; read the result from `GET /tasks/{key}/result`. Writes need `Authorization: Bearer` (same token as `/mcp`). Optional header `Idempotency-Key` (JSON field `idempotency_key` is a fallback): a retry with the same key and the same request returns the existing task (`replayed: true`); the same key with a different body, workspace, output, or files is HTTP 409. Optional `files` (alias `attachments`): `[{name, content_base64}]` or `[{name, text}]` — each file lands in the worktree under that filename when the worktree is ready.

```bash
curl -s -X POST http://127.0.0.1:7420/tasks \
  -H "Authorization: Bearer $ATR_MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: sum-once' \
  -d '{"title":"Sum","body":"what is 2+2?","workspace":"none","output":"text"}'

# Attach a file by name (text or base64). It appears in the worktree as brief.md.
curl -s -X POST http://127.0.0.1:7420/tasks \
  -H "Authorization: Bearer $ATR_MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"title":"Read brief","body":"summarize brief.md","workspace":"none","output":"text","files":[{"name":"brief.md","text":"hello attach"}]}'
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

`atr init` can offer the folder you are already in. Status, the dashboard, usage, and Telegram show which repo a task belongs to (`atr status --repo demo-1`, `/status demo-1`). How to write several repos in config: [CONFIG.md](CONFIG.md).

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
/ui
```

`/ui` replies with a dashboard link: `[dashboard] public_url` if set, else a detected Tailscale Serve `https://…ts.net/` URL, else the local `atr ui --print` address (marked local-only).

If you type a normal sentence (not a `/command`), the assistant answers. It can only propose the same commands; each write still needs Confirm.

The assistant uses `[telegram.assistant] provider`, or the first available provider. To give it a fallback order, set `providers`: each turn takes the first listed provider that is enabled and not busy, cooling down, rate-limited or at quota, and moves down the list if a run fails. The "⏳ working" message, `atr status` and the dashboard name the provider actually used.

```bash
atr config set telegram.assistant.providers '["grok", "codex", "claude"]'
atr config get telegram.assistant.providers
```

`providers` wins over `provider` (doctor warns when both are set); names must be `[[provider]]` names. See [CONFIG.md](CONFIG.md).

**Dashboard.** While atr is running, open `http://127.0.0.1:7420/`. It is read-only: what is running, what is queued, recent results, and usage. No buttons. Times are in `display_timezone`, shown once in the header and not repeated after each time. Plan usage reads like `5h 0% · resets Oct 16; 7d 4% · resets Oct 17`; hover a reset date for the full time and zone.

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
atr retry my-task
atr pause
atr resume
atr config get github.label
atr config set github.label for:atr
```

`atr retry` starts a fresh attempt: the last consumed prompt is marked pending again so the scheduler can dispatch (no extra `atr submit` needed), the `task_timeout` wall clock restarts, and the provider is chosen again from the current config (a Telegram assistant task follows `[telegram.assistant]` as it is now; a compare arm keeps its pinned provider). `atr pause` / `atr resume` / `atr retry` call the running `atr serve` (bearer token, read from the same token file as `atr cancel`). If the service is not reachable they write the state database instead and the change applies at the next start. `atr config get/set` use the same keys and validation as Telegram `/config` (secret keys refused, comments kept, `config.toml.bak` written, service reloaded with SIGHUP).

Telegram command to CLI:

| Telegram | CLI | HTTP (bearer token) |
|---|---|---|
| `/status [repo]` | `atr status [--repo R]` | `GET /status` |
| `/tasks [repo]` | `atr tasks [--repo R]` | `GET /dashboard.json` |
| `/task <key>` | `atr tasks <key>` | `GET /tasks/<key>` |
| `/add <prompt>` | `atr add <prompt>` | `POST /tasks` |
| `/cancel <key> [force]` | `atr cancel <key> [--force]` | `POST /tasks/<key>/cancel` |
| `/retry <key>` | `atr retry <key>` | `POST /tasks/<key>/retry` |
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

### Repo lessons

Short notes from earlier runs in a repo, for example "run `npm ci` before the tests in a fresh checkout". atr adds them to the start of a task's first run in that repo, after the cached prompt prefix, so prompt caching is unaffected. Lessons are off by default: turn them on for every repo with `[lessons] enabled = true`, or for a single repo with `lessons = true` in its `[[repo]]` block (see [CONFIG.md](CONFIG.md#lessons)).

```bash
atr lessons list [--repo R] [--json]
atr lessons add --repo R "Run npm ci before npm test; a fresh checkout has no node_modules."
atr lessons pin 3            # never pruned
atr lessons unpin 3
atr lessons clear --repo R [--id 3]
atr lessons export --repo R  # markdown, to copy into AGENTS.md by hand
```

Lessons are refused if they look like they contain a secret (token prefixes, `KEY=value`, `Bearer …`, private keys, URLs with passwords, values from `shadow.env`) or like instructions (`git push`, `gh pr create`, "ignore previous instructions", …). With lessons on, atr also writes up to 2 lessons itself after an informative failure (see [CONFIG.md](CONFIG.md#lessons)); `atr lessons list` shows them with their evidence (task, run, signal). There is no Telegram command for lessons yet.

### Compare two providers

Run the same task on two providers and keep the better result:

```bash
atr add --repo R --output pr --compare grok,claude "Fix the flaky date parser test"
atr compare show <key> [--json]   # arms, outcomes, verify, diff sizes, decision, PR URL
```

atr creates the task `K` plus two arms, `K-a` (first provider) and `K-b`. Each arm runs only on its own provider: if that provider hits a limit, auth error or stall, the arm fails instead of moving to the other provider. Arms work in their own worktrees and can't push or write to GitHub.

When both arms are done, atr keeps the arm that **passed verify** with the **smallest diff** against the repo's base branch (added + removed lines, untracked files included). Ties go to fewer files, then fewer verify retries, fewer tokens, lower cost, shorter run time, then the first arm. If only one arm passed, it wins (a *walkover* when the other ended before verify). An empty diff never beats a real change. The winner's branch becomes `atr/K` and its output (for `--output pr`, the PR, with a short comparison table in the body) is delivered from `K`. The loser's worktree and local branch are removed. Follow-ups on `K` continue the winner's session in its tree.

If neither arm passes verify, there is no PR: `K` goes to `needs_attention` and both arm worktrees are kept for you to inspect.

Compare needs verify commands for the repo (`[verify]`, see [CONFIG.md](CONFIG.md)); without them it can't judge, so `atr add --compare` refuses. It also refuses when either provider is disabled, missing its CLI, or over quota (a compare never falls back to a single run). It costs about twice the tokens of a single run. `--compare` can't be combined with `--provider`, `--dir` or `--needs`. Not there yet: prompt variants on one provider, more than two arms, picking an arm by hand, and Telegram/dashboard views.

### Using atr from other agents

Other coding agents can give atr work too.

- **On this machine:** run `atr mcp` and add it as an MCP server in Claude Code, Cursor, Codex, or Grok Build. `add_task` accepts workspace (`git` / `dir` / `none`), output, optional `idempotency_key`, and optional `files`; `wait_task` blocks until the task finishes (or times out); `get_result` returns text, diff, and PR URL. Snippets and the tool list: [HOW-IT-WORKS.md](HOW-IT-WORKS.md#mcp-tool-reference). Local Agent Plugin: [plugins/atr-operator](../plugins/atr-operator/README.md).
- **GitHub-label method:** label an issue `for:atr`. Comments become follow-ups. Use this when the other agent lives on GitHub.

`atr serve` must be running either way.

### Providers and OpenCode

atr drives Claude Code, Grok Build, Cursor and Codex through their own CLIs. **OpenCode** is the fifth supported CLI, and through it atr can use many more providers: DeepSeek, Kimi, Qwen, GLM, MiniMax, local Ollama, a self-hosted vLLM server, and any other model OpenCode supports, by its `provider/model` id. Sign in with `opencode auth login` (local Ollama needs no key).

Shortcuts for common ones:

```bash
atr providers add deepseek
atr providers add kimi
atr providers add ollama
```

Or write it in config yourself. Order in the file is the fallback order:

```toml
[[provider]]
name = "claude"
kind = "claude"

[[provider]]
name = "deepseek"
kind = "opencode"
model = "deepseek/deepseek-chat"

[[provider]]
name = "local"
kind = "opencode"
model = "ollama/llama3.2"
```

More, including self-hosted vLLM: [HOW-IT-WORKS.md](HOW-IT-WORKS.md#providers).

### Grok Build as the main provider

Grok Build is the recommended main provider. When it's enabled and `driver` is left empty, atr uses Grok to plan big issues and to review diffs (review is off until you set `[review] enabled = true`). Grok also gives atr a few extras:

- **Live progress.** With `plugin = true` on the grok provider, atr's plugin sends tool, subagent and stop events to the dashboard and Telegram as the run goes.
- **Cost and tokens.** After each run, `grok usage` gives the session's token counts and cost for the dashboard.
- **Resume and fork.** A retry after a failed check, stall or review forks the session, so the original is kept.
- **Turn limits.** Every Grok run gets `--max-turns`.
- **Built-in review.** Review uses Grok's bundled `/review` skill.

You need Grok Build 1.0.52 or newer; `atr doctor` checks it. Everything else in atr works with any provider, and these extras just switch off when the driver isn't Grok. Details: [HOW-IT-WORKS.md](HOW-IT-WORKS.md#grok-extras-1052).

### Maintenance

```bash
atr upgrade               # no-op if already on the latest release
atr upgrade --force       # reinstall the latest release anyway
atr rollback
atr skills install
atr providers detect
atr providers add kimi
atr providers add claude --kind claude
atr providers move claude --to 1
atr providers enable cursor
atr providers disable grok
atr service restart
atr reset --dry-run
atr tasks rm my-task --dry-run
atr gc --tasks --dry-run
```

`atr skills install` writes the atr agent skill (`skills/atr/SKILL.md`) for each coding CLI installed on this machine: `~/.claude` (claude), `~/.cursor` (cursor-agent), `~/.codex` (codex), `~/.grok` (grok) and `~/.config/opencode` (opencode). CLIs that aren't installed are skipped and their folders aren't created. Config files are never touched. `atr init` does the same. Run it again after installing a new CLI, or on each host that runs atr.

`atr tasks rm` only deletes finished or failed work, never a running task.

`atr reset` is a full reset. It lists what it will remove and asks before doing it (`--yes`/`-y` skips the prompt; without a terminal it refuses unless you pass `--yes`; `--dry-run` only prints the list). It stops and uninstalls the service, then deletes only atr's own files: `~/.config/atr` (config, secrets, launcher), `~/.local/share/atr` (task database, logs, backups, inbox), `~/atr-work` (worktrees), `~/Library/Logs/atr` and `~/Library/Logs/atr.boot.err` on macOS, and the `skills/atr/` folders under `~/.claude`, `~/.cursor`, `~/.codex`, `~/.grok` and `~/.config/opencode`. It never touches provider configs, your repos, or the `atr` binary; configured paths outside those dirs, or a dir that holds a watched repo, are listed as kept. Run `atr init` afterwards to set up again. Full flag list: `atr --help`. Service files and config edits: [CONFIG.md](CONFIG.md).

## Remote access

To open the dashboard from a phone or another Mac, or connect a remote MCP client, use Tailscale Serve and an MCP token: [REMOTE-ACCESS.md](REMOTE-ACCESS.md). The dashboard has no login; your tailnet is what protects it. Never use Tailscale Funnel.

## How other agents and tools reach atr

**Control.** AI agent apps such as Claude Code, Codex, Cursor, Grok Build, Grok Bot and OpenCode control atr through its MCP server:

- on the same machine, with local stdio: `atr mcp`
- from another machine, with HTTP `/mcp` over Tailscale and a bearer token ([REMOTE-ACCESS.md](REMOTE-ACCESS.md))

Other ways in are the GitHub `for:atr` label, `atr add` / `atr submit` over SSH, `POST /tasks`, Telegram `/add`, an inbox directory of TOML/JSON files, and the dashboard on port 7420. MCP can add, wait, fetch results, steer and cancel tasks, but it cannot change config. Details: [HOW-IT-WORKS.md](HOW-IT-WORKS.md#mcp-tool-reference) and [CONFIG.md](CONFIG.md#mcp).

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

Commands (allowed chats only; also registered in the `/` menu via setMyCommands):

| Command | What it does |
|---|---|
| `/status` | queue, running tasks, providers |
| `/tasks`, `/task <id>` | recent and running tasks; one task's detail |
| `/cancel <id>` | cancel a task (asks Confirm) |
| `/doctor` | `atr doctor`, warn/FAIL lines only |
| `/tokens` | GitHub tokens: owner, env var, ok/warn/fail, days left — never values |
| `/add`, `/usage`, `/ui`, `/help` | queue work, spend, dashboard link, everything else |

Buttons: GitHub auth-failure and token-expiry alerts carry **Run doctor**; the
assistant's "⏳ working" message carries **Cancel** (only cancels that chat's
own assistant task). Button presses are checked against `allowed_chat_ids`.

Long replies are split at paragraph / code-fence boundaries (each part stays
valid formatting, under Telegram's 4096-char limit). Replies that would need
more than 4 parts arrive as one `reply.md` document.

Reachability: a getUpdates timeout is logged at debug; WARN after 3 failures
in a row; INFO when it recovers. Unreachable for 15+ minutes fires the
`[notify] command` hook (`ATR_ALERT_KIND=telegram_unreachable`, then
`telegram_recovered`) — Telegram itself can't deliver that alert. State shows
in `atr doctor` (`telegram reachability`), `atr status` (`telegram:` line) and
a dashboard banner.

## Dashboard

While atr is running, open `http://127.0.0.1:7420/`. It is read-only and refreshes on its own. Reaching it from another machine: [CONFIG.md](CONFIG.md#dashboard).

## Troubleshooting

| Symptom | What to check |
|---|---|
| `atr doctor` says a CLI is missing | Install that vendor CLI, then `atr providers detect`. |
| `atr doctor` says you are not signed in | Sign in to that CLI, then restart atr. |
| `atr submit` / `atr add` does nothing | atr is not running. Try `atr status`, then `atr service restart`. (`atr tui` does not need serve.) |
| GitHub issues ignored | Watching is off, or the label is not `for:atr`. See [GITHUB.md](GITHUB.md). |
| `atr doctor` — `gh auth` FAIL | Run `gh auth login`, or put `GH_TOKEN` in `shadow.env`. |
| Cursor / Claude fail over SSH | Run atr on the desktop, not over SSH. |
| `git push` or opening a PR fails | GitHub login is missing in the running service. Never print the token. |

More detail: [HOW-IT-WORKS.md](HOW-IT-WORKS.md) and [GITHUB.md](GITHUB.md).

## Browser and desktop tasks

Tasks that need a browser or a desktop session declare `--needs browser` or
`--needs desktop` (also via MCP, HTTP, inbox files, or GitHub `needs:*`
labels). See [NEEDS.md](NEEDS.md).
