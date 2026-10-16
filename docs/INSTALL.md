# Installing atr

Short version: see the [README](../README.md#install).

## Release files

Release binaries: `macos-arm64` (macOS Apple Silicon only), `linux-x86_64` (glibc), `linux-arm64` (glibc), `linux-x86_64-musl` (static, Alpine). Files are named `atr-<ver>-<platform>.tar.gz`. There is no Intel Mac build — on Intel macOS use `cargo install --git`.

## Manual install

```bash
ver=0.1.1
platform=macos-arm64   # or linux-x86_64 / linux-arm64 / linux-x86_64-musl
curl -fsSL -o /tmp/atr.tgz \
  "https://github.com/rimusz/agent-task-runner/releases/download/v${ver}/atr-${ver}-${platform}.tar.gz"
tar -xzf /tmp/atr.tgz -C /tmp
install -m 755 /tmp/atr ~/.local/bin/atr
```

Or from this repo: `cargo install --path .`

## `atr: command not found`

`install.sh` installs to `~/.local/bin` (or `ATR_BIN_DIR`); `cargo install` uses `~/.cargo/bin`. If that dir is not on `PATH`, the installer prints a one-line fix, and `atr doctor` warns `atr on PATH`. Fix (zsh: `~/.zshenv`; bash: `~/.bashrc`):

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshenv && exec $SHELL -l
```

## Signing in to the coding CLIs

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

`atr init` writes your config, finds the CLIs you already installed, lets you pick models, installs the user service, and runs doctor. `--yes` enables every detected **and signed-in** CLI. Same check later: `atr providers detect`. It does not overwrite existing files unless you pass `--force`. It will not replace a loaded `dev.atr.serve` job or install a service for a non-default `ATR_CONFIG`/`HOME` unless you pass `--force-service` (or `--no-service` to skip). For GitHub you also need `git` and `gh` — see [docs/GITHUB.md](GITHUB.md). Secrets are never printed.

## Arch, Omarchy and Asahi

**Arch / Omarchy.** The `linux-x86_64` or `linux-x86_64-musl` binary works. Or `pacman -S rust` and `cargo install --git https://github.com/rimusz/agent-task-runner`.

**Asahi** (Apple Silicon Mac running Linux). Use `linux-arm64`. Asahi uses 16K pages — if that binary does not run, build from source (`cargo install --git` or `cargo install --path .`).

## Hosts

**Hosts.** atr runs on:

- a local Mac (Apple Silicon),
- any Linux distro (the Linux build is a static musl binary),
- a cloud VM or headless server. Run `sudo loginctl enable-linger "$USER"` so the service keeps running after you log out.

The coding CLIs atr drives (Claude, Codex, Cursor, Grok, OpenCode and others) must be installed and signed in on that same host.

## Full agent deploy prompt

Paste the prompt below into Claude Code, Codex, Cursor, or Grok on the machine that will run atr. The agent installs and configures atr end to end. **You** still sign in to each vendor CLI yourself; the prompt never asks for, handles, or prints API keys or tokens.

```
Deploy atr (https://github.com/rimusz/agent-task-runner) on this machine. Follow these steps in order. Never print, echo, log, or paste any secret, token, password, or API key. Never sign in to anything on my behalf.

1. Detect the platform with uname -s and uname -m, and whether this is a desktop session or a headless server/VM (no display, reached over SSH).
   - macOS arm64 -> platform macos-arm64. Intel Mac has no build: use cargo install --git instead.
   - Linux x86_64 glibc -> linux-x86_64; Linux x86_64 musl (Alpine) -> linux-x86_64-musl; Linux aarch64 -> linux-arm64.

2. Install the binary. Download atr-<ver>-<platform>.tar.gz from the latest GitHub release (verify the .sha256 if present), unpack it, and install it to ~/.local/bin/atr with mode 755. Create ~/.local/bin if needed and make sure it is on PATH (zsh: ~/.zprofile; bash: ~/.profile). If the download fails, fall back to: cargo install --git https://github.com/rimusz/agent-task-runner
   Confirm with: atr --version

3. Check the coding CLIs: atr providers detect
   It lists Claude Code, Grok Build, Cursor, Codex, OpenCode and others with version and signed-in status. Do NOT sign in for me. For each one that is installed but signed out, tell me the exact login command it prints and wait until I say I have done it. On macOS, "unverified (keychain locked)" over SSH is fine; Cursor must be signed in at the desktop.

4. Check GitHub access with gh auth status (it must show a logged-in account; never print the token). If not logged in, ask me to run gh auth login myself. Then run gh auth setup-git so git clone/push use the same token. A token is required for watching labels and posting comments; an SSH key is optional and only covers git.

5. Configure. Ask me whether I want to answer the setup questions myself (then hand me the terminal for: atr init) or accept defaults, which enables every detected and signed-in CLI:
   atr init --yes
   Never add --force unless I agree to overwrite an existing config.

6. Make sure the background service is installed and running (init normally does this already):
   atr service install
   On headless Linux also run: sudo loginctl enable-linger "$USER" so it keeps running after logout. On macOS the LaunchAgent (dev.atr.serve) must be loaded from the desktop session, not over SSH.

7. Ask me which GitHub repos to watch, then add each one and list them:
   atr repos add <owner/name> --path <local-clone-path>
   atr repos list

8. Optional, only if I say yes: let this agent use it over MCP.
   - Local: register an MCP server named "atr" in your own MCP config with command "atr" and args ["mcp"].
   - Remote (over Tailscale): create a random token file ~/.config/atr/mcp.token with mode 0600 (for example: umask 077; openssl rand -hex 32 > ~/.config/atr/mcp.token), add an [mcp] section with enabled = true to the config, then restart:
     atr service restart
     Expose it with Tailscale Serve (tailscale serve --bg 7420) and point the remote client at https://<host>.<tailnet>.ts.net/mcp with the header "Authorization: Bearer ${ATR_MCP_TOKEN}". Never display the token value. Full steps: docs/REMOTE-ACCESS.md in the atr repo.

9. Run the health check and report:
   atr doctor
   Report: OS and target, binary path, version, which CLIs are enabled and signed in, GitHub status (logged in yes/no, setup-git done), service status, repos added, MCP setup (if any), and every doctor line marked warn or FAIL with a suggested fix.
```
