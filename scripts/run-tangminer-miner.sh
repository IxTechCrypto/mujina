#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

: "${MUJINA_TANG_NANO_PORT:=${MUJINA_TANG_NANO_9K_PORT:-/dev/cu.usbserial-20250303171}}"
: "${MUJINA_TANG_NANO_BAUD:=${MUJINA_TANG_NANO_9K_BAUD:-115200}}"
: "${MUJINA_USB_DISABLE:=1}"
: "${MUJINA_POOL_URL:=stratum+tcp://pool.256foundation.org:3333}"
: "${MUJINA_POOL_USER:=npub1ql2zzp3g6yndgz05js7wdc4qkr88wkyne5nw2cc7csrtzqs0yeesgwrxya.tangminer}"
: "${RUST_LOG:=mujina_miner=debug}"

MUJINA_TANG_NANO_9K_PORT="${MUJINA_TANG_NANO_PORT}"
MUJINA_TANG_NANO_9K_BAUD="${MUJINA_TANG_NANO_BAUD}"
export MUJINA_TANG_NANO_PORT
export MUJINA_TANG_NANO_BAUD
export MUJINA_TANG_NANO_9K_PORT
export MUJINA_TANG_NANO_9K_BAUD
export MUJINA_USB_DISABLE
export MUJINA_POOL_URL
export MUJINA_POOL_USER
export RUST_LOG

echo "Starting Mujina Tang Nano FPGA miner"
echo "  MUJINA_TANG_NANO_PORT=${MUJINA_TANG_NANO_PORT}"
echo "  MUJINA_TANG_NANO_BAUD=${MUJINA_TANG_NANO_BAUD}"
echo "  MUJINA_USB_DISABLE=${MUJINA_USB_DISABLE}"
echo "  MUJINA_POOL_URL=${MUJINA_POOL_URL}"
echo "  MUJINA_POOL_USER=${MUJINA_POOL_USER}"
echo "  RUST_LOG=${RUST_LOG}"

cd "${REPO_ROOT}"
exec cargo run -p mujina-miner --bin mujina-minerd
