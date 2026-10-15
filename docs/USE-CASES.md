# Use cases

atr is one program with many ways in. Pick the uses that fit you; they all run the same kind of task on the same machine.

| Use case | What you need to set up |
|---|---|
| [1. Background runner on your laptop](#1-background-runner-on-your-laptop) | atr + `atr init` (installs the user service), at least one signed-in coding CLI |
| [2. Always-on Linux server or cloud VM](#2-always-on-linux-server-or-cloud-vm) | Same as 1, plus `loginctl enable-linger "$USER"` |
| [3. GitHub issue worker](#3-github-issue-worker) | GitHub token (`gh auth login` or `GH_TOKEN`), repos added with `atr repos add` |
| [4. Backend for AI agents (MCP)](#4-backend-for-ai-agents-mcp) | `atr mcp` in your agent's MCP config; for remote use, Tailscale + an MCP token |
| [5. Scripts and automations](#5-scripts-and-automations) | `curl` to `POST /tasks`, or the inbox folder turned on in config |
| [6. Plain folders and quick questions](#6-plain-folders-and-quick-questions) | Nothing extra: `atr add` with `--dir` or no workspace |
| [7. Control from your phone](#7-control-from-your-phone) | `atr telegram setup`; for the dashboard off-machine, a Tailscale bind |
| [8. Several AI providers with fallback](#8-several-ai-providers-with-fallback) | Two or more signed-in CLIs, ordered with `atr providers move` |

### 1. Background runner on your laptop

Run atr quietly on your own Mac or Linux laptop. Queue work, close the terminal, and it keeps going in the background using the CLIs you already pay for.

```bash
atr init          # writes config and installs the user service
atr add "fix the typo in notes.txt" --dir ~/notes
atr status
```

### 2. Always-on Linux server or cloud VM

Put atr on a headless Linux box or cloud VM so it works around the clock, even when nobody is logged in. Sign in to the coding CLIs on that server, then keep the service alive after logout.

```bash
atr init --yes
loginctl enable-linger "$USER"
atr doctor
```

### 3. GitHub issue worker

Label an issue `for:atr` and atr picks it up, does the work, and opens a pull request. Comments on the issue become follow-ups. One install can watch several repos.

```bash
atr repos add owner/website --path ~/src/website
atr repos add owner/api --path ~/src/api
gh issue edit 12 --add-label for:atr
```

### 4. Backend for AI agents (MCP)

Let Claude Code, Codex, Cursor, Grok Build, Grok Bot or OpenCode hand work to atr and get the answer back. On the same machine they run `atr mcp`; from another machine they connect over Tailscale with a token. They add work with `add_task`, wait with `wait_task`, and fetch the answer, diff or PR link with `get_result`.

```json
{
  "mcpServers": {
    "atr": { "command": "atr", "args": ["mcp"] }
  }
}
```

Remote setup with a token: [HOW-IT-WORKS.md](HOW-IT-WORKS.md#mcp-tool-reference).

### 5. Scripts and automations

Any script, cron job or tool can queue work: send an HTTP request, or drop a small task file into the inbox folder. See [Hand work to atr](USAGE.md#hand-work-to-atr) for the inbox setup.

```bash
curl -s -X POST http://127.0.0.1:7420/tasks \
  -H 'Content-Type: application/json' \
  -d '{"title":"Sum","body":"what is 2+2?","workspace":"none","output":"text"}'
```

```toml
# ~/.local/share/atr/inbox/notes.toml
prompt = "Fix the spelling mistakes in notes.txt."
dir = "~/notes"
```

### 6. Plain folders and quick questions

Not everything is a git repo. Point atr at any folder and it edits files there (and only there), or give no folder at all and it just answers.

```bash
atr add "tidy up the headings in these markdown files" --dir ~/docs
atr add "explain what a race condition is"
```

### 7. Control from your phone

Connect a Telegram bot to queue work and check progress from anywhere. The read-only dashboard at `http://127.0.0.1:7420/` shows what is running, queued and done; reach it from your phone over Tailscale (see [CONFIG.md](CONFIG.md#dashboard)).

```bash
atr telegram setup
```

```
/add what is 2+2?
/status
/tasks
```

### 8. Several AI providers with fallback

Use several subscriptions side by side, for example Claude, Grok, Cursor, Codex and OpenCode. When one runs out or hits its limit, atr moves to the next one in your order automatically.

OpenCode is a supported coding CLI too, and it opens the door to many more AI providers: DeepSeek, Kimi, Qwen, GLM, MiniMax, local Ollama, a self-hosted vLLM server, and any other model OpenCode supports. See [Providers and OpenCode](USAGE.md#providers-and-opencode).

```bash
atr providers detect
atr providers add deepseek
atr providers move claude --to 1
atr providers enable cursor
atr usage --period 7d
```
