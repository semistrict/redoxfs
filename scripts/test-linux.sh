#!/usr/bin/env bash
# Run the test suite on Linux inside a Lima VM, covering what macOS cannot:
# FUSE mounts and the memfd-backed page cache.
#
# Lima mounts the host home read-only, so the repo is copied into the VM.
# Arguments are passed to `cargo nextest run`.
set -euo pipefail

instance="${REDOXFS_LIMA_INSTANCE:-default}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
args=$(printf ' %q' "$@")

limactl shell "$instance" -- bash -lc "
    set -euo pipefail
    rsync -a --delete --exclude target --exclude .git '$repo/' ~/redoxfs-test/
    cd ~/redoxfs-test
    cargo nextest run$args
"
