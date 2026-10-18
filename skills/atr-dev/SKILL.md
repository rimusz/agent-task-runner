---
name: atr-dev
description: Develop atr itself (rimusz/agent-task-runner) — repo map, build/test/lint, adding config keys, CLI commands and dashboard changes, docs to update, PR flow and the release rule. Not for operating an installed atr (that is the `atr` skill).
---

# atr-dev

For agents and contributors changing the atr source. To *operate* an installed atr, use `skills/atr/SKILL.md` instead.

atr is a single Rust binary (`atr`, edition 2021, MSRV 1.85) that runs as a long-lived service and drives official agent CLIs (claude, grok, cursor, codex, opencode) on subscription plans.

## Repo map

| Path | What |
|------|------|
| `src/main.rs` | CLI entry, clap subcommands, dispatch |
| `src/lib.rs` | module list |
| `src/config.rs`, `src/config_edit.rs` | `~/.config/atr/config.toml` schema + defaults; in-place edits (`atr config set`, `atr driver set`, …) |
| `src/providers.rs`, `providers_edit.rs`, `presets.rs`, `opencode_cfg.rs`, `detect.rs` | provider kinds, presets, CLI detection/versions |
| `src/scheduler.rs`, `runner.rs`, `task.rs`, `plan.rs`, `review.rs`, `verify.rs`, `worktree.rs`, `handoff.rs` | task lifecycle: plan → run in worktree → review/verify |
| `src/shadow.rs`, `github.rs`, `github_auth.rs`, `gh_token*.rs` | GitHub shadow mode, labels, gh wrapper, per-owner tokens |
| `src/telegram*.rs`, `notify.rs` | Telegram bot/assistant, health, alerts, `[notify] command` hook |
| `src/http.rs`, `dashboard.rs`, `dashboard.html`, `mcp.rs` | HTTP API, read-only dashboard (`dashboard.json`), MCP server |
| `src/doctor.rs`, `service.rs`, `upgrade.rs`, `init.rs`, `skill.rs`, `plugin.rs` | doctor checks, launchd/systemd service, self-upgrade, init, skill install |
| `src/db.rs`, `usage.rs`, `quota.rs`, `gc.rs`, `logrotate.rs` | SQLite state, usage/quota, cleanup |
| `src/lessons.rs` | per-repo lessons: scrubber, `## Repo lessons (atr)` block (variable tail only, first runs, worker roles), export; automatic reflect (signal mapping, hidden `<key>-reflect` task, JSON parse, dedupe, ttl). Fake CLIs answer reflect runs from `fake/reflect.json` |
| `tests/` | integration tests (`e2e.rs`, `init.rs`, `readme.rs` docs checks, `release_decide.rs`, shell tests, `fixtures/` fake CLIs) |
| `contrib/` | service units, `release-decide.sh`, `next-version.sh` |
| `docs/` | user docs (CONFIG, HOW-IT-WORKS, TROUBLESHOOTING, USAGE, INSTALL, DESIGN, RELEASING, …) |
| `skills/atr/SKILL.md` | operator skill, embedded in the binary via `include_str!` (`src/skill.rs`) |
| `plugins/atr-operator/` | Agent Plugin; its `skills/atr/SKILL.md` must stay byte-identical to `skills/atr/SKILL.md` |
| `skills/atr-dev/SKILL.md` | this file (canonical); agent dirs symlink to it |

## Build and run locally

```sh
cargo build                       # debug
cargo build --release             # target/release/atr
./target/debug/atr --version
./target/debug/atr doctor
```

Use a throwaway config/home when trying the service so you do not touch a real install (`ATR_SKILL_HOME` redirects skill installs). Never put secrets in config or commits; secrets live in `~/.config/atr/shadow.env`. gitleaks runs in CI.

## Test, lint, format (same as CI)

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all -- --test-threads=4
cargo test --test readme          # docs checks only (README.md, docs/*.md)
sh tests/release-decide.sh        # release version rules
sh tests/skills-sync.sh           # skill copies/symlinks have not drifted
shellcheck -s sh -S error install.sh contrib/*.sh tests/*.sh
cargo deny check                  # licenses/advisories (CI: deny job)
```

Known flaky: none. Root-caused and fixed (do not reintroduce):
- `ETXTBSY` ("Text file busy") when a test writes a script and execs it while another thread forks: spawn scripts through `util::retry_exec_busy` / `util::output_retry_busy` (production does too).
- Read-then-write status races with the scheduler: use atomic conditional updates (`Db::queue_unless_running`, `claim_task`), never `get_task` + `set_status`.
- HTTP handlers that reject before reading the request body must drain it first, or the client can get RST and lose the response body.
- Tests use their own temp dir for every path (no shared `/tmp/...` files), `127.0.0.1:0` ports, and wait on conditions (`Harness::wait_until`) rather than fixed sleeps.

If a test fails intermittently, find the cause; do not add sleeps, bump timeouts or `#[ignore]` it.

CI (`.github/workflows/ci.yml`): docs-only diffs (`docs/`, `*.md`, LICENSE) skip the Rust jobs and run just the readme test; code diffs run fmt, clippy, tests on Linux + macOS, deny, coverage, cross builds. gitleaks and release-rule always run.

## Making changes

- **Config key:** add to the struct in `src/config.rs` with a serde default so old configs still load; unknown/removed keys must be ignored with an info line, never a hard error. Add edit support in `config_edit.rs` if settable. Document it in `docs/CONFIG.md` (tests/readme.rs checks CONFIG.md).
- **CLI command:** add the clap subcommand in `src/main.rs`, logic in its module, `--json` output where status-like. Document in `docs/USAGE.md`/`CONFIG.md`, and in the operator skill if operators use it.
- **Doctor / status:** new health facts get a `doctor` line (ok/warn/fail) and, if useful, a `status` line and dashboard badge.
- **Prompt text:** per-task additions go after `prompt::VARIABLE_MARKER` via `task::insert_before_messages`, never into the stable prefix (`tests/lessons.rs` asserts the prefix hash stays equal when lessons change).
- **Dashboard:** keep it **read-only** (GET `dashboard.json` + `dashboard.html`; no mutating endpoints, no actions). Never expose secret values — env var names only.
- **Docs to update with behaviour changes:** `docs/CONFIG.md`, `docs/HOW-IT-WORKS.md`, `docs/TROUBLESHOOTING.md` (plus USAGE/README as relevant).
- **Operator skill:** edit `skills/atr/SKILL.md` and copy it to `plugins/atr-operator/skills/atr/SKILL.md` — the two must be identical (`tests/skills-sync.sh` enforces this).
- No hardcoded policy: provider order, models, driver, repos all come from config.

## PR flow

1. Branch from `main` (`git checkout -b <topic>`); small focused PRs.
2. Run fmt/clippy/tests locally; open a PR to `main`.
3. Merge only when CI is green (squash). Re-run a job only for the known flaky test.
4. Never force-push, never rewrite pushed history, never delete `main`, `develop`, `rimas-develop` or release tags.

## Release rule

- Release **only when Rimas asks**.
- Only after the post-merge `ci` run on `main` HEAD has finished **green** (wait for it; do not release while it is running).
- Then: `gh workflow run release.yml -R rimusz/agent-task-runner --ref main -f bump=patch`, wait for success, confirm the `vX.Y.Z` release.
- Docs-only changes do not need a release. A merge to `main` never releases by itself. Details: `docs/RELEASING.md`.
