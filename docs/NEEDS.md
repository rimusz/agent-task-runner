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

## Task side

| Intake | How to set it |
|--------|---------------|
| CLI | `atr add "check the signup page" --needs browser` (comma-separated or repeated) |
| MCP | `add_task` argument `"needs": ["browser"]` (array or comma string) |
| HTTP | `POST /tasks` with `"needs": ["desktop"]` |
| Inbox | `needs = ["desktop"]` in the TOML/JSON file |
| GitHub | label the issue `needs:browser` or `needs:desktop` (alongside the intake label) |

## Scheduling rules

- A provider hint (`--provider`) is ignored when that provider lacks a needed
  capability; the next capable provider is used.
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
