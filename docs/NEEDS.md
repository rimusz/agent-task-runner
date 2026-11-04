# Browser and desktop tasks (`needs`)

Some tasks need more than a shell: they must drive a web browser, or click
through a desktop app. A task says so with `needs`, and each provider says
what it can do with `capabilities`. The scheduler only gives a task to a
provider that lists **every** capability the task needs.

Known capabilities: `browser`, `desktop`.

## Provider side

```toml
[[provider]]
name = "grok"
kind = "grok"
capabilities = ["browser"]          # or "browser" / "browser,desktop"

[[provider]]
name = "codex-gui"
kind = "codex"
capabilities = ["browser", "desktop"]
```

Providers without `capabilities` only get tasks with no `needs` (the default).
Unknown capability names are a config error.

Codex can drive a browser through Playwright MCP, set in the provider config:

```toml
[[provider]]
name = "codex"
kind = "codex"
capabilities = ["browser"]
config_overrides = [
  'mcp_servers.playwright.command="/opt/homebrew/bin/npx"',
  'mcp_servers.playwright.args=["-y","@playwright/mcp@0.0.84","--headless","--isolated","--browser","chromium"]',
]
```

## Task side

| Intake | How to set it |
|--------|---------------|
| CLI | `atr add "check the signup page" --needs browser` (comma-separated or repeated) |
| MCP | `add_task` argument `"needs": ["browser"]` (array or comma string) |
| HTTP | `POST /tasks` with `"needs": ["desktop"]` |
| Inbox | `needs = ["desktop"]` in the TOML/JSON file |
| GitHub | label the issue `needs:browser` or `needs:desktop` (alongside the intake label) |

## Scheduling rules

- Naming a provider that lacks a needed capability is refused at intake:
  `provider codex lacks capability browser; providers with browser: claude`
  (`atr add`, HTTP, MCP). On GitHub, the `provider:` and `model:` labels are
  dropped with a warning and the `needs` stay. An inbox `provider` stays a soft
  hint: it is ignored and the next capable provider is used. See
  [USAGE.md](USAGE.md#per-task-provider-and-model).
- Setting a model pins the task strictly to the named provider (no fallback);
  `--provider` alone stays a soft preference. If that provider loses a needed
  capability after intake, the task fails with the reason instead of moving.
- If no capable provider is free, the task stays **queued** instead of running
  on an incapable one.
- `desktop` tasks also wait until the user running atr has a logged-in, local,
  unlocked graphical session:
  - Linux: `loginctl` reports an active, non-remote `x11`/`wayland` session
    with `LockedHint=no`.
  - macOS: the console is owned by the atr user and the screen is not locked.
  - Override with a top-level `desktop_check = "<command>"`; exit 0 means ready.
  The result is cached for about 10 seconds. `browser` tasks do not wait for a
  session (headless browsers are fine).

atr does not provide the browser or desktop itself; the provider CLI (via its
own MCP tools or computer-use features) does. `capabilities` just tells atr
which providers can be trusted with such tasks.
