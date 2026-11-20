# Releasing Rider

Push a `v*` tag to create a GitHub release. Publishing to crates.io is a separate, manual step.

1. Open a pull request that bumps `version` under `[package]` in `Cargo.toml` and refreshes
   `Cargo.lock`, and wait for CI to pass. CI and the release workflow use `--locked`, so a stale
   lockfile fails both. To refresh the lockfile:

   ```sh
   cargo update -p swarm-rider --offline
   ```

2. Merge it to `main`, and wait for CI to pass on the merge commit.
3. Tag the merge commit with `v` plus the version, and push the tag:

   ```sh
   git switch main && git pull --ff-only
   git tag v0.2.0
   git push origin v0.2.0
   ```

## What the release workflow does

The workflow runs on every `v*` tag.

- `validate` fails unless the tag is `v` plus the `swarm-rider` version in the manifest and the
  tagged commit is an ancestor of `refs/remotes/origin/main`.
- `fmt`, `clippy`, `test`, and `package` then run the CI checks again. They use the Rust version
  pinned in `rust-toolchain.toml`.
- `release` creates the GitHub release with generated notes, only if all four pass.

## Publishing to crates.io

Only the crates.io publish is manual. CI never publishes. Push the tag, wait for the release
workflow to pass, then publish from the tagged commit:

```sh
(
  set -e
  tag=v0.2.0
  git switch --detach "refs/tags/$tag"
  test "$(git rev-parse HEAD)" = "$(git rev-parse "refs/tags/$tag^{commit}")"
  cargo package --locked
  cargo publish --locked
)
```

The subshell stops at the first failing command, so a missing tag never reaches `cargo publish`.
`cargo publish` repackages and verifies the tarball before it uploads.

If a step fails, fix the cause and rerun the step. Do not move the tag. If the source must
change, bump to a new version and release again.

The package is named `swarm-rider` because `rider` is taken on crates.io. The library and the
binary are still named `rider`. The crate `swarmrider` is only a placeholder and is not used.

## Rules

- **The tag equals `v` plus the manifest version.** The `validate` job refuses any other tag.
- **The tagged commit is on `main`.** The `validate` job refuses any other commit.
- **Wait for `main` CI before tagging.**
- **Never move, delete, or reuse a tag.** Bump to a new version instead.
