---
description: Cut a new release of ssm-monitor (bump version, tag, push, GitHub release)
argument-hint: [version]
---

Cut a new release of ssm-monitor.

Requested version: `$1` (empty if not provided).

# Steps

1. **Read current version** from `Cargo.toml` (`[package].version`).
2. **Resolve the new version**:
   - If `$1` is provided, use it (strip a leading `v` if present).
   - Otherwise ask the user via `AskUserQuestion`, proposing the three SemVer bumps (patch / minor / major) from the current version, plus a free-text option.
3. **Pre-flight checks**:
   - `git status --short` must be empty (no uncommitted changes) — abort if not, ask user to commit or stash first.
   - Current branch must be `main` — abort otherwise.
   - `git fetch origin && git status -sb` — ensure local main is in sync with origin (not behind).
4. **Bump `Cargo.toml`**: replace the `version = "X.Y.Z"` line in the `[package]` section with the new version.
5. **Refresh `Cargo.lock`**: run `cargo build` (must succeed). This updates `Cargo.lock` with the new version.
6. **Show the diff**: print `git diff Cargo.toml Cargo.lock` and ask the user to confirm before committing.
7. **Commit**: stage `Cargo.toml` and `Cargo.lock`, commit with message `Release v<VERSION>`.
8. **Tag**: `git tag v<VERSION>`.
9. **Push**: `git push origin main && git push origin v<VERSION>`.
10. **Create GitHub release**: `gh release create v<VERSION> --generate-notes --title "v<VERSION>"`.
11. **Confirm the automation kicked in**: print the URL of the `update-homebrew` workflow run on `furybee/ssm-monitor`. Tell the user to check `gh run watch` or wait ~1 min, then verify `furybee/homebrew-tap` has a fresh commit updating the formula.

# Notes

- The push of the tag triggers the `release: published` event after `gh release create` runs, which fires the `update-homebrew.yml` workflow.
- If the workflow doesn't trigger or fails, fallback: `gh workflow run update-homebrew.yml -f version=v<VERSION> --repo furybee/ssm-monitor`.
- The `HOMEBREW_TAP_TOKEN` secret must be set on `furybee/ssm-monitor` (a fine-grained PAT with `Contents: write` on `furybee/homebrew-tap`).
- Never run destructive cleanup (resetting, deleting tags) on failure — surface the error and stop. The release tag, once pushed, is harder to retract cleanly.
