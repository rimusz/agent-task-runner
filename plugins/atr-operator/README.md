# atr-operator

One [Agent Plugin](https://agent-plugins.org) that gives a coding CLI **atr MCP** and the **atr skill**. Cursor and other Agent Plugins clients load this folder. Claude Code, Codex, Grok Build, Gemini CLI, and OpenCode use the MCP snippets below.

`mcp.json` declares two servers and holds only `${ATR_MCP_HOST}` and `${ATR_MCP_TOKEN}` placeholders, never a real token. Do not put the token in chat or in a committed file.

Queued work only dispatches while `atr serve` (or the user service) is running. Tools: [MCP tool reference](../../docs/HOW-IT-WORKS.md#mcp-tool-reference).

## Local mode

`atr` must be on `PATH`. Use the stdio server `atr`. Turn off `atr-remote` if it is not configured.

## Remote mode

On a machine without the `atr` binary, set `ATR_MCP_HOST` and `ATR_MCP_TOKEN` under **Plugins → Configure**. Turn off the stdio server `atr`; it fails without the binary.

The remote URL is `https://<host>/mcp` with header `Authorization: Bearer <token>`. Host setup, the token file, and per-CLI HTTP snippets: [REMOTE-ACCESS.md](../../docs/REMOTE-ACCESS.md).

## Variables

Cursor substitutes these from the plugin Configure form. `mcp.json` contains the placeholders only.

| Variable | Secret | What it is |
|----------|--------|------------|
| `ATR_MCP_HOST` | no | Hostname that serves `/mcp` over HTTPS. No `https://` and no `/mcp`. Add `:port` only when HTTPS is not on 443. Example: `ai-stack.tailxxxx.ts.net`. |
| `ATR_MCP_TOKEN` | yes (`writeOnly`, `format: password`) | Bearer token from `~/.config/atr/mcp.token` (or `ATR_MCP_TOKEN`) on the atr host. Cursor stores it and does not show it back. |

Other Agent Plugins clients do not substitute these `${VAR}`s. Use stdio, or the per-CLI remote snippets in [REMOTE-ACCESS.md](../../docs/REMOTE-ACCESS.md).

## Team-wide

Cursor team plugin variables are shared. The token can add and cancel tasks. A `ts.net` URL only works on that tailnet.

## Cursor (local Agent Plugin)

Cursor loads Agent Plugins from `~/.cursor/plugins/local/<name>/` when local plugin imports are allowed.

```bash
mkdir -p ~/.cursor/plugins/local
cp -R plugins/atr-operator ~/.cursor/plugins/local/atr-operator
```

Copy the files (Cursor skips a symlink whose target is outside `~/.cursor/plugins/local`). Then restart Cursor or run **Developer: Reload Window**, and check **Customize** for the atr skill and the atr MCP servers.

The repo-root `.cursor-plugin/marketplace.json` points at this nested plugin (`plugins/atr-operator`).

## Skill for other CLIs

This plugin's `skills/atr/SKILL.md` is a copy of the repo skill so the package is self-contained. Keep the two files in sync.

`atr init` / `atr skills install` still writes that skill into each installed CLI's skill dir (`~/.claude`, `~/.cursor`, `~/.codex`, `~/.grok`, `~/.gemini`, `~/.config/opencode`). Use that if you are not loading this Agent Plugin.

## MCP snippets (same machine)

These register stdio `atr mcp`. They contain no tokens.

### Claude Code

```bash
claude mcp add --transport stdio atr -- atr mcp
```

Or add this to `~/.claude.json` (or the desktop MCP config):

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

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.atr]
command = "atr"
args = ["mcp"]
```

### Grok Build

Grok Build uses the same `mcpServers` JSON as Claude Code / Cursor:

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

### OpenCode

`~/.config/opencode/opencode.json` (or a project `opencode.json`). OpenCode uses `mcp`, a `type`, and `command` as one array:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "atr": {
      "type": "local",
      "command": ["atr", "mcp"],
      "enabled": true
    }
  }
}
```

## License

MIT OR Apache-2.0, same as atr.
