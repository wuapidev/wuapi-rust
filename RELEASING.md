# Releasing the `wuapi` crate

The crate is developed in the wuapi monorepo (`packages/wuapi-sdk-rust`) and
published from its public mirror,
[wuapidev/wuapi-rust](https://github.com/wuapidev/wuapi-rust). Every change to
what the crate ships goes to crates.io when it reaches `main`; you only pick
the version.

```
monorepo PR (bump the version)
  -> merge into wuapidev/wuapi main
  -> sync-sdk-rust.yml pushes the folder to wuapidev/wuapi-rust main
  -> its release.yml publishes to crates.io, tags v<version>, creates a GitHub Release
```

## The crate is generated

`src/`, `tests/`, `Cargo.toml`, `README.md`, `LICENSE`, `rust-toolchain.toml`
and `.gitignore` are the output of the SDK generator
([`packages/sdk-codegen`](../sdk-codegen)) run on the API's OpenAPI spec,
`apps/wuapi/public/openapi.json`. Never edit them: the `codegen` CI job fails
when a committed file differs from what the generator writes, and the next
`bun run codegen` would undo the edit anyway.

To change the crate, change the spec, the generator config
(`packages/sdk-codegen/wuapi.sdk.toml`) or the templates
(`packages/sdk-codegen/templates/rust/`), then run `bun run codegen` from the
repository root and commit what it writes.

Only `.github/` and this file are hand-written.

## Cutting a release

1. In your monorepo pull request, bump `package_version` under
   `[targets.rust]` in `packages/sdk-codegen/wuapi.sdk.toml` and run
   `bun run codegen`. The generator writes it to `Cargo.toml` and to `VERSION`
   in `src/meta.rs`.

   We are pre-1.0 (`0.x`), so breaking changes may land in any release. By
   convention: a fix bumps the patch (`0.5.0` → `0.5.1`), anything else the
   minor (`0.5.0` → `0.6.0`).
2. Merge into `main`. `.github/workflows/sync-sdk-rust.yml` mirrors this folder
   to `wuapidev/wuapi-rust`, where `.github/workflows/release.yml` (this
   folder's `.github/`, at that repository's root) checks and tests the crate,
   runs `cargo publish` if crates.io does not have that version yet, then
   creates the `v<version>` tag and a GitHub Release with generated notes.

Re-running the release is safe: a version crates.io already has is skipped,
and an existing release is left alone. To run it by hand, open **Actions →
Release → Run workflow** in wuapi-rust on `main`; it is a dry run
(`cargo publish --dry-run`) unless you untick `dry-run`.

A published version cannot be replaced or deleted, only yanked
(`cargo yank --version <v> wuapi`). Fix forward with a new version.

Never commit to wuapi-rust directly: the sync stops when that repository's
`main` has commits it did not push (see `scripts/sync-sdk.sh` in the monorepo).

### What needs a bump

On monorepo pull requests the `Version bumped (Rust)` check
(`.github/workflows/sdk-rust-version.yml`) fails when `src/`, `Cargo.toml`,
`README.md` or `LICENSE` changed without a higher version. `tests/`, `.github/`
and this file need none.

## One-time setup

1. **Sync token.** A fine-grained GitHub token for `wuapidev/wuapi-rust` with
   Contents and Workflows read and write, saved in the monorepo
   (`wuapidev/wuapi`) as the Actions secret `SDK_RUST_TOKEN`. A deploy key
   cannot do it: GitHub refuses pushes that change `.github/workflows` without
   the workflows permission.
2. **Trusted publisher.** On crates.io, open the crate's **Settings → Trusted
   Publishing → Add** and enter: owner `wuapidev`, repository `wuapi-rust`,
   workflow `release.yml`, no environment. The release then needs no stored
   crates.io token.
