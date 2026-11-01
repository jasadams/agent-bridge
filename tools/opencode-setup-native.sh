#!/bin/sh
# OpenCode V2.2.5 installed assets; only disposable private state/configuration.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec bun "$root/plugins/opencode/setup-native.ts" "${1:-$root/target/debug/agent-bridge}" "${2:-opencode}"
