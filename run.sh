#!/usr/bin/env bash
# Convenience launcher.
#
# Usage: ./run.sh [URL]
#
# - builds in release if the binary is missing or stale
# - copies the binary to local disk before launching (a debug build is ~500 MB;
#   running it from a slow mount makes startup crawl because of demand paging)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="$here/target/release/stream-player"
local_bin="${XDG_RUNTIME_DIR:-/tmp}/stream-player"

if [[ ! -x "$bin" ]] || [[ -n "$(find "$here/src" "$here/Cargo.toml" -newer "$bin" -print -quit 2>/dev/null)" ]]; then
    echo "building…" >&2
    ( cd "$here" && cargo build --release )
fi

install -m 755 "$bin" "$local_bin"
exec "$local_bin" "$@"
