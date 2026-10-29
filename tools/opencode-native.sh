#!/bin/sh
# Run only against a locally built executable; all native state is disposable.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec bun "$root/plugins/opencode/native.ts" "${1:-$root/target/debug/agent-bridge}" "${2:-opencode}"
