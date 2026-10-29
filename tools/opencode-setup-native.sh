#!/bin/sh
# Install and exercise native integration using only disposable private state.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec bun "$root/plugins/opencode/setup-native.ts" "${1:-$root/target/debug/agent-bridge}" "${2:-opencode}"
