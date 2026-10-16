# Remote access over Tailscale

Reach atr's dashboard and its HTTP MCP endpoint from another Mac, an iPhone or a remote agent, using only your own Tailscale network (tailnet). Nothing here exposes atr to the internet.

## What is reachable, and what protects it

atr serves one HTTP listener (`listen`, default `127.0.0.1:7420`):

| Path | Auth |
|---|---|
| `/`, `/dashboard.json`, `/health` (dashboard) | **None** |
| `/status`, `/tasks`, `/tasks/<key>`, `/tasks/<key>/result`, `/tasks/<key>/messages`, `/tasks/<key>/cancel`, `/hooks` | **None** |
| `POST /mcp` | Bearer token (see [MCP over HTTP](#mcp-over-http)) |

The dashboard has no login of its own. The `/mcp` token protects only `/mcp`. Anyone who can reach the listener can view the dashboard and, on the main listener, use the HTTP task API (add, steer and cancel tasks). Your protection for everything except `/mcp` is that only your tailnet can reach it: keep the tailnet to your own devices, or restrict the host with [Tailscale ACLs](https://tailscale.com/kb/1018/acls). Secrets (Telegram and GitHub tokens) are redacted from the dashboard data.

## Recommended: Tailscale Serve

atr keeps listening on loopback only, and Tailscale proxies a tailnet-only HTTPS address to it.

Prerequisites, in the [Tailscale admin console](https://login.tailscale.com/admin/dns):

- **MagicDNS** enabled.
- **HTTPS Certificates** enabled.

Keep the default listener in `~/.config/atr/config.toml`:

```toml
listen = "127.0.0.1:7420"
```

On the atr host:

```bash
tailscale serve --bg 7420
```

On macOS with the Tailscale app, the CLI is `/Applications/Tailscale.app/Contents/MacOS/Tailscale` (use it in place of `tailscale`). `--bg` keeps the setting after the command exits and across reboots.

The dashboard is then at `https://<host>.<tailnet>.ts.net/` (for example `https://ai-stack.tailxxxx.ts.net/`). The very first request can take a few seconds while Tailscale issues the certificate. To open it from an iPhone or another Mac, the Tailscale app must be on and signed in to the same tailnet.

Check and turn off:

```bash
tailscale serve status            # shows the https://… URL and the 127.0.0.1:7420 target
tailscale serve --https=443 off   # stop serving
```

`atr ui --print` still prints the local `http://127.0.0.1:7420/` address; it does not know about Serve.

### Never use Funnel

Do not run `tailscale funnel`, and do not bind atr to `0.0.0.0` on a machine with a public IP, port-forward 7420, or put it behind a public reverse proxy. The dashboard and task API have no auth; public exposure gives anyone control of your agents.

## Alternative: bind to the Tailscale IP

The older approach from [CONFIG.md](CONFIG.md#dashboard): listen on the host's Tailscale IP (`tailscale ip -4`).

```toml
listen = "<tailscale-ip>:7420"

[dashboard]
enabled = true
bind = "<tailscale-ip>"
```

Or keep `listen` on loopback and set only `[dashboard] bind = "<tailscale-ip>"`: atr then adds a second, read-only listener on `<tailscale-ip>:7420` serving just `/`, `/dashboard.json` and `/health`, so task submit/cancel and `/mcp` stay on loopback.

Restart with `atr service restart`. Trade-offs versus Serve:

- Plain HTTP, not HTTPS (traffic is still WireGuard-encrypted inside the tailnet, but browsers mark it insecure).
- The Tailscale IP is stable in practice but can change (re-auth, new node), and then the config is wrong and atr cannot bind.
- With `listen` on the Tailscale IP, the full API and `/mcp` are on the tailnet, and the host itself must use that IP instead of `127.0.0.1`.

## MCP over HTTP

Local agents on the atr host use stdio (`atr mcp`) and need no token. Remote agents use `POST /mcp` with a bearer token.

### 1. Create the token

```bash
mkdir -p ~/.config/atr
umask 077; openssl rand -hex 32 > ~/.config/atr/mcp.token
```

The file must be mode 0600. `atr init` can create it for you. Alternatively put `ATR_MCP_TOKEN=<value>` in `~/.config/atr/shadow.env` (0600). atr reads the env var named by `token_env` first, then `token_file`.

### 2. Enable `/mcp`

```toml
[mcp]
enabled = true
token_env = "ATR_MCP_TOKEN"
token_file = "~/.config/atr/mcp.token"
```

```bash
atr service restart
atr doctor
```

`atr doctor` should show `ok mcp token`. If `/mcp` is enabled with no token, doctor shows a `warn` (`HTTP /mcp enabled, no token`) and `/mcp` answers 503 `mcp token not configured` to every request. atr never prints the token.

### 3. Verify

```bash
URL=https://<host>.<tailnet>.ts.net/mcp

# No token: expect 401
curl -s -o /dev/null -w '%{http_code}\n' -X POST "$URL" \
  -H 'Content-Type: application/json' -d '{}'

# With token: expect 200 and a tools list
curl -s -X POST "$URL" \
  -H "Authorization: Bearer $ATR_MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Status codes: 401 missing or wrong token, 403 a browser `Origin` that is not loopback or the `listen` host, 404 `[mcp] enabled = false`, 405 `GET /mcp`, 503 no token configured. Non-browser MCP clients send no `Origin`, so the origin check does not affect them.

### 4. Client config

On the client, export the token from a secret store (`export ATR_MCP_TOKEN=…`), never hard-code it. Replace the URL with your own.

**Claude Code**

```bash
claude mcp add --transport http atr https://<host>.<tailnet>.ts.net/mcp \
  --header "Authorization: Bearer $ATR_MCP_TOKEN"
```

**Cursor** (`~/.cursor/mcp.json`)

```json
{
  "mcpServers": {
    "atr": {
      "url": "https://<host>.<tailnet>.ts.net/mcp",
      "headers": { "Authorization": "Bearer ${env:ATR_MCP_TOKEN}" }
    }
  }
}
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.atr]
url = "https://<host>.<tailnet>.ts.net/mcp"
bearer_token_env_var = "ATR_MCP_TOKEN"
```

**Any other MCP client**: transport "Streamable HTTP", URL `https://<host>.<tailnet>.ts.net/mcp`, header `Authorization: Bearer <token>`.

### Rotating the token

```bash
umask 077; openssl rand -hex 32 > ~/.config/atr/mcp.token
atr service restart
```

atr re-reads the token on each request, so the restart is only needed when it comes from `ATR_MCP_TOKEN`; restarting is harmless either way. Then update every client. The old token stops working immediately.

### Keep the token secret

Never commit it, paste it into chat, issues, logs or screenshots, or put it in `config.toml`. Keep it in the 0600 file or `shadow.env`.

## Troubleshooting

| Symptom | What to check |
|---|---|
| TLS / certificate error on the `ts.net` URL | HTTPS Certificates not enabled in the admin console, or the first cert is still being issued. Enable it, wait a few seconds, retry. |
| `ts.net` name does not resolve | The client is not on the tailnet, the Tailscale app is off, or MagicDNS is off. Some machines override DNS (VPNs, custom resolvers, `/etc/resolv.conf` on Linux): check `tailscale status`, and turn on "Use Tailscale DNS" on the client. Using the Tailscale IP instead does not work with Serve's HTTPS (the cert is for the name). |
| 502 / connection refused through Serve | atr is not running or not on 7420. `atr status`, `atr service restart`, `curl http://127.0.0.1:7420/health` on the host, and `tailscale serve status` shows the right port. |
| Dashboard works, `/mcp` 401 | Wrong token, or the header is missing `Bearer `. |
| `/mcp` 503 | No token configured; see step 1 and `atr doctor`. |
| `/mcp` 403 | Request sent a browser `Origin`; call it from an MCP client, not a web page. |
| Bind alternative fails after reboot | The Tailscale IP changed or Tailscale was not up yet; switch to Serve. |
