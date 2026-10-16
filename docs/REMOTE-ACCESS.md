# Remote access over Tailscale

Reach atr's dashboard and its HTTP MCP endpoint from another Mac, an iPhone or a remote agent, using only your own Tailscale network (tailnet). Nothing here exposes atr to the internet.

## What is reachable, and what protects it

atr serves one HTTP listener (`listen`, default `127.0.0.1:7420`):

| Path | Auth |
|---|---|
| `GET /`, `/dashboard.json`, `/health`, `/tasks/<key>/result` (dashboard) | **None** (read-only) |
| `GET /status`, `/tasks/<key>`, `/tasks/<key>/result` | **None** (read-only) |
| `POST /tasks`, `/tasks/<key>/messages`, `/tasks/<key>/cancel`, `/tasks/<key>/retry`, `/intake/pause`, `/intake/resume`, `/hooks` | Bearer token |
| `POST /mcp` | Bearer token |

Every state-changing route uses the same token as `/mcp` (see [MCP over HTTP](#mcp-over-http)), compared in constant time, and rejects a foreign browser `Origin` (403). Missing or wrong token: 401. No token configured: 503. `atr serve` creates a 0600 token file at start if none exists, and the local CLI (`atr add`, `atr submit`, `atr cancel`, `atr retry`, `atr pause`, `atr resume`) and the Grok plugin hook scripts read it automatically, so local use needs no setup.

The dashboard is read-only (no buttons or forms) and has no login: anyone who can reach the listener can view task titles, status and usage. Keep the tailnet to your own devices, or restrict the host with [Tailscale ACLs](https://tailscale.com/kb/1018/acls). Secrets (Telegram and GitHub tokens) are redacted from the dashboard data.

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

`atr ui --print` still prints the local `http://127.0.0.1:7420/` address; it does not know about Serve. Telegram `/ui` does: it sends `[dashboard] public_url` if set, else the detected Serve URL. To pin it:

```toml
[dashboard]
public_url = "https://<host>.<tailnet>.ts.net/"
```

### Never use Funnel

Do not run `tailscale funnel`, and do not bind atr to `0.0.0.0` on a machine with a public IP, port-forward 7420, or put it behind a public reverse proxy. The dashboard has no auth, and the bearer token is the only thing guarding writes.

## Alternative: bind to the Tailscale IP

The older approach from [CONFIG.md](CONFIG.md#dashboard): listen on the host's Tailscale IP (`tailscale ip -4`).

```toml
listen = "<tailscale-ip>:7420"

[dashboard]
enabled = true
bind = "<tailscale-ip>"
```

Or keep `listen` on loopback and set only `[dashboard] bind = "<tailscale-ip>"`: atr then adds a second, read-only listener on `<tailscale-ip>:7420` serving just `/`, `/dashboard.json`, `/health` and the read-only `/tasks/<key>/result` view, so task submit/cancel and `/mcp` stay on loopback.

Restart with `atr service restart`. Trade-offs versus Serve:

- Plain HTTP, not HTTPS (traffic is still WireGuard-encrypted inside the tailnet, but browsers mark it insecure).
- The Tailscale IP is stable in practice but can change (re-auth, new node), and then the config is wrong and atr cannot bind.
- With `listen` on the Tailscale IP, the full API and `/mcp` are on the tailnet (writes still need the token), and the host itself must use that IP instead of `127.0.0.1`.

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

`atr doctor` should show `ok mcp token` and `ok http write auth`. With no token, doctor warns (`HTTP /mcp enabled, no token`, `http write auth`) and `/mcp` and the write routes answer 503 `mcp token not configured`. atr never prints the token. `atr serve` creates the token file at start when none exists.

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

atr re-reads the token on each request, so the restart is only needed when it comes from `ATR_MCP_TOKEN`; restarting is harmless either way. The local CLI and hook scripts pick up the new file on their next call. Update every remote client; the old token stops working immediately.

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
| `POST /tasks`, `/messages`, `/cancel` or `/hooks` 401 | Script or tool calling the HTTP API without `Authorization: Bearer <token>`; read the token file. The `atr` CLI falls back to the local database if the HTTP call is refused. |
| Same routes 503 | No token; restart `atr serve` (creates one) or run `atr init`. |
| No live progress from Grok runs after an upgrade | Hook scripts are rewritten on each new Grok run; restart `atr serve` so it has the token. |
| Bind alternative fails after reboot | The Tailscale IP changed or Tailscale was not up yet; switch to Serve. |
