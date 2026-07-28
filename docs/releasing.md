# Releasing spelunk

This document describes how to cut a release of spelunk.

## Overview

Releases are fully automated via GitHub Actions. Pushing a version tag triggers
`.github/workflows/release.yml`, which:

1. Builds `spelunk` and `spelunk-server` release binaries for all supported platforms.
2. Strips binaries where possible to reduce download size.
3. Packages each platform's binaries into a `.tar.gz` archive.
4. Builds an `amd64` Debian package (`spelunk_<version>_amd64.deb`).
5. Creates a GitHub Release and attaches all `.tar.gz` archives and the `.deb` as downloadable assets.
6. Auto-generates release notes from merged pull requests and commits.

Two install paths live outside this workflow:

- **`install.sh`** is fetched directly from the canonical copy on `main`
  (`https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh`),
  so the documented command always matches the committed script. It resolves the
  latest release tag via the GitHub API and downloads the matching tarball — it
  does not need updating per release. (The Windows `install.ps1` is fetched the
  same way.)
- **Homebrew tap** lives in the separate `spelunk-cloud/homebrew-spelunk`
  repo. The `update-homebrew-formula` job in `.github/workflows/release.yml`
  regenerates `Formula/spelunk.rb` with the new `url`/`sha256`/`version` and
  pushes it to that repo's `main` branch directly, using the
  `HOMEBREW_TAP_TOKEN` secret (a token with `contents: write` on
  `homebrew-spelunk` — `GITHUB_TOKEN` only has access to this repo).

## Supported platforms

| Target | Runner | Archive format | Notes |
|--------|--------|---------------|-------|
| `x86_64-unknown-linux-gnu` | ubuntu-latest | `.tar.gz` | Built in a `debian:11` container; binaries stripped |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm | `.tar.gz` | Native arm64 runner, built in a `debian:11` container |
| `aarch64-apple-darwin` | macos-latest | `.tar.gz` | Native build (Apple Silicon) |
| `x86_64-pc-windows-msvc` | windows-latest | `.zip` | Native build; produces `.exe` binaries |

> **Note:** `x86_64-apple-darwin` (Intel Mac) prebuilt binaries were dropped —
> Apple deprecated the architecture and Apple Silicon replaced it on new
> hardware six years ago. Intel Mac users build from source (see
> `docs/building.md`).

## Local dry run before tagging

The release workflow only triggers on a pushed `v*.*.*` tag: there is no
`workflow_dispatch`, so the packaging pipeline (glibc-floor container build,
the `.deb`'s `dpkg-shlibdeps`-derived `Depends`, and the floor install/smoke
test) otherwise gets exercised for the first time at real tag-push, after
which a passing run cascades straight into a GitHub Release and the
Homebrew/Scoop publish steps.

`scripts/release-dry-run.sh` reproduces the Linux x86_64 leg of that
pipeline locally, with Docker as the only prerequisite:

```bash
scripts/release-dry-run.sh
```

It builds `spelunk` + `spelunk-server` inside `debian:11` (the glibc 2.31
floor), runs the same glibc-ceiling check as CI, assembles and builds the
`.deb` (with `Depends` derived inside `debian:11`, matching the workflow),
and installs + smoke-tests the result in fresh `debian:11` / `ubuntu:20.04`
/ `ubuntu:24.04` containers.

**What it proves:** the Linux x86_64 build links against the glibc floor,
the `.deb` installs and its shipped binaries actually run (not just link)
on the support floor.

**What it does not prove:** macOS/Windows builds, the arm64 Linux leg, the
real GitHub Release, or the Homebrew/Scoop publish steps. Those are only
exercised by `.github/workflows/release.yml` at real tag-push time. The
script has no code path that can create a GitHub release, push to the
`homebrew-spelunk` tap, or write `bucket/spelunk.json`.

Run it before tagging; a failure here is cheaper to fix than one discovered
after a tag is already pushed.

### 1. Bump the version in `Cargo.toml`

Edit the `version` field in `Cargo.toml`:

```toml
[package]
name = "spelunk"
version = "0.8.0"   # <-- update this
```

### 1a. Check for hardcoded version references in docs

The install docs were rewritten to avoid hardcoding the version: `docs/getting-started.md`
points at `install.sh` / Homebrew and uses a `<version>` placeholder for manual
tarball and `.deb` downloads, so it normally needs no per-release edit. Still,
sweep for stray hardcoded versions before tagging:

```bash
grep -rn "spelunk-v[0-9]\|spelunk_[0-9]" docs/ README.md
```

Fix anything that pins a specific old version (use `<version>` or point at
`install.sh`). Commit everything together:

```bash
git add Cargo.toml Cargo.lock docs/
git commit -m "chore: bump version to 0.8.0"
git push origin main
```

### 2. Tag and push

```bash
git tag v0.8.0
git push origin v0.8.0
```

That's it. The release workflow triggers automatically on the pushed tag.

### 3. Monitor the workflow

Watch progress at:
`https://github.com/spelunk-cloud/spelunk/actions/workflows/release.yml`

Once all jobs pass, the release appears at:
`https://github.com/spelunk-cloud/spelunk/releases/tag/v0.8.0`

## Pre-releases

Append a pre-release suffix to the tag. The workflow automatically marks the
GitHub Release as a pre-release when the tag contains `-rc`, `-beta`, or
`-alpha`:

```bash
git tag v0.8.0-rc.1
git push origin v0.8.0-rc.1
```

## Download URLs

After a release is published, assets follow these patterns (the `<version>`
segment is the full tag, e.g. `v0.8.0`):

```
# Unix tarballs
https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk-<version>-<target>.tar.gz

# Windows zip
https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk-<version>-x86_64-pc-windows-msvc.zip

# Debian package (amd64)
https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk_<version-no-v>_amd64.deb
```

Examples for `v0.9.0`:

```bash
# macOS Apple Silicon
https://github.com/spelunk-cloud/spelunk/releases/download/v0.9.0/spelunk-v0.9.0-aarch64-apple-darwin.tar.gz

# Linux x86_64
https://github.com/spelunk-cloud/spelunk/releases/download/v0.9.0/spelunk-v0.9.0-x86_64-unknown-linux-gnu.tar.gz

# Linux ARM64
https://github.com/spelunk-cloud/spelunk/releases/download/v0.9.0/spelunk-v0.9.0-aarch64-unknown-linux-gnu.tar.gz

# Windows x86_64
https://github.com/spelunk-cloud/spelunk/releases/download/v0.9.0/spelunk-v0.9.0-x86_64-pc-windows-msvc.zip

# Debian (amd64)
https://github.com/spelunk-cloud/spelunk/releases/download/v0.9.0/spelunk_0.9.0_amd64.deb
```

> `releases/latest/download/<asset>` also works when the asset name is exact,
> but the tag-pinned `releases/download/<version>/<asset>` form is unambiguous
> and avoids the stale-filename 404s tracked in #340.

## Deleting a bad release

If a release needs to be pulled:

```bash
# Delete the tag locally and on remote
git tag -d v0.8.0
git push origin :refs/tags/v0.8.0

# Delete the GitHub Release (requires gh CLI)
gh release delete v0.8.0 --yes
```

Then fix the issue, re-commit, and re-tag.
