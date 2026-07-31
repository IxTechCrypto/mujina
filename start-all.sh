#!/bin/bash
# Start both the Mujina miner daemon and its web dashboard server in one command.
# Usage:  bash start-all.sh

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

echo "=== Mujina Rig Bootstrapping ==="

# 1. Start the miner daemon
bash "$here/start-mujina.sh"
sleep 2

# 2. Start the dashboard server
if pgrep -f "python3.*dashboard.py" >/dev/null; then
    echo "Web dashboard server is already running."
else
    echo "Starting Web Dashboard Server..."
    # If running on Port 80, we need sudo. Since start-all is run,
    # let's run dashboard.py in background redirecting output.
    nohup python3 "$here/dashboard.py" > "$here/dashboard.log" 2> "$here/dashboard.log.err" &
    dashboard_pid=$!
    echo "Dashboard started in background (PID $dashboard_pid)."
fi

echo "================================="
echo "All systems ready."
