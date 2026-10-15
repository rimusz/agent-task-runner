# atr-operator

One [Agent Plugin](https://agent-plugins.org) that gives a coding CLI **atr MCP** (`atr mcp` over stdio) and the **atr skill**. Cursor and other Agent Plugins clients load this folder. Claude Code, Codex, Grok Build, and OpenCode use the MCP snippets below.

This is not published to any marketplace yet. Install from a local checkout.

`atr` must be on `PATH`. Queued work only dispatches while `atr serve` (or the user service) is running. Tools and remote HTTP details: [MCP tool reference](../../docs/HOW-IT-WORKS.md#mcp-tool-reference).

## Cursor (local Agent Plugin)

Cursor loads Agent Plugins from `~/.cursor/plugins/local/<name>/` when local plugin imports are allowed.

```bash
mkdir -p ~/.cursor/plugins/local
cp -R plugins/atr-operator ~/.cursor/plugins/local/atr-operator
```

Copy the files (Cursor skips a symlink whose target is outside `~/.cursor/plugins/local`). Then restart Cursor or run **Developer: Reload Window**, and check **Customize** for the atr skill and the atr MCP server.

## Skill for other CLIs

This plugin's `skills/atr/SKILL.md` is a copy of the repo skill so the package is self-contained. Keep the two files in sync.

`atr init` / `atr skills install` still writes that skill into each installed CLI's skill dir (`~/.claude`, `~/.cursor`, `~/.codex`, `~/.grok`, `~/.config/opencode`). Use that if you are not loading this Agent Plugin.

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

## Remote HTTP MCP

HTTP is `POST /mcp` on atr's listener and needs a bearer token. atr never prints the token. Do not put the token in chat, a committed file, or this plugin's `mcp.json`.

`atr init` can write `~/.config/atr/mcp.token` (mode `0600`). Point the remote client at `http://<host>:7420/mcp` with header `Authorization: Bearer ${ATR_MCP_TOKEN}` (or your Tailscale hostname). Full adapter notes: [HOW-IT-WORKS.md](../../docs/HOW-IT-WORKS.md#mcp-tool-reference).

## License

MIT OR Apache-2.0, same as atr.
