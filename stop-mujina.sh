#!/bin/bash
# Gracefully stop the Mujina miner daemon.
# Usage:  bash stop-mujina.sh

pid=$(pgrep -x "mujina-minerd")
if [ -z "$pid" ]; then
    echo "Mujina is not running."
    exit 0
fi

# Send SIGINT (equivalent to Ctrl-C / Interrupt) to trigger clean port & ASIC shutdown
echo "Sending SIGINT (Ctrl-C) to Mujina (PID $pid) for graceful exit..."
kill -INT "$pid"

# Poll for up to 10 seconds to check if process exited cleanly
for i in {1..10}; do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "Mujina stopped gracefully."
        exit 0
    fi
    sleep 1
done

# Fallback: Force-kill if graceful shutdown timed out
echo "Graceful shutdown timed out. Falling back to force-kill (SIGKILL)..."
kill -KILL "$pid"
echo "Force-stopped Mujina (PID $pid)."

# --- Stop E-Paper Monitor & Web Dashboard ---
epaper_pid=$(pgrep -f "python3.*mujina_epaper.py")
if [ -n "$epaper_pid" ]; then
    echo "Stopping E-Paper Display Monitor (PID $epaper_pid)..."
    kill "$epaper_pid" 2>/dev/null
fi

dash_pid=$(pgrep -f "python3.*dashboard.py")
if [ -n "$dash_pid" ]; then
    echo "Stopping Web Dashboard Server (PID $dash_pid)..."
    kill "$dash_pid" 2>/dev/null
fi

