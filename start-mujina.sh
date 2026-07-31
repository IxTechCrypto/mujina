#!/bin/bash
# Start Mujina miner daemon on Raspberry Pi.
# Usage:  bash start-mujina.sh

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
exe="$here/mujina-minerd"
log="$here/mujina.log"
err_log="$here/mujina.log.err"

# Fallback: check target/release/ if not in the current directory
if [ ! -f "$exe" ]; then
    exe="$here/target/release/mujina-minerd"
fi

if [ ! -f "$exe" ]; then
    echo "Error: mujina-minerd binary not found at $here/mujina-minerd or $here/target/release/mujina-minerd"
    exit 1
fi

# If already running, stop it gracefully first
if pgrep -x "mujina-minerd" >/dev/null; then
    echo "Mujina is already running. Stopping it gracefully first..."
    bash "$here/stop-mujina.sh"
fi

# --- API / state configuration ---
export MUJINA_API_LISTEN="0.0.0.0:7785"
export RUST_LOG="warn,mujina_miner=info"
export MUJINA_STATE_DIR="$here"

# --- Pool Seed Configuration ---
# The pool configuration in mujina-settings.json takes precedence over these.
settings="$here/mujina-settings.json"
if [ ! -f "$settings" ]; then
    export MUJINA_POOL_URL="stratum+tcp://parasite.wtf:42069"
    export MUJINA_POOL_USER="bc1qa70cqk8hl3jg6hgqlts66g2fqazpwspn3wrn80.Mujina_gamma"
    export MUJINA_POOL_PASS="x"
    pool_desc="$MUJINA_POOL_URL (seeded from this script)"
else
    pool_desc="from $settings"
fi

# Start daemon in background (nohup prevents termination on shell exit)
nohup "$exe" > "$log" 2> "$err_log" &
proc_pid=$!

echo "Started Mujina (PID $proc_pid)."
echo "  Pool: $pool_desc"
echo "  API : http://0.0.0.0:7785/api/v0/miner"

# --- Start E-Paper Monitor ---
if pgrep -f "python3.*mujina_epaper.py" >/dev/null; then
    echo "E-paper monitor is already running."
else
    # Check if waveshare library is linked in our script folder
    if [ -d "$here/scripts/epaper/waveshare_epd" ]; then
        echo "Starting E-Paper Display Monitor..."
        nohup python3 "$here/scripts/epaper/mujina_epaper.py" > "$here/epaper.log" 2> "$here/epaper.log.err" &
    else
        echo "Note: Waveshare drivers not linked at $here/scripts/epaper/waveshare_epd yet. E-paper monitor startup skipped."
    fi
fi
