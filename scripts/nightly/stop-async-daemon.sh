#!/usr/bin/env bash
# Gracefully stop the autter async daemon started by start-async-daemon.sh.
#
# Usage:  bash scripts/nightly/stop-async-daemon.sh [autter-binary]
#
# Reads ASYNC_DAEMON_PID, AUTTER_DAEMON_HOME, and socket paths from env.
# Falls back to kill -9 if graceful shutdown times out.
set -uo pipefail

AUTTER_BIN="${1:-}"

if [ -z "${ASYNC_DAEMON_PID:-}" ]; then
    echo "No ASYNC_DAEMON_PID set — nothing to stop."
    exit 0
fi

# Try graceful shutdown via the control socket.
if [ -n "$AUTTER_BIN" ] && [ -S "${AUTTER_DAEMON_CONTROL_SOCKET:-}" ]; then
    "$AUTTER_BIN" bg shutdown 2>/dev/null || true
fi

# Wait up to 2 s for the process to exit.
for _i in $(seq 1 40); do
    kill -0 "$ASYNC_DAEMON_PID" 2>/dev/null || break
    sleep 0.05
done

# Force-kill if still alive.
kill -9 "$ASYNC_DAEMON_PID" 2>/dev/null || true
wait "$ASYNC_DAEMON_PID" 2>/dev/null || true

# Clean up daemon home.
if [ -n "${AUTTER_DAEMON_HOME:-}" ] && [ -d "$AUTTER_DAEMON_HOME" ]; then
    rm -rf "$AUTTER_DAEMON_HOME"
fi

echo "Async daemon stopped (PID=$ASYNC_DAEMON_PID)."
