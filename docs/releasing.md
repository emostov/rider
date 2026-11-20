# Releasing Rider

Push a `v*` tag to create a GitHub release. Publishing to crates.io is a separate, manual step.

1. Open a pull request that bumps `version` under `[package]` in `Cargo.toml` and refreshes
   `Cargo.lock`, and wait for CI to pass. CI and the release workflow use `--locked`, so a stale
   lockfile fails both:

   ```sh
   cargo update -p swarm-rider --offline
   ```
2. Merge it to `main`, and wait for CI to pass on the merge commit.
3. Tag the merge commit with the version prefixed by `v` and push the tag:

   ```sh
   git switch main && git pull --ff-only
   git tag v0.2.0
   git push origin v0.2.0
   ```

## What the release workflow does

The workflow runs on every `v*` tag.

- `validate` fails unless the tag is `v` plus the `swarm-rider` version in the manifest and the
  tagged commit is an ancestor of `origin/main`.
- `fmt`, `clippy`, `test`, and `package` then run the CI checks again on Rust 1.97.1.
- `release` creates the GitHub release with generated notes, only if all four pass.

## Publishing to crates.io

CI never publishes. The owner does it by hand from the tagged commit:

```sh
git switch --detach v0.2.0

# Builds the tarball and compiles it in isolation.
cargo package

# Publishes that tarball.
cargo publish
```

The tag, the GitHub release, and the crates.io publish are separate steps. One does not
trigger the others.

The package is named `swarm-rider` because `rider` is taken on crates.io. The library and the
binary are still named `rider`. The crate `swarmrider` is only a placeholder and is not used.

## Rules

- **The tag equals `v` plus the manifest version.** The `validate` job refuses any other tag.
- **The tagged commit is on `main`.** The `validate` job refuses any other commit.
- **Wait for `main` CI before tagging.**
- **Never move, delete, or reuse a tag.** Bump to a new version instead.
