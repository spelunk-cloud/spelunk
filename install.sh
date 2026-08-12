#!/bin/sh
# spelunk is now inkentry.
#
# Usage: curl -fsSL https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh | sh
#        curl -fsSL https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh | sh -s -- --dry-run
#
# This script no longer installs spelunk. It hands over to the inkentry
# migration, which installs inkentry, carries every spelunk memory store across,
# verifies each one, and only then retires spelunk.
#
# Nothing is destroyed on the way: the migration opens each existing .spelunk
# store read-only and never deletes one, and it leaves spelunk installed if any
# store fails to migrate or is declined.
set -e

MIGRATE_URL="https://get.inkentry.com/migrate.sh"

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    *)
      printf 'Unknown flag: %s\n' "$arg" >&2
      exit 1
      ;;
  esac
done

printf '%s\n' "spelunk is now inkentry."
printf '%s\n' ""
printf '%s\n' "This installer now runs the migration instead: it installs inkentry,"
printf '%s\n' "carries your memory across, verifies it, and then retires spelunk."
printf '%s\n' "Your existing .spelunk stores are never modified or deleted."
printf '%s\n' ""

command -v curl > /dev/null 2>&1 || {
  printf 'install.sh: curl is required\n' >&2
  exit 1
}

# `--dry-run` is carried over rather than dropped: it meant "show me what this
# would do" on the installer this replaces, and it means the same thing here.
if [ "$DRY_RUN" -eq 1 ]; then
  printf '%s\n' "Running $MIGRATE_URL (dry run — nothing will be changed)"
  printf '%s\n' ""
  curl -fsSL --proto '=https' "$MIGRATE_URL" | INKENTRY_MIGRATE_DRY_RUN=1 sh
else
  printf '%s\n' "Running $MIGRATE_URL"
  printf '%s\n' ""
  curl -fsSL --proto '=https' "$MIGRATE_URL" | sh
fi
