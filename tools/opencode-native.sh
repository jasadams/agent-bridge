#!/bin/sh
# OpenCode V2.2.5 only; locally built broker and isolated disposable native state.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec bun "$root/plugins/opencode/native.ts" "${1:-$root/target/debug/agent-bridge}" "${2:-opencode}"
