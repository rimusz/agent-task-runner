# atr

**Your AI subscriptions are idle 16 hours a day. atr turns them into a 24/7 dev team.**

- Hand it a task (a prompt, plus a folder or repo if you like) and it runs it with Claude Code, Grok Build, Cursor, Codex or OpenCode.
- When one provider hits its limit, it moves on to the next one in your list.
- Retries transient provider errors (overloaded, 429, 5xx) with backoff, waits on `after:` task dependencies, and keeps new work queued when the host CPU or memory is busy.
- Takes work from the terminal, GitHub issues, other agents (MCP), Telegram, HTTP or a drop folder.
- Shows what is running on a local dashboard, in the terminal or in Telegram.

atr never calls vendor APIs or handles your API keys. You sign in to each CLI yourself.

## Install

One line, on macOS (Apple Silicon) or Linux (x86_64 or arm64):

```bash
curl -fsSL https://raw.githubusercontent.com/rimusz/agent-task-runner/main/install.sh | sh
```

It picks the right file for your machine, checks its checksum and puts `atr` in `~/.local/bin`.

<details>
<summary><b>Or let your coding agent install and set it up</b> (paste into Claude Code, Codex, Cursor or Grok)</summary>

```
Install atr (https://github.com/rimusz/agent-task-runner) on this machine.
Never print, log or paste any secret, token or API key. Never sign in to anything for me.

1. Install: curl -fsSL https://raw.githubusercontent.com/rimusz/agent-task-runner/main/install.sh | sh
   Make sure ~/.local/bin is on PATH, then check: atr --version
2. Run: atr providers detect
   For each CLI that is installed but signed out, tell me its login command and wait for me.
3. Check gh auth status. If not logged in, ask me to run gh auth login, then run gh auth setup-git.
4. Ask whether I want to answer the setup questions myself or take defaults, then run one of:
   atr init
   atr init --yes
5. Run atr service install. On headless Linux also: sudo loginctl enable-linger "$USER".
6. Ask which GitHub repos to watch and add each: atr repos add <owner/name> --path <clone>
7. Run atr doctor and report every warn or FAIL line with a suggested fix.
```

Full version, including MCP setup: [docs/INSTALL.md](docs/INSTALL.md#full-agent-deploy-prompt).
</details>

<details>
<summary>Or download it yourself</summary>

Pick your platform: `macos-arm64`, `linux-x86_64`, `linux-arm64` or `linux-x86_64-musl` (Alpine).

```bash
ver=0.1.7 platform=macos-arm64
curl -fsSL "https://github.com/rimusz/agent-task-runner/releases/download/v$ver/atr-$ver-$platform.tar.gz" | tar -xz -C /tmp
mkdir -p ~/.local/bin && install -m 755 /tmp/atr ~/.local/bin/atr
```

No Intel Mac build: use `cargo install --git https://github.com/rimusz/agent-task-runner`.
Arch, Omarchy and Asahi notes: [docs/INSTALL.md](docs/INSTALL.md).
</details>

Later, `atr upgrade` updates it in place, refreshes the atr skill so it matches the new binary, and restarts the service (it does nothing if already on the latest release; `--force` reinstalls) (`atr rollback` undoes it and refreshes the skill from the restored binary). `atr reset` removes atr's service, config, data and skills to start fresh (see [USAGE.md](docs/USAGE.md#maintenance)).

## Setup

**1. Sign in to your coding CLIs.** Use the ones you have:

```bash
claude                 # Claude Code
grok                   # Grok Build
codex login            # Codex
opencode auth login    # OpenCode
# Cursor: sign in to the Cursor app (on a Mac, at the desktop, not over SSH)
```

**2. Give it GitHub access.** You need a GitHub token: `gh auth login`, then `gh auth setup-git`. An SSH key is optional. Details: [docs/GITHUB.md](docs/GITHUB.md).

**3. Set it up and start it.**

```bash
atr init               # asks a few questions; or: atr init --yes
atr service install    # runs atr in the background (init usually does this)
```

Or run `atr` / `atr tui` / `atr ask` on a TTY: if there is no config yet, atr offers this same `atr init` (including whether to install the user service) and then continues. Scripts should call `atr init --yes` first.

**4. Check it.**

```bash
atr doctor             # shows anything that needs fixing
atr ui                 # opens the dashboard at http://127.0.0.1:7420/
```

## Your first task

Ask it to do something in a folder:

```bash
atr add "fix the spelling mistakes in README.md" --dir .
atr status
```

Or let it work on GitHub issues. Add the repo, then label an issue `for:atr`. atr does the work and opens a pull request:

```bash
atr repos add owner/my-repo --path ~/src/my-repo
gh issue edit 12 --add-label for:atr
```

## Interactive TUI

A bare `atr` (same as `atr tui`) opens a chat in your terminal. It behaves like the provider CLI (Claude Code, Grok Build, Cursor, Codex or OpenCode) with atr in the middle as a proxy: atr keeps the session, reuses the prompt cache and shows tokens and cache use under the input. atr never writes the replies itself.

```bash
atr                      # chat with no workspace (answer-only)
atr --repo .             # work in the current directory's git repo
atr --repo demo          # a repo from your config, by name or path
atr tui --dir ~/notes    # edit a plain folder in place
atr ask "what is 2+2?"   # one turn, print the answer, exit (scripts, CI)
```

| In the chat | What it does |
|---|---|
| `/` | Opens the atr command menu |
| `/provider`, `/model` | Pick the provider, or that provider's model, for this session |
| `/effort`, `/context` | Set reasoning effort or context size for the next turn, where that CLI has a flag for it |
| `/repo` | Switch workspace: a repo from your config, `/repo .` (current git repo) or `/repo none` |
| `/new`, `/status`, `/help` | Fresh session, session status, help |
| `/cli` | Open the provider's own TUI |
| `/quit` or Ctrl+C | Leave |

Any other `/…` line (for example `/compact`) goes to the provider as a turn. Press Enter while a turn is running and the line is queued; atr sends queued lines in order once the turn finishes. Full reference: [docs/USAGE.md](docs/USAGE.md#terminal-tui-atr-tui).

## Ways to run atr

| Mode | Use it for | Start it |
|---|---|---|
| TUI only | Chatting with your CLIs on a laptop, no background service | `atr` |
| TUI + serve | A machine that runs background work and that you also chat on | `atr service install`, then `atr` |
| serve only | A headless runner (Mac Mini, Linux box, VM) | `atr init --yes`, `atr service install` |

**TUI only.** The TUI runs atr's engine in its own process, so `atr serve` is not needed. It only runs its own chat session. Nothing polls GitHub, Telegram or the inbox, the dashboard, HTTP API and MCP over HTTP are off, and tasks from `atr add` wait in the queue until serve runs. The first `atr` on a terminal offers `atr init` if there is no config yet.

**TUI + serve.** Serve runs as the user service and you open `atr` when you want to chat. The TUI does not connect to serve. Both read the same config file and state database (by default `~/.config/atr/config.toml` and `~/.local/share/atr/state.db`; pass the same `--config` or `ATR_CONFIG` to both if you use another one). So:

- `/repo` lists the repos in that config (`[[repo]]` or `repo_path`, added with `atr repos add`) and the status line marks a picked repo `(watched)` when it is enabled there. `/repo` rereads the config each time you open it, and works whether or not serve is running.
- TUI sessions are tasks named `cli-<name>`. Serve never starts them, and the TUI only starts its own session, so they do not run the same task twice. Each process stamps its pid and start time on the runs it owns in `state.db`. Restarting serve (`atr service restart`, `atr upgrade`) leaves a live TUI turn alone; a run whose owner process is gone is still reaped.
- Provider cooldowns and quota limits live in the shared database. `max_parallel` is counted across every live owner (TUI turns use the same slots as serve). A TUI turn waits and the status line shows `provider at limit` rather than going past the cap.
- The status line under the input shows `· serve` when a live serve is registered in `state.db` or the serve pidfile is still valid. The launchd/systemd user service check is a fallback.

**serve only.** `atr serve` (usually the user service) runs the scheduler, the HTTP API with the dashboard and MCP, and the GitHub, Telegram, inbox and shadow pollers. Nobody needs to be at the keyboard. On headless Linux, also run `sudo loginctl enable-linger "$USER"`. The TUI cannot reach a serve on another machine; to chat with that machine's setup, SSH in and run `atr` there. See [docs/USE-CASES.md](docs/USE-CASES.md#2-always-on-linux-server-or-cloud-vm) and [Sources](docs/CONFIG.md#sources).

## What you can use it for

- **Background runner on your Mac**: queue work and close the terminal. [More](docs/USE-CASES.md#1-background-runner-on-your-laptop)
- **Always-on Linux box or VM**: runs around the clock. [More](docs/USE-CASES.md#2-always-on-linux-server-or-cloud-vm)
- **GitHub issue worker**: label an issue, get a pull request. [More](docs/USE-CASES.md#3-github-issue-worker)
- **Backend for other agents**: Claude, Codex, Cursor or Grok hand work to atr over MCP. [More](docs/USE-CASES.md#4-backend-for-ai-agents-mcp)
- **Grok Bot operator**: import the [atr Operator](https://x.ai/bot/oDG33vy41WVZKy-HFKVk0) bot template to queue work and watch atr from Grok Bot.
- **Inbox folder**: scripts drop a task file and atr runs it. [More](docs/USAGE.md#hand-work-to-atr)
- **Telegram**: add tasks and check progress from your phone. [More](docs/USE-CASES.md#7-control-from-your-phone)
- **Terminal TUI**: chat with your coding CLIs in the terminal, with atr as the proxy. [More](#interactive-tui)

## Providers

Claude Code, Grok Build, Cursor, Codex and OpenCode. OpenCode also opens the door to DeepSeek, Kimi, Qwen, local Ollama, self-hosted vLLM and more. See [docs/USAGE.md](docs/USAGE.md#providers-and-opencode).

Each CLI gets atr's instructions in its own system-prompt slot. For Codex that is `-c model_instructions_file`, which replaces Codex's built-in base prompt with atr's (details in [docs/CONFIG.md](docs/CONFIG.md#cache)).

### Local models (OpenCode 2.x + Ollama)

atr can run worker tasks on a model on your own machine through OpenCode and Ollama. Tested setup: OpenCode 2.0.28, Ollama with `qwen3-coder:30b` (30B-A3B, 32k context), Apple M4 Pro with 48 GB.

```bash
ollama pull qwen3-coder:30b && ollama serve          # http://127.0.0.1:11434
atr providers add ollama --model ollama/qwen3-coder:30b --url http://127.0.0.1:11434/v1
                                                      # also writes the provider into ~/.config/opencode/opencode.json
atr doctor                                            # checks the CLI, the URL and the model
```

- **The model has no shell.** Every OpenCode run gets an `OPENCODE_CONFIG_CONTENT` deny list, loaded last so it beats your `opencode.json`. It denies `shell`, `execute`, `subagent`, `question`, `webfetch`, `websearch`, `external_directory` and `skill`. The model only reads and edits files in its worktree.
  - atr runs your `[verify]` commands, commits, pushes, opens the PR and posts comments itself.
  - Because the model can't run tests, it gets one extra verify follow-up (`[verify] no_shell_extra_retries`).
  - `allow_shell = true` on the provider gives the shell back, but only for a model you trust.
- **Extra guards for no-shell models only.** These don't apply to Claude, Codex, Cursor or Grok:
  - new scratch files and unrequested README/docs edits are dropped (`[scratch]`);
  - an edit to the main checkout withholds the result;
  - `atr retry` starts a fresh session.
- **Triage is suggest-only.** `atr triage run` stores a label and comment suggestion. Nothing is posted until `atr triage approve`.
- **Hardware.** `qwen3-coder:30b` is an 18.6 GB download and needs about 20 GB of free RAM or VRAM with a 32k context, so 32 GB or more is comfortable. Give the model a context limit of at least 32k in `opencode.json` (`provider.ollama.models.<id>.limit.context`). atr runs one task at a time per Ollama model and keeps the model loaded (`keepAlive` 24h).
- **Results so far.** On the 10 sandbox issues the local model fixed 8 out of 10 at first, and 9 out of 10 after the follow-up hardening. The misses failed verify and were not delivered.
- **Known limits.**
  - Small-model quality varies from run to run.
  - Triage label accuracy is weak.
  - The model sometimes still writes files during triage; they are dropped, never verified or posted.
  - Docs revert only covers docs-style files.

Setup, config and safety details: [docs/USAGE.md](docs/USAGE.md#providers-and-opencode), [docs/CONFIG.md](docs/CONFIG.md#scratch-files).

**Why Grok Build:** it's the recommended main provider. atr uses it by default to plan and review, and it adds live progress on the dashboard and Telegram, per-run cost and token data, and session resume. Everything else works with any provider. See [docs/USAGE.md](docs/USAGE.md#grok-build-as-the-main-provider).

## More docs

| Page | What it covers |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | Manual install, Arch/Asahi, full agent deploy prompt |
| [docs/USE-CASES.md](docs/USE-CASES.md) | Each use case with examples |
| [docs/USAGE.md](docs/USAGE.md) | Adding work, watching repos, Telegram (and the CLI equivalent of every Telegram command), dashboard, providers, upkeep, troubleshooting |
| [docs/GITHUB.md](docs/GITHUB.md) | GitHub token, SSH, what needs what |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | `atr doctor` checks, status, logs, service, GitHub token, Telegram, notify hook |
| [docs/REMOTE-ACCESS.md](docs/REMOTE-ACCESS.md) | Dashboard and MCP from other devices over Tailscale |
| [docs/CONFIG.md](docs/CONFIG.md) | Config file, paths, several repos, dashboard bind, [choosing the driver provider and model](docs/CONFIG.md#choosing-the-driver-provider-and-model) |
| [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) | How providers, MCP and CI fit together |
| [docs/DESIGN.md](docs/DESIGN.md) | Internals, for people changing atr |
| [docs/RELEASING.md](docs/RELEASING.md) | How to cut a release (on demand only) |

## Trademarks and license

atr is not affiliated with or endorsed by Anthropic, xAI, Anysphere, OpenAI or the OpenCode authors. Claude, Grok, Cursor, Codex, ChatGPT and OpenCode are trademarks of their owners. Use atr with **your own** CLI accounts and follow each provider's terms. Do not resell access or share accounts.

Licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). Dependency licenses: `THIRD_PARTY_LICENSES`.
