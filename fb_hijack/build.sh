#!/bin/bash
set -e
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=/home/grant/nano3s-build/rv64gc-gcc
export RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=generic-rv64 -L /home/grant/riscv64-sysroot/usr/lib/riscv64-linux-gnu"
cd "$(dirname "$0")"
cargo build --release --target riscv64gc-unknown-linux-gnu --bin fb_button
BIN="target/riscv64gc-unknown-linux-gnu/release/fb_button"
riscv64-linux-gnu-strip --strip-all "$BIN"
ls -lh "$BIN"
echo "[+] Built $BIN"
