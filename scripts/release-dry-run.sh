#!/usr/bin/env bash
#
# scripts/release-dry-run.sh
#
# Local, docker-based dry run of the Linux leg of .github/workflows/release.yml:
# build inside debian:11 (the glibc 2.31 floor), enforce the glibc ceiling on
# the resulting binaries, assemble the .deb with its Depends line derived by
# dpkg-shlibdeps run inside debian:11, then install and smoke-test the .deb in
# a fresh floor container. Run this before pushing a version tag, to catch
# release breakage on a dev machine instead of at tag-push time.
#
# What this proves: the Linux x86_64 build links against the glibc floor,
# the .deb's Depends line is floor-derived (so it can actually install on
# debian:11 / ubuntu:20.04), and the installed package survives real
# subcommands, not just `apt-get install`.
#
# What this does NOT prove: macOS or Windows builds, the arm64 Linux leg,
# the actual GitHub Release, or the Homebrew/Scoop publish steps. Those are
# only exercised by release.yml at real tag-push time. This script has no
# code path that can create a GitHub release, push to homebrew-spelunk, or
# write bucket/spelunk.json -- see the "explicitly does not touch" note
# below the stage functions.
#
# Usage:
#   scripts/release-dry-run.sh
#
# Requires: Docker only. No GITHUB_TOKEN, no tag push, no write access to
# any repo other than this script's own gitignored output directory
# (target/release-dry-run/).
#
# Env overrides (optional):
#   SMOKE_IMAGES  space-separated list of images to install+smoke-test the
#                 built .deb in. Defaults to the three floor/current images
#                 release.yml's own "deb" job smoke-tests against.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TARGET="x86_64-unknown-linux-gnu"
FEATURES="rich-formats"
BUILD_IMAGE="debian:11"
SMOKE_IMAGES="${SMOKE_IMAGES:-debian:11 ubuntu:20.04 ubuntu:24.04}"
# Every container below must run as amd64, matching the amd64 target/.deb --
# on an arm64 host (e.g. Apple Silicon), Docker silently resolves some image
# tags to a native arm64 manifest and others to amd64 depending on what's
# cached, with no warning either way. Without pinning this, a floor-image
# smoke test can fail with "Depends: libc6:amd64 ... not installable" that
# has nothing to do with the .deb's real installability -- the container
# itself has no amd64 architecture enabled. Pinning makes every stage
# deterministic regardless of host architecture.
DOCKER_PLATFORM="linux/amd64"

WORKDIR="target/release-dry-run"
DEB_LAYOUT="${WORKDIR}/BUILD"
CACHE_CARGO="${WORKDIR}/cache/cargo"
CACHE_RUSTUP="${WORKDIR}/cache/rustup"

VERSION="$(grep -m1 '^version' crates/spelunk-cli/Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')-dryrun"
DEB_VERSION="${VERSION}"

CONTAINER_PREFIX="spelunk-release-dry-run-$$"
STAGE="init"

# --- diagnostics -------------------------------------------------------

die() {
  echo "" >&2
  echo "release-dry-run FAILED at stage: ${STAGE}" >&2
  echo "  ${1}" >&2
  exit 1
}

log_stage() {
  STAGE="$1"
  echo ""
  echo "=== release-dry-run: ${STAGE} ==="
}

# --rm containers clean themselves up on normal exit; this trap also sweeps
# up anything left behind by an interrupted (killed) run, so no manual
# `docker system prune` is ever required.
cleanup() {
  local leftover
  leftover="$(docker ps -aq --filter "name=${CONTAINER_PREFIX}" 2>/dev/null || true)"
  if [ -n "${leftover}" ]; then
    # Word-splitting is intentional: leftover is a newline-separated list of
    # container ids, all passed to one rm -f.
    # shellcheck disable=SC2086
    docker rm -f ${leftover} >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "${CACHE_CARGO}" "${CACHE_RUSTUP}"
rm -rf "${DEB_LAYOUT}"
mkdir -p "${DEB_LAYOUT}"

# --- stage 1: build inside the glibc floor container --------------------
#
# Mirrors release.yml lines 55-117: git/curl/build tooling installed fresh
# (nothing is preinstalled in the base image), rustup stable, then a release
# build with the same feature set the Linux legs ship with. Building on the
# host's own userland instead of debian:11 is exactly the mistake this
# script exists to catch -- it would silently raise the glibc floor.
build_in_floor_container() {
  log_stage "build (${BUILD_IMAGE}, target ${TARGET})"
  docker run --rm --name "${CONTAINER_PREFIX}-build" \
    --platform "${DOCKER_PLATFORM}" \
    -v "${REPO_ROOT}:/repo" \
    -v "${REPO_ROOT}/${CACHE_CARGO}:/root/.cargo" \
    -v "${REPO_ROOT}/${CACHE_RUSTUP}:/root/.rustup" \
    -w /repo \
    "${BUILD_IMAGE}" bash -euc '
      set -euo pipefail
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends \
        git curl ca-certificates build-essential pkg-config libdbus-1-dev binutils
      if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
        curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
          | sh -s -- -y --profile minimal --default-toolchain stable
      fi
      export PATH="$HOME/.cargo/bin:$PATH"
      cargo build --release --target '"${TARGET}"' --features '"${FEATURES}"'
      strip "target/'"${TARGET}"'/release/spelunk"
      strip "target/'"${TARGET}"'/release/spelunk-server"
    ' || die "container build failed (see docker output above)"

  for bin in spelunk spelunk-server; do
    [ -x "target/${TARGET}/release/${bin}" ] || die "expected binary target/${TARGET}/release/${bin} not found after build"
  done
}

# --- stage 2: glibc ceiling check ---------------------------------------
#
# Lifted near-verbatim from release.yml lines 125-144. A missing, empty, or
# non-numeric ceiling is a failure, not a silent pass.
enforce_glibc_ceiling() {
  log_stage "glibc ceiling check (floor: GLIBC_2.31)"
  docker run --rm --name "${CONTAINER_PREFIX}-glibc-check" \
    --platform "${DOCKER_PLATFORM}" \
    -v "${REPO_ROOT}:/repo:ro" \
    -w /repo \
    "${BUILD_IMAGE}" bash -euc '
      set -euo pipefail
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends binutils >/dev/null
      for bin in spelunk spelunk-server; do
        path="target/'"${TARGET}"'/release/${bin}"
        raw="$(objdump -T "$path")"
        ceiling="$(printf "%s\n" "$raw" | grep -o "GLIBC_[0-9.]*" | sort -Vu | tail -1 || true)"
        if ! [[ "$ceiling" =~ ^GLIBC_[0-9]+\.[0-9]+$ ]]; then
          echo "ERROR: ${bin}: no valid versioned GLIBC symbol found (got '"'"'${ceiling}'"'"')" >&2
          exit 1
        fi
        echo "${bin}: max versioned glibc symbol = ${ceiling}"
        max="$(printf "%s\nGLIBC_2.31\n" "$ceiling" | sort -V | tail -1)"
        if [ "$max" != "GLIBC_2.31" ]; then
          echo "ERROR: ${bin} requires ${ceiling}, above the glibc 2.31 floor" >&2
          exit 1
        fi
      done
    ' || die "glibc ceiling check failed -- a binary links against a glibc symbol newer than 2.31, or has no versioned GLIBC symbols at all"
}

# --- stage 3: assemble the .deb layout + derive Depends ------------------
#
# Depends is derived from the packaged binaries, never hardcoded, and
# derived INSIDE debian:11 (the build floor) -- release.yml lines 228-246
# explains why: deriving it on a newer-glibc host resolves to a Depends line
# that can't install on the floor. Layout assembly and control-file
# generation (via the existing write-deb-control.js -- pure Node builtins,
# nothing CI-specific) run on the host; only dpkg-shlibdeps itself needs the
# container.
assemble_deb() {
  log_stage "assemble .deb layout + derive Depends (${BUILD_IMAGE})"

  mkdir -p "${DEB_LAYOUT}/DEBIAN" "${DEB_LAYOUT}/usr/bin" "${DEB_LAYOUT}/usr/lib/systemd/user"
  install -m 755 "target/${TARGET}/release/spelunk" "${DEB_LAYOUT}/usr/bin/spelunk"
  install -m 755 "target/${TARGET}/release/spelunk-server" "${DEB_LAYOUT}/usr/bin/spelunk-server"
  install -m 644 packaging/spelunk-server.service "${DEB_LAYOUT}/usr/lib/systemd/user/spelunk-server.service"

  local deb_depends
  deb_depends="$(docker run --rm --name "${CONTAINER_PREFIX}-shlibdeps" \
    --platform "${DOCKER_PLATFORM}" \
    -v "${REPO_ROOT}:/w:ro" "${BUILD_IMAGE}" bash -euc '
      set -euo pipefail
      apt-get update -qq >/dev/null
      apt-get install -y -qq --no-install-recommends dpkg-dev libdbus-1-3 >/dev/null
      mkdir -p /tmp/sd/debian
      printf "Source: spelunk\nMaintainer: spelunk-cloud <hello@spelunk.cloud>\n\nPackage: spelunk\nArchitecture: amd64\nDescription: placeholder\n placeholder\n" > /tmp/sd/debian/control
      cd /tmp/sd
      dpkg-shlibdeps -O "/w/'"${DEB_LAYOUT}"'/usr/bin/spelunk" "/w/'"${DEB_LAYOUT}"'/usr/bin/spelunk-server"
    ')" || die "dpkg-shlibdeps failed inside ${BUILD_IMAGE}"
  echo "Derived ${deb_depends}"

  DEB_DEPENDS="${deb_depends}" \
    node .github/scripts/write-deb-control.js \
    --deb-version "${DEB_VERSION}" \
    --out "${DEB_LAYOUT}/DEBIAN/control" \
    || die "write-deb-control.js failed"
}

# --- stage 4: build the .deb ---------------------------------------------
#
# -Zxz, not the host/container's default compressor: debian:11's dpkg (1.20)
# cannot read zstd-compressed control/data members, so a default-built .deb
# fails to install on the floor with "unknown compression for member". dpkg
# itself (and dpkg-deb) ships in every debian base image, so no extra
# package install is needed here.
build_deb() {
  log_stage "build .deb (-Zxz)"
  DEB_PATH="${WORKDIR}/spelunk_${DEB_VERSION}_amd64.deb"
  docker run --rm --name "${CONTAINER_PREFIX}-dpkg-deb" \
    --platform "${DOCKER_PLATFORM}" \
    -v "${REPO_ROOT}:/w" -w /w \
    "${BUILD_IMAGE}" \
    dpkg-deb --build -Zxz "${DEB_LAYOUT}" "${DEB_PATH}" \
    || die "dpkg-deb --build failed"
  [ -f "${DEB_PATH}" ] || die "expected .deb not found at ${DEB_PATH} after dpkg-deb --build"
}

# --- stage 5: install + smoke-test on the floor (and current) images -----
#
# apt-get install succeeds on a .deb whose Depends omits a linked library;
# only executing real subcommands (a git-backed memory round trip included)
# surfaces that gap and proves the installed binary runs, not just links.
# SPELUNK_SECRET_STORE=file keeps the smoke test from touching a keychain
# inside the container. The scratch git repo lives in the container's own
# filesystem, never in this checkout.
smoke_test_deb() {
  for image in ${SMOKE_IMAGES}; do
    log_stage "install + smoke-test .deb (${image})"
    docker run --rm --name "${CONTAINER_PREFIX}-smoke" \
      --platform "${DOCKER_PLATFORM}" \
      -v "${REPO_ROOT}/${DEB_PATH}:/pkg/spelunk_${DEB_VERSION}_amd64.deb:ro" \
      -e SPELUNK_SECRET_STORE=file \
      "${image}" bash -euc '
        set -euo pipefail
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq "/pkg/spelunk_'"${DEB_VERSION}"'_amd64.deb"
        apt-get install -y -qq --no-install-recommends git ca-certificates
        test -n "$(spelunk --version)"
        test -n "$(spelunk-server --version)"

        mkdir -p /w && cd /w
        git init -q .
        git config user.email t@t
        git config user.name t
        echo "fn main() {}" > main.rs
        git add . && git commit -qm init

        spelunk status
        spelunk init --no-index
        spelunk memory add --kind note --title "deb smoke" --body "runs on this image"
        spelunk memory list | grep -q "deb smoke"
      ' || die "install/smoke-test failed on ${image}"
  done
}

# Explicitly does not touch: `gh release create`, any push to the
# `homebrew-spelunk` tap, or a write to `bucket/spelunk.json`. Grep this
# file -- there is no code path above that invokes any of the three.

main() {
  build_in_floor_container
  enforce_glibc_ceiling
  assemble_deb
  build_deb
  smoke_test_deb

  echo ""
  echo "=== release-dry-run: PASS ==="
  echo "Built and smoke-tested: ${DEB_PATH}"
  echo "This proves the Linux x86_64 build, glibc-2.31 floor, and .deb install/smoke are release-safe."
  echo "It does NOT exercise macOS/Windows builds, the GitHub Release, or the Homebrew/Scoop publish steps."
}

main "$@"
