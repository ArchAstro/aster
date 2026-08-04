#!/bin/sh
set -eu

screenshot_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$screenshot_dir/../.." && pwd)
fixture_workspace=$(mktemp -d "${TMPDIR:-/tmp}/firstlanding.XXXXXX")

cleanup() {
  rm -rf "$fixture_workspace"
}
trap cleanup EXIT HUP INT TERM

cp -R "$screenshot_dir/workspace/." "$fixture_workspace/"
cd "$fixture_workspace"

if [ "${1:-}" = "services" ]; then
  "$repository_root/target/debug/aster" "$@"
else
  CLICOLOR_FORCE=1 "$repository_root/target/debug/aster" "$@" 2>&1 | cat
fi
