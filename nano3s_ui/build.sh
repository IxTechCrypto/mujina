#!/bin/bash
set -e
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=/home/grant/nano3s-build/rv64gc-gcc
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=/home/grant/riscv64-sysroot
export PKG_CONFIG_PATH=/home/grant/riscv64-sysroot/usr/lib/riscv64-linux-gnu/pkgconfig
export RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=generic-rv64 -L /home/grant/riscv64-sysroot/usr/lib/riscv64-linux-gnu"
export CFLAGS="-march=rv64gc -mabi=lp64d"

cd "$(dirname "$0")"
cargo build --release --target riscv64gc-unknown-linux-gnu
BIN="target/riscv64gc-unknown-linux-gnu/release/nano3s_ui"
riscv64-linux-gnu-strip --strip-all "$BIN"
ls -lh "$BIN"
echo "[+] Built $BIN"
