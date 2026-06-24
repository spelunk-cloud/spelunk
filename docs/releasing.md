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

- **`install.sh`** is hosted at `https://spelunk.cloud/install.sh`. It resolves
  the latest release tag via the GitHub API and downloads the matching tarball —
  it does not need updating per release.
- **Homebrew tap** lives in the separate `spelunk-cloud/homebrew-spelunk`
  repo. The `update-homebrew-formula` job in `.github/workflows/release.yml`
  regenerates `Formula/spelunk.rb` with the new `url`/`sha256`/`version` and
  pushes it to that repo's `main` branch directly, using the
  `HOMEBREW_TAP_TOKEN` secret (a token with `contents: write` on
  `homebrew-spelunk` — `GITHUB_TOKEN` only has access to this repo).

## Supported platforms

| Target | Runner | Notes |
|--------|--------|-------|
| `x86_64-unknown-linux-gnu` | ubuntu-latest | Native build |
| `aarch64-unknown-linux-gnu` | ubuntu-latest | Cross-compiled via `cross` |
| `aarch64-apple-darwin` | macos-latest | Native build (Apple Silicon) |

> **Note:** `x86_64-apple-darwin` (Intel Mac) prebuilt binaries were dropped —
> Apple deprecated the architecture and Apple Silicon replaced it on new
> hardware six years ago. Intel Mac users build from source (see
> `docs/getting-started.md`).

## Cutting a release

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
# Tarballs
https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk-<version>-<target>.tar.gz

# Debian package (amd64)
https://github.com/spelunk-cloud/spelunk/releases/download/<version>/spelunk_<version-no-v>_amd64.deb
```

Examples for `v0.8.0`:

```bash
# macOS Apple Silicon
https://github.com/spelunk-cloud/spelunk/releases/download/v0.8.0/spelunk-v0.8.0-aarch64-apple-darwin.tar.gz

# Linux x86_64
https://github.com/spelunk-cloud/spelunk/releases/download/v0.8.0/spelunk-v0.8.0-x86_64-unknown-linux-gnu.tar.gz

# Linux ARM64
https://github.com/spelunk-cloud/spelunk/releases/download/v0.8.0/spelunk-v0.8.0-aarch64-unknown-linux-gnu.tar.gz

# Debian (amd64)
https://github.com/spelunk-cloud/spelunk/releases/download/v0.8.0/spelunk_0.8.0_amd64.deb
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
