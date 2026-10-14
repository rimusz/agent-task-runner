# How atr works

Friendly day-to-day steps live in the [README](../README.md). This page is the
technical companion: providers, caching, the driver, MCP tools, and CI.

Architecture and state-machine detail: [DESIGN.md](DESIGN.md).
Config keys and file paths: [CONFIG.md](CONFIG.md).
GitHub tokens and `gh`: [GITHUB.md](GITHUB.md).

## Providers

A `[[provider]]` block can be just `name` + `kind`. CLI path, flags, and
`reported_usage` are filled from the kind. Order in the file is the failover
order.

`atr init` and `atr providers detect` look on `PATH` and common install dirs
(`~/.local/bin`, `~/.claude/local`, `~/.cursor/bin`, `~/.opencode/bin`,
Homebrew, …) for `claude`, `grok`, `cursor-agent`, `codex`, `opencode`, and
the Chinese vendor CLIs (Kimi Code, Qwen Code, ZCode, DeepSeek, MiniMax,
MiMo Code). For each installed binary they print the version and a cheap
signed-in probe (no paid API call). If you are not signed in they print the
exact login command. If the macOS keychain is locked (typical over SSH) they
print `unverified (keychain locked)`. Vendor CLIs are interactive only — atr
has no kinds for them; add the model through OpenCode.

When OpenCode is installed, detect lists signed-in providers and
`opencode models`. Init lets you pick which to add (presets or a
`provider/model` id), including Ollama/spark if they are already in
`opencode.json`. Default order is subscriptions first, then OpenCode.
`atr init --yes` writes the four subscription blocks (enabled only when
detected and signed in) plus those OpenCode picks.

| Provider | CLI | Kind |
|---|---|---|
| Claude Code | `claude` | `claude` |
| Grok Build | `grok` | `grok` |
| Cursor CLI | `cursor-agent` | `cursor` |
| Codex | `codex` | `codex` |
| OpenCode | `opencode` | `opencode` |

OpenCode covers Ollama and the Chinese providers (Kimi/Moonshot, Qwen/DashScope,
DeepSeek, Z.ai GLM, MiniMax, Xiaomi MiMo) plus local spark. **Any model
OpenCode supports works with atr by its `provider/model` id.** Presets
(`atr providers add kimi`) are shortcuts only. Claude, Codex, Cursor, and Grok
stay on their own CLIs.

After each add: `atr doctor`, then a tiny task (`atr serve` must be running):

```bash
atr doctor
atr submit smoke --body "reply with the word ok and do not edit files" --repo ~/src/my-repo
atr tasks smoke
```

### Claude, Grok, Cursor, Codex

| CLI | Sign in | Add |
|---|---|---|
| Claude Code | `claude` on the desktop | `atr providers add claude --kind claude` |
| Grok Build | `grok` | `atr providers add grok --kind grok` |
| Cursor CLI | Cursor app (keychain) | `atr providers add cursor --kind cursor` |
| Codex | `codex login` | `atr providers add codex --kind codex` |

### OpenCode (any provider/model)

1. Install OpenCode ([docs](https://opencode.ai/docs)).
2. Sign in: `opencode auth login` then `opencode auth list`.
3. List models: `opencode models` — copy a `provider/model` id.
4. Add it: `atr providers add my-oc --kind opencode --model moonshotai/kimi-k2.6`

Shortcut: `atr providers add kimi` asks for `MOONSHOT_API_KEY` only (stored in
`~/.config/atr/shadow.env`, never in config) and writes `preset = "kimi"`.

| Name | Sign-in / key | Example model | Shortcut |
|---|---|---|---|
| Kimi (Moonshot) | `opencode auth login` → Moonshot, or `MOONSHOT_API_KEY` | `moonshotai/kimi-k2.6` | `atr providers add kimi` |
| Qwen (DashScope) | `DASHSCOPE_API_KEY` or custom OpenAI-compatible | `qwen/qwen3.5-plus` | `atr providers add qwen` |
| Z.ai GLM | `opencode auth login` → Z.AI | `zai/glm-4.6` | `atr providers add glm` |
| MiniMax | `opencode auth login` → MiniMax | `minimax/minimax-m2.5` | `atr providers add minimax` |
| DeepSeek | `opencode auth login` → DeepSeek | `deepseek/deepseek-chat` | `atr providers add deepseek` |
| Xiaomi MiMo | `MIMO_API_KEY` + custom `opencode.json` | `xiaomi/mimo-v2-flash` | `atr providers add mimo` |
| Ollama (local) | no key; `ollama serve` at `http://127.0.0.1:11434` | `ollama/llama3.2` | `atr providers add ollama` |
| spark (local) | custom OpenCode URL `http://127.0.0.1:8091/v1` | `spark/deepseek-v4-flash-0731` | `atr providers add spark` |

Older Spark-via-Codex: `atr providers add spark --kind codex --override 'model_provider="openai"'`.

`atr providers add ollama` / `atr providers add spark` write the custom
provider into `~/.config/opencode/opencode.json`. Usage for OpenCode, Cursor,
and Codex shows `not reported`.

## Token caching

atr never calls vendor HTTP APIs. Cache hits come from the official CLIs only.
The goal is high prompt-cache hit rates on first runs and resumes.

**What atr does**

| Lever | Behaviour |
|---|---|
| Worktree pool | Reuses `.atr/wt/<provider>-<n>` per repo+provider so cwd/git in the CLI system prompt stay byte-stable. Reset (`checkout -B`, `reset --hard`, `clean -fd`) between different tasks. `[cache] worktree_pool = false` restores one folder per task key. |
| CLI system-prompt slot | Stable prefix (system prompt + `AGENTS.md` + provider contract) goes in a *file*, never argv/`ps`: Claude `--append-system-prompt-file`, Grok `--rules`, Codex `instructions_file`, OpenCode `--system`. Cursor has no slot; the prefix stays in the prompt file. |
| Resume delta | Follow-ups send only the new messages (and optional handoff). The stable prefix is not re-sent. |
| Provider affinity | Retries and follow-ups stay on the same provider and session. Work still inside `cache_window` (Claude ~5m, others 60m) is scheduled first; remaining queued tasks for that provider are batched next. |
| Ollama | OpenCode `keepAlive` default `24h`. At most one inflight task per model. |
| Metrics | Headline hit% is only runs with ≥ 40k prompt tokens (`[cache] headline_min_input`). First-run and resume rates are shown separately. Short runs are listed but excluded from the headline. Same split in `atr usage`, `atr status`, and the dashboard. |

**What you control**

| Setting | Where |
|---|---|
| `[cache] worktree_pool` / `headline_min_input` | `config.toml` |
| `[[provider]] cache_window` | How long atr treats a session as warm (Claude default `5m`) |
| `[[provider]] system_prompt_flag` | Override the file flag if your CLI build differs |
| `[[provider]] keep_alive` | OpenCode/Ollama `keepAlive` (default `24h`) |
| vLLM prefix caching | On the vLLM server: enable prefix caching and `--enable-prompt-tokens-details` so OpenCode/Codex can report cache reads |
| Ollama `keep_alive` | atr writes `keepAlive` into `~/.config/opencode/opencode.json`; you can also set it in Ollama itself |

`atr doctor --cache` checks that two rendered tasks share a byte-identical prefix.

See [DESIGN.md](DESIGN.md) §5 for the prefix contract and hit-rate math.

## Driver, planner, and review

`driver` is the provider used for **planning** and **review**. Empty means Grok
if that provider is enabled, otherwise the first enabled provider. `atr init`
asks which one to use. If the driver is missing, signed out, or limited, atr
falls back to the next provider using the same error classes as worker failover.

Planning and review work with any provider. Grok-only extras (plugin,
`grok usage`, session fork) switch off when the driver is not Grok.

### Planner

The driver reads the issue and the repo **read-only** and returns:

```json
{"subtasks":[{"title":"…","prompt":"…","provider_hint":"grok","deps":[]}]}
```

Parent/child links are stored on the task and shown in `atr status`, the
dashboard, and Telegram `/task`. Default `planner.mode = "auto"` only splits
when the issue is long (`auto_chars`, default 4000). Set `mode = "off"` to
disable, or `always` to split every new issue.

Grok plan command (verified on 1.0.52): `--max-turns N --output-format json
--json-schema … --permission-mode plan`. Other providers use their
structured/JSON output, or a fenced JSON block.

### Review

When `[review] enabled = true`, the driver reviews the worker diff after a
successful run (and passing verify). Verdict is `approve` or `request_changes`.
Requested changes go back to the original worker session. `max_rounds`
(default 2) caps the loop. Grok uses the bundled `/review` skill under `-p`
with at least 15 turns.

### Grok extras (1.0.52+)

| Extra | What |
|---|---|
| `--max-turns` | Set on every Grok run. Planner is read-only (`--permission-mode plan`). |
| `plugin = true` on the grok provider | Uses `grok agent --plugin-dir <atr-plugin> --always-approve stdio` (JSON-RPC). `-p` rejects `--plugin-dir`. The plugin lives under atr's data dir — **never `~/.grok`**. Hooks POST `PostToolUse`, `SubagentStart`/`Stop`, `Stop`, and `StopFailure` to `http://<listen>/hooks` for live dashboard/Telegram progress. Optional `[plugin] test_command` makes the Stop hook wait until that command passes. The atr skill is bundled. Custom `--agents` are not used. |
| Checkpoint retry | Failed verify/stall/review retries fork: `-r <sid> --fork-session -s <new uuid>` so the original session is kept. |
| `grok usage <sid>` | After each run, session `inputTokens` (includes cache reads), `cachedReadTokens`, `cacheCreationTokens`, `outputTokens`, `costUsdTicks` feed the dashboard. |
| Streaming events | `text`, `thought`, `tool_call`, `tool_call_update`, `usage`, `end`, `SubagentCompleted` are stored as the task timeline. |

`atr doctor` requires Grok **>= 1.0.52**. Detect uses the auth file or billing
stdio — not `grok auth status` (that command does not exist) and never
`grok login`.

## MCP tool reference

Agents can drive atr over MCP. The [README](../README.md#using-atr-from-other-agents)
covers the idea; this page has the adapters and the tool list.

**Stdio** — `atr mcp`. atr writes only JSON-RPC to stdout.

**Claude Code** (`~/.claude.json` or the desktop MCP config), **Cursor**
(Settings → MCP, or `~/.cursor/mcp.json`), and **Grok Build**:

```json
{
  "mcpServers": {
    "atr": {
      "command": "atr",
      "args": ["mcp"]
    }
  }
}
```

**Codex** (`~/.codex/config.toml`):

```toml
[mcp_servers.atr]
command = "atr"
args = ["mcp"]
```

**Streamable HTTP** — `POST /mcp` on the existing listener. HTTP requires a
bearer token. `mcp.token_env` names the env var (default `ATR_MCP_TOKEN`).
atr never prints the token. `atr init` can generate one and write
`~/.config/atr/mcp.token` (mode `0600`). Serve also reads that file when the
env var is empty.

```toml
[mcp]
enabled = true
token_env = "ATR_MCP_TOKEN"
token_file = "~/.config/atr/mcp.token"
```

Tools: `add_task`, `list_tasks`, `task_status` (timeline + last output),
`steer_task`, `cancel_task`, `list_repos`, `list_providers` (with usage),
`usage_summary`, `doctor`, and read-only `config_get` (secrets redacted).
Config cannot be changed over MCP. Resources: `atr://status`, `atr://dashboard`.

`atr serve` must be running for queued work to dispatch. `atr mcp` stdio talks
to the same database; it does not start the scheduler.

**Generic HTTP client** (Grok Bot, remote Cursor, curl). Do not put the token
in a chat or a committed file:

```json
{
  "mcpServers": {
    "atr": {
      "url": "http://127.0.0.1:7420/mcp",
      "headers": {
        "Authorization": "Bearer ${ATR_MCP_TOKEN}"
      }
    }
  }
}
```

```bash
export ATR_MCP_TOKEN="$(tr -d '\n' < ~/.config/atr/mcp.token)"
curl -sS http://127.0.0.1:7420/mcp \
  -H "Authorization: Bearer $ATR_MCP_TOKEN" \
  -H "Accept: application/json, text/event-stream" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

**GitHub-label method** (no MCP). Label an issue `for:atr` (or your
`[github].label` / `[[repo]] label`). The poller enqueues it. Comments become
follow-ups. Use this when the agent lives on GitHub; use MCP when another
local or Tailscale agent should submit and steer.

## CI usage

atr itself does **not** run in GitHub Actions. It runs on your machine (or a
Mini). CI can only label issues `for:atr` so the local poller picks them up.

```yaml
# .github/workflows/label-for-atr.yml
# Does not install or run atr. The local `atr serve` watches this label.
name: label for atr
on:
  issues:
    types: [opened]
jobs:
  label:
    if: contains(github.event.issue.title, '[atr]')
    runs-on: ubuntu-latest
    permissions:
      issues: write
    steps:
      - uses: actions/github-script@v7
        with:
          script: |
            await github.rest.issues.addLabels({
              owner: context.repo.owner,
              repo: context.repo.repo,
              issue_number: context.issue.number,
              labels: ['for:atr'],
            })
```

Or label by hand: `gh issue edit 12 --add-label for:atr`. On the machine that
runs atr, `[github] enabled = true` and `repo` must be set.
