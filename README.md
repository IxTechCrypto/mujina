# mujina-miner

Open-source Bitcoin mining firmware.

## ⚡ IxTech Distribution — Repository Updates & Windows Bring-Up

This repository ([`IxTechCrypto/mujina`](https://github.com/IxTechCrypto/mujina)) is an active, production-hardened fork of [256foundation/mujina](https://github.com/256foundation/mujina) engineered for enterprise stability, native Windows USB CDC-ACM support, multi-chip ASIC bring-up, real-time fleet telemetry, and automated fan/frequency autotuning.

### 🚀 Major Improvements & Features in this Fork

* **🪟 Native Windows Transport & CDC-ACM Hardening**:
  * **Custom Win32 Serial Driver (`src/transport/serial_windows.rs`)**: Complete native Windows `SerialStream` implementation utilizing Win32 overlapped asynchronous I/O and custom byte ringbuffers, eliminating external Unix serial dependencies.
  * **0-Byte EOF Prevention**: Intercepts 0-byte `ReadFile` in `SerialReader::poll_read` on Windows to return `Poll::Pending` with waker re-poll instead of premature EOF, permanently resolving `Control stream closed` errors during board initialization.
  * **COM Port Lifecycle Resiliency**: DTR/RTS auto-reset pulse handling, port settling delay, and COM handle preservation preventing USB disconnects.
  * **Detached Console Protection**: Graceful handling of `tokio::signal::windows::ctrl_c` disconnections so the daemon stays continuously hashing in background/headless runs.

* **⛏️ Multi-Chip Hardware Bring-Up & Fleet Expansion**:
  * **NerdQAxe++ (BM1366 4-Chip Chain)**: Full board driver (`src/board/nerdqaxe_pp.rs`), multi-chip PLL and chain enumeration, TI TPS53647 multi-phase core buck regulator driver, and Microchip EMC2302 dual-fan controller over I2C/PMBus.
  * **Bitaxe Gamma Multi-Device Fleet Support**: Concurrently run multiple Bitaxe Gammas (BM1370) on Windows over native USB CDC-ACM without port collisions or key clashes (~1.7+ TH/s verified aggregate).
  * **Per-Chip Hashrate & Thermal Telemetry**: Reports individual chip frequencies, core voltages, and temperatures in JSON REST APIs.

* **🧠 Advanced Auto-Tuning Supervisor & Intelligent Cooling**:
  * **AxeOS-Style Fan Controller**: Closed-loop PI fan controller with EMA smoothing to eliminate acoustic oscillation while locking target ASIC junction temperatures.
  * **Adaptive Autotuner (`src/autotune.rs`)**: Dynamic frequency and core voltage tuning engine with multiple operational profiles (`Efficiency`, `Balanced`, `Max Hash`, `Target Power/Hashrate`).
  * **Chip-Count Scaled Power Capping**: Dynamic power cap calculations that scale by active ASIC count to prevent false trip-offs on multi-chip boards.
  * **Profile Persistence**: State-saving to `mujina-autotune.json` with fallback mechanisms and delta updates.

* **📊 Cyberpunk Fleet HUD & Real-Time Telemetry Dashboard**:
  * **Interactive Web Interface**: Live responsive web dashboard (`dashboard.html` / `dashboard.py`) hosted on port 8088.
  * **Sub-50ms Delta Updates**: Decoupled control modals submit only modified settings, eliminating board timeout errors.
  * **Memory-Capped Log Viewer**: Real-time log streaming capped at 500 DOM elements to prevent browser lag during long runs.
  * **Promoted Mining Logs**: Elevated share submission, pool acceptance, and pool difficulty updates to `INFO` level for transparent live monitoring.

* **🍓 Raspberry Pi 4 & Embedded Linux Deployment**:
  * Complete headless deployment configuration for Raspberry Pi 4 (Debian aarch64) with native systemd service daemons (`mujina-miner`, `mujina-dashboard`).
  * **Waveshare 2.13" V4 E-Paper HUD**: Live hardware status monitor (`scripts/waveshare_epaper_monitor.py`) displaying real-time hashrate, temperatures, pool latency, and network IP.

* **🎬 Launch Media & Cross-Platform Distribution (`brag-output/`)**:
  * High-production 1080p and 9:16 vertical launch videos, visual storyboards, poster frames, and ready-to-publish social media copy (`social-posts.md`) formatted for Twitter/X, Facebook, TikTok, and YouTube Shorts.

## Why Mujina

You bought the hardware, but someone else controls the software. Whether
you have thousands of machines in a data center or one in your basement,
the firmware running them is closed. It comes from the manufacturer or a
third-party vendor, and you can't read it, audit it, or change it.

Mujina is here to change that: one open-source codebase to run any
hashboard from any vendor on any control board, written by hardware
engineers, protocol authors, and mining operators from across the industry.
Read every line, modify it without permission, control it through a
documented API, and pay no dev fee. Own your firmware.

## Current Status

Mujina is under active development. Today's supported hardware is a
starting point:

**Working now**

- **[Bitaxe Gamma](mujina-miner/src/board/bitaxe_gamma.md)** (single
  BM1370 ASIC): an open-source single-chip miner. Good for developers
  and advanced users who want to run Mujina on real hardware today.
- **CPU backend**: software SHA-256 hashing, no hardware required.
  Useful for exercising Mujina itself, testing pool and other server
  software against a working miner client, and teaching the full mining
  pipeline. See [CPU Mining](docs/cpu-mining.md).

**Landing now**

- **[EmberOne00](https://github.com/256foundation/emberone00-pcb)**
  (twelve BM1362 ASICs): a sister project from the 256 Foundation. An
  open-source hashboard designed to be driven by open firmware.

**Near-term targets**

- Installable images for the Antminer S19 series
- The 256 Foundation's forthcoming
  [Libreboard](https://github.com/256foundation/libreboard) control
  board
- Broader support for commercial mining machines

APIs are still moving and parts of the docs lag the code.

## Quick Start

Build Mujina and watch it run end to end, no mining hardware required.
On Debian or Ubuntu:

```bash
git clone https://github.com/256foundation/mujina.git
cd mujina
sudo apt-get install libudev-dev libssl-dev
MUJINA_CPUMINER_THREADS=1 MUJINA_CPUMINER_DUTY=50 MUJINA_USB_DISABLE=1 \
  cargo run --bin mujina-minerd
```

In this example, the CPU backend hashes in software against a dummy job
source, exercising the full pipeline: job distribution, hashing, share
detection, logging, and the API. When you're ready to mine for real,
continue below.

## Build Requirements

Mujina builds with the current stable
[Rust toolchain](https://rustup.rs). Install the additional packages
below for your platform.

### Linux

On Debian or Ubuntu:

```bash
sudo apt-get install libudev-dev libssl-dev
```

Other distributions need their equivalents of the udev and openssl
development packages.

### macOS

macOS is supported. Install Xcode Command Line Tools alongside the Rust
toolchain. A build failure on `openssl-sys` usually means the build
can't find openssl; see the
[openssl crate's macOS notes](https://docs.rs/openssl/latest/openssl/#automatic)
for the supported installation and environment options.

### Windows

Windows is fully supported.

- **Pre-built binary (No compilation required)**: Download or locate `mujina-minerd.exe` in the repository root and run `.\start-all.ps1` in PowerShell to launch both the miner daemon and web dashboard. No Rust toolchain or Visual Studio setup is required.
- **Building from source**: Install the [Rust toolchain](https://rustup.rs) via `rustup-init.exe` (`x86_64-pc-windows-msvc`) and **Build Tools for Visual Studio** (with the "Desktop development with C++" workload). Python 3 is required for the web dashboard.

See the detailed [Windows Setup Guide](docs/windows-setup.md) for full COM port drivers, gotchas, pre-built launcher usage, and PowerShell scripts.

## Building

mujina-miner is a cargo workspace. Build and test it the usual way:

```bash
cargo build
cargo test
```

The workspace contains several binaries: `mujina-minerd` (the daemon),
`mujina-cli`, and others. Running requires picking one:

```bash
cargo run --bin mujina-minerd
```

If you'll be working in the repo regularly, install
[just](https://github.com/casey/just) (`cargo install just`) for
shorter aliases that avoid retyping the `--bin` flag:

```bash
just run        # same as cargo run --bin mujina-minerd
just test       # same as cargo test
just checks     # fmt, lint, and test in one step
```

Examples in the rest of this README use plain cargo so they work
without `just` installed.

## Running

Mujina is currently configured through environment variables.
Persistent configuration via the REST API and CLI will follow as those
interfaces mature.

### Connecting to a job source

Point Mujina at a Stratum v1 mining pool:

```bash
MUJINA_POOL_URL="stratum+tcp://pool.example.com:3333" \
MUJINA_POOL_USER="your-address.worker" \
cargo run --bin mujina-minerd
```

`MUJINA_POOL_USER` defaults to `mujina-testing` and `MUJINA_POOL_PASS`
defaults to `x`, so only `MUJINA_POOL_URL` is strictly required.

### Testing without a pool

Omit `MUJINA_POOL_URL` to use a dummy job source that generates
synthetic mining work. Useful for development without a network
connection.

```bash
cargo run --bin mujina-minerd
```

### Controlling log output

The default filter emits Mujina log entries at info level and
third-party crates at warn. Two environment variables adjust it.
`MUJINA_LOG` filters Mujina's own modules, named exactly as the log
output shows them, and a bare level applies to Mujina as a whole.
`RUST_LOG` keeps its usual Rust meaning: a directive that names a
crate adds to the defaults, and a bare level takes full control of
the filter. `MUJINA_LOG` wins where the two overlap.

```bash
# Default: info for Mujina, warn for third-party crates
cargo run --bin mujina-minerd

# Trace all of Mujina, third-party crates stay at warn
MUJINA_LOG=trace cargo run --bin mujina-minerd

# Trace the Stratum v1 client, everything else unchanged
MUJINA_LOG=stratum_v1=trace cargo run --bin mujina-minerd

# Debug Stratum v1 and trace BM13xx at the same time
MUJINA_LOG=stratum_v1=debug,asic::bm13xx=trace \
  cargo run --bin mujina-minerd

# Trace a third-party crate, Mujina's defaults unchanged
RUST_LOG=nusb=trace cargo run --bin mujina-minerd
```

Debug shows logical stages and summaries: chip initialization, jobs
received from the pool, shares submitted. Trace adds step-by-step
execution detail: individual serial frames, I2C transactions, and USB
device events.

### Using the REST API

Mujina logs the API bind address at startup. By default it's
`127.0.0.1:7785`; set `MUJINA_API_LISTEN` to change it:

```bash
# All interfaces, default port
MUJINA_API_LISTEN="0.0.0.0" cargo run --bin mujina-minerd

# All interfaces, custom port
MUJINA_API_LISTEN="0.0.0.0:9000" cargo run --bin mujina-minerd
```

See [REST API](docs/api.md) for endpoints and conventions. The
`/api/v0/` prefix signals the API is still in flux. Authentication
is on the roadmap.

## Contributing

We welcome contributions! Whether you're fixing bugs, adding features,
improving documentation, or simply exploring the codebase to learn
about Bitcoin mining protocols and hardware, your involvement is
valued.

Please see our [Contribution Guide](CONTRIBUTING.md) for details on
how to get started. For user-oriented discussion and support, visit
the [Mujina forum](https://forum.256foundation.org/c/mujina/7). For
real-time chat, join our [Telegram group](https://t.me/the256foundation)---note
that Telegram is ephemeral; decisions and important context belong on
GitHub.

## Further Reading

### Design and operation

- [Architecture Overview](docs/architecture.md): system design and
  component interaction
- [REST API](docs/api.md): endpoints, conventions, and OpenAPI spec
- [CPU Mining](docs/cpu-mining.md): the CPU backend in detail
- [Windows Setup Guide](docs/windows-setup.md): Windows build instructions,
  COM port drivers, gotchas, and PowerShell scripts
- [Raspberry Pi Setup Guide](docs/raspberry-pi-setup.md): Raspberry Pi
  deployment, systemd auto-start, and Waveshare e-paper setup
- [Container Image](docs/container.md): build and run Mujina as a
  container

### Protocols

- [BM13xx Chip Reference](mujina-miner/src/asic/bm13xx/REFERENCE.md):
  serial protocol, registers, and behavior of the BM13xx mining-chip
  family
- [Bitaxe-Raw Control Protocol](mujina-miner/src/mgmt_protocol/bitaxe_raw/PROTOCOL.md):
  management protocol for Bitaxe board peripherals

### Hardware

- [Bitaxe Gamma Board Guide](mujina-miner/src/board/bitaxe_gamma.md):
  board hardware, firmware flashing, and Mujina integration

### Contributor reference

- [Contribution Guide](CONTRIBUTING.md): process and requirements
- [Code Style Guide](CODE_STYLE.md): formatting and mechanical style
- [Coding Guidelines](CODING_GUIDELINES.md): design patterns and best
  practices

## Related Projects

- [Bitaxe](https://github.com/bitaxeorg): open-source Bitcoin mining
  hardware
- [bitaxe-raw](https://github.com/bitaxeorg/bitaxe-raw): pass-through firmware for
  Bitaxe boards required for use by Mujina
- [EmberOne00](https://github.com/256foundation/emberone00-pcb): 256
  Foundation's first open-source Bitcoin mining hashboard
- [Libreboard](https://github.com/256foundation/libreboard): 256
  Foundation's open-source mining control board

## License

This project is licensed under the GNU General Public License v3.0 or
later. See the [LICENSE](LICENSE) file for details.
