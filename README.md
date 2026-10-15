# atr

**atr runs your coding agents in the background, using the CLIs you already have.**

- Hand it a task (a prompt, plus a folder or repo if you like) and it runs it with Claude Code, Grok Build, Cursor, Codex or OpenCode.
- When one provider hits its limit, it moves on to the next one in your list.
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
5. Run atr service install. On headless Linux also: loginctl enable-linger "$USER".
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

Later, `atr upgrade` updates it in place and restarts the service (`atr rollback` undoes it).

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

## What you can use it for

- **Background runner on your Mac**: queue work and close the terminal. [More](docs/USE-CASES.md#1-background-runner-on-your-laptop)
- **Always-on Linux box or VM**: runs around the clock. [More](docs/USE-CASES.md#2-always-on-linux-server-or-cloud-vm)
- **GitHub issue worker**: label an issue, get a pull request. [More](docs/USE-CASES.md#3-github-issue-worker)
- **Backend for other agents**: Claude, Codex, Cursor or Grok hand work to atr over MCP. [More](docs/USE-CASES.md#4-backend-for-ai-agents-mcp)
- **Inbox folder**: scripts drop a task file and atr runs it. [More](docs/USAGE.md#hand-work-to-atr)
- **Telegram**: add tasks and check progress from your phone. [More](docs/USE-CASES.md#7-control-from-your-phone)

## Providers

Claude Code, Grok Build, Cursor, Codex and OpenCode. OpenCode also opens the door to DeepSeek, Kimi, Qwen, local Ollama, self-hosted vLLM and more. See [docs/USAGE.md](docs/USAGE.md#providers-and-opencode).

## More docs

| Page | What it covers |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | Manual install, Arch/Asahi, full agent deploy prompt |
| [docs/USE-CASES.md](docs/USE-CASES.md) | Each use case with examples |
| [docs/USAGE.md](docs/USAGE.md) | Adding work, watching repos, Telegram, dashboard, providers, upkeep, troubleshooting |
| [docs/GITHUB.md](docs/GITHUB.md) | GitHub token, SSH, what needs what |
| [docs/CONFIG.md](docs/CONFIG.md) | Config file, paths, several repos, dashboard bind |
| [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) | How providers, MCP and CI fit together |
| [docs/DESIGN.md](docs/DESIGN.md) | Internals, for people changing atr |

## Trademarks and license

atr is not affiliated with or endorsed by Anthropic, xAI, Anysphere, OpenAI or the OpenCode authors. Claude, Grok, Cursor, Codex, ChatGPT and OpenCode are trademarks of their owners. Use atr with **your own** CLI accounts and follow each provider's terms. Do not resell access or share accounts.

Licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). Dependency licenses: `THIRD_PARTY_LICENSES`.
