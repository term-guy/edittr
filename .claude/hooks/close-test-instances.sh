#!/bin/bash
# Keeps at most one edittr window open during local testing.
#   close-test-instances.sh pre   PreToolUse(Bash): if the command launches edittr, close running test instances first
#   close-test-instances.sh stop  Stop: close any test instance left running
# Only matches builds in this repo's target/ and app bundles in Claude's temp dirs;
# copies the user launched themselves (e.g. /Applications/edittr.app) are never touched.
set -u
proj="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
pattern="^((${proj}/)?target/[a-z]+/edittr|/(private/)?tmp/claude-[^ ]*/edittr\.app/Contents/MacOS/edittr)( |$)"

if [ "${1:-}" = "pre" ]; then
  cmd=$(jq -r '.tool_input.command // empty')
  case "$cmd" in
    *target/release/edittr*|*target/debug/edittr*|*edittr.app*|*"cargo run"*) ;;
    *) exit 0 ;;
  esac
fi

if pgrep -f "$pattern" >/dev/null; then
  pkill -f "$pattern"
  sleep 0.3
fi
exit 0
