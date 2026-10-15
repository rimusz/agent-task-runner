# Releasing

Releases happen only when you ask for one. A push or merge to `main` just builds and tests (`ci.yml`); it never publishes a release.

## Cut a release

Either:

- **From GitHub:** Actions → **release** → **Run workflow** on `main`. Leave **version** empty to bump the patch from the latest `v*` tag (pick **bump** `minor` or `major` if needed), or type an exact version such as `0.2.0`.
- **From a tag:** tag the commit on `main` and push the tag:

  ```sh
  git tag v0.2.0 && git push origin v0.2.0
  ```

The workflow builds macOS arm64 and Linux (x86_64, arm64, x86_64 musl) archives with `.sha256` files and publishes the GitHub Release `vX.Y.Z`. A manual run creates the tag at the current `main` commit.

## Version

The tag is the version. The build sets `ATR_RELEASE_VERSION` (read by `build.rs`), and stamps `Cargo.toml` in the build checkout only, so the binary's `atr --version` matches the tag; the build fails if it doesn't. CI never commits to `main`, so `Cargo.toml` on `main` may show an older version.

## Rules

- Never force-push or delete `main` or release tags. An existing tag or release is never overwritten; pick the next version instead.
- `contrib/release-decide.sh` picks the version; its cases are in `tests/release-decide.sh`.
