---
name: release
description: Cut a Symphony release by bumping the committed version, landing it, tagging the merged commit, and verifying the release workflow. Use when asked to release, tag, or retag Symphony.
---

# Release

1. Start from fresh `origin/main` in a clean worktree. Never disturb unrelated
   local changes.
2. Pick the requested version. If none is given, use the next patch after the
   latest `vX.Y.Z` tag.
3. Update `version = "X.Y.Z"` under `[workspace.package]` in the root
   `Cargo.toml` so it matches the intended tag, then run `cargo update -w` so
   `Cargo.lock` records the new version for the workspace crates. Search the
   old version and change other files only when they are true release-version
   sources, not examples.
4. Run `make all`, then commit, push, create a PR, and land it.
5. Fetch the merged `main` commit. Verify its `Cargo.toml` version, then create
   an annotated tag on that exact commit (the `release` workflow fails when the
   tag is not `v` + that version):

   ```sh
   git tag -a vX.Y.Z <merged-commit> -m "Symphony vX.Y.Z"
   git push origin vX.Y.Z
   ```

6. Watch the `release` workflow (`.github/workflows/release.yml`) until it
   finishes. Verify the build, smoke, publish-binaries, and publish-image jobs
   pass and that the GitHub release has all expected assets (four
   `symphony-vX.Y.Z-<target>` binaries, each with a `.sha256`).

Do not tag an uncommitted or unmerged revision. Do not move a published tag
without explicit user approval.
