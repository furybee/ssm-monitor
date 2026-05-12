---
description: Cut a new release of ssm-monitor (bump version, tag, push — dist handles binaries + Homebrew)
argument-hint: [version]
---

Cut a new release of ssm-monitor.

Requested version: `$1` (empty if not provided).

# How releases work in this repo

We use [`dist`](https://opensource.axo.dev/cargo-dist/) (formerly `cargo-dist`). When a tag matching `v*` is pushed, `.github/workflows/release.yml` runs and:

1. Cross-compiles `ssm-monitor` for macOS (arm64 + x86_64) and Linux (arm64 + x86_64)
2. Creates the GitHub Release (with auto-generated notes)
3. Uploads per-platform binary tarballs
4. Publishes an updated `Formula/ssm-monitor.rb` to `furybee/homebrew-tap`

So the local workflow boils down to: bump → commit → tag → push. No `gh release create` needed.

# Steps

1. **Read current version** from `Cargo.toml` (`[package].version`).
2. **Resolve the new version**:
   - If `$1` is provided, use it (strip a leading `v` if present).
   - Otherwise ask the user via `AskUserQuestion`, proposing the three SemVer bumps (patch / minor / major) from the current version, plus a free-text option.
3. **Pre-flight checks**:
   - `git status --short` must be empty — abort if not, ask the user to commit or stash first.
   - Current branch must be `main` — abort otherwise.
   - `git fetch origin && git status -sb` — local main must not be behind origin.
4. **Bump `Cargo.toml`**: replace the `version = "X.Y.Z"` line in the `[package]` section with the new version.
5. **Refresh `Cargo.lock`**: run `cargo build`. Must succeed.
6. **Show the diff** of `Cargo.toml` and `Cargo.lock`, then ask the user to confirm before committing — release tags are hard to retract cleanly.
7. **Commit**: stage `Cargo.toml` and `Cargo.lock`, commit with message `Release v<VERSION>`.
8. **Tag**: `git tag v<VERSION>`.
9. **Push**: `git push origin main && git push origin v<VERSION>`. The tag push triggers the dist workflow.
10. **Watch the release**: tell the user the workflow URL is at `https://github.com/furybee/ssm-monitor/actions`, and suggest `gh run watch --repo furybee/ssm-monitor`. The full CI takes ~5 min (cross-compile + linker + upload).

# Notes

- The `HOMEBREW_TAP_TOKEN` secret must be set on `furybee/ssm-monitor` (fine-grained PAT with `Contents: write` on `furybee/homebrew-tap`). Without it, binaries are still uploaded but the Homebrew tap won't update.
- If something goes wrong AFTER the tag is pushed: don't `git tag -d` and force-push — that's a mess once the workflow has touched anything. Bump to the next version and try again.
- Never run destructive cleanup on failure. Surface the error and stop.
