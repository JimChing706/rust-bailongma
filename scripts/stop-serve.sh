#!/usr/bin/env bash
# Stop the bailongma API server.
#
# Strategy:
#   1. kill the PID recorded in logs/serve.pid (if still alive), wait for exit
#   2. fallback: stop whatever listens on the API port (fuser)
#   3. delete the PID file only after the port is actually free
#
# Usage: ./scripts/stop-serve.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIDF="$ROOT/logs/serve.pid"
PORT=3721

PID=""
[ -f "$PIDF" ] && PID="$(tr -d '[:space:]' < "$PIDF")"

stopped=""

# 1) graceful kill by PID file
if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
  kill "$PID" 2>/dev/null || true
  for _ in $(seq 1 10); do
    kill -0 "$PID" 2>/dev/null || { stopped=1; break; }
    sleep 0.5
  done
  if [ -z "$stopped" ]; then
    echo "Process did not exit within 5s (PID=$PID), forcing..."
    kill -9 "$PID" 2>/dev/null || true
    sleep 1
    kill -0 "$PID" 2>/dev/null || stopped=1
  fi
fi

# 2) port-based fallback (process gone, or caller lacks permission to signal it)
if [ -z "$stopped" ]; then
  if command -v fuser >/dev/null 2>&1 && fuser "$PORT/tcp" >/dev/null 2>&1; then
    if fuser -k "$PORT/tcp" >/dev/null 2>&1; then
      sleep 1
      fuser "$PORT/tcp" >/dev/null 2>&1 || stopped=1
      [ -n "$stopped" ] && echo "Stopped serve via port $PORT"
    fi
  fi
fi

if [ -n "$stopped" ]; then
  echo "Stopped serve (PID=${PID:-by-port})"
elif [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
  echo "Failed to stop serve (PID=$PID): permission denied? try: sudo $0"
elif fuser "$PORT/tcp" >/dev/null 2>&1; then
  echo "Port $PORT still in use; stop it manually, e.g.: sudo fuser -k $PORT/tcp"
else
  echo "No serve process found (nothing to stop)"
fi

# only clean up the PID file once the port is free
if [ -z "$PID" ] || [ -z "$(fuser "$PORT/tcp" 2>/dev/null)" ]; then
  rm -f "$PIDF"
fi
