# AGENTS.md

Working on the atr source? Read `skills/atr-dev/SKILL.md` first: repo map, build/test/lint, how to add config keys, CLI commands and dashboard changes, PR flow and the release rule.

Short version:

- `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --all -- --test-threads=4`
- Dashboard stays read-only; never expose secret values.
- Keep `skills/atr/SKILL.md` and `plugins/atr-operator/skills/atr/SKILL.md` identical (`sh tests/skills-sync.sh`).
- PR to `main`, merge on green CI, no force-push, never delete `main`.
- Release only when Rimas asks, only after `main` HEAD CI is green, via `release.yml` `bump=patch`.

Operating an installed atr (not developing it) uses `skills/atr/SKILL.md`.
