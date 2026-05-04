#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

: "${MUJINA_TANG_NANO_9K_PORT:=/dev/cu.usbserial-1101}"
: "${MUJINA_TANG_NANO_9K_BAUD:=115200}"
: "${MUJINA_USB_DISABLE:=1}"
: "${RUST_LOG:=mujina_miner=debug}"

export MUJINA_TANG_NANO_9K_PORT
export MUJINA_TANG_NANO_9K_BAUD
export MUJINA_USB_DISABLE
export RUST_LOG

echo "Starting Mujina Tang Nano 9K FPGA miner"
echo "  MUJINA_TANG_NANO_9K_PORT=${MUJINA_TANG_NANO_9K_PORT}"
echo "  MUJINA_TANG_NANO_9K_BAUD=${MUJINA_TANG_NANO_9K_BAUD}"
echo "  MUJINA_USB_DISABLE=${MUJINA_USB_DISABLE}"
echo "  RUST_LOG=${RUST_LOG}"

cd "${REPO_ROOT}"
exec cargo run -p mujina-miner --bin mujina-minerd
