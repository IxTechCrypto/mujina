# NerdQAxe++ support + Linux/Raspberry Pi build

Design notes for two related efforts:

1. Running the existing feature work (fan control, auto-tuning, dashboard)
   on Linux, targeting a Raspberry Pi 4 driving up to four NerdQAxe++.
2. Adding NerdQAxe++ as a supported board, reusing the bitaxe-raw pattern
   already used for the Bitaxe Gamma and emberOne/00.

## Hardware summary (NerdQAxe++)

Established from the `shufps/qaxe` schematic (`nerdqaxe++/nerdqaxe++/*.kicad_sch`)
and BOM. KiCad symbols are named `bm1366`/`BM1368` for legacy reasons; the
actual silicon is BM1370.

| Item | Part | Notes |
|------|------|-------|
| ASIC | 4× BM1370 | Same chip as Bitaxe Gamma. Daisy-chained UART (`RO→RI`), reset (`NRSTO→NRSTI`), clock (`CO→CI`). |
| Host MCU | ESP32-S3 (LILYGO T-Display S3) | Same family as the Bitaxe. Drives UART + I2C + GPIO. |
| Core power | TI TPS53647, 4-phase buck | Single core rail feeding the 4 chips as a series/stacked voltage-domain string. **No existing Mujina driver.** |
| Aux rails | MCP1824T-0802E (0.8 V), MCP1824T-1802E (1.8 V) | GPIO-enabled via `LDO_EN`. No driver needed. |
| Fan | Microchip EMC2302 (2-channel) | **Not** EMC2101. Needs a new/adapted driver. |
| Temp | 2× TMP1075 (U37, U38) | Existing `peripheral/tmp1075.rs` (emberOne uses it). |

### ESP32-S3 pin map (from `pi.kicad_sch`)

Verified by parsing the raw KiCad s-expression and tracing nets (union-find
over wires, bridging series passives). The T-Display-S3 module is `U30`.

Complete and verified — every net traced to a `U30` pin:

| Net | GPIO | Pin | Direction | Notes |
|-----|------|-----|-----------|-------|
| TXD (UART to ASIC) | **GPIO17** | 19 | out | |
| RXD (UART from ASIC) | **GPIO18** | 20 | in | |
| RESET (ASIC nRST) | **GPIO1** | 2 | out | 10k pull-up R16 |
| SDA (I2C) | **GPIO44** | 21 | bidir | 3.3k pull-up R9 |
| SCL (I2C) | **GPIO43** | 22 | out | 3.3k pull-up R8 |
| PWR_EN | **GPIO10** | 5 | out | |
| VR_RDY | **GPIO11** | 6 | in | 10k pull-up R36 |
| LDO_EN | **GPIO13** | 8 | out | |

> **Two traps this table avoids.** (a) An LLM-summarized read of the same
> schematic claimed PWR_EN=GPIO2, LDO_EN=GPIO11, SDA=GPIO21, SCL=GPIO22 —
> wrong on all four. (b) GPIO43/44 are UART0 TX/RX on a stock LILYGO
> T-Display S3, so the natural assumption is TXD=43/RXD=44. On this board
> 43/44 are **I2C**, and the ASIC UART is on 17/18. Flashing on the stock-pinout
> assumption would cross the UART and I2C buses.

Method: parsed the raw KiCad s-expression, union-find over `wire` segments,
symbol pins placed from the `lib_symbols` template. Coordinates match to
0.01 mm, so compare pin/label positions with a tolerance rather than exact
equality.

`U33` (TXU0102 2-bit level shifter) sits by the fans and shifts the **tacho**
lines — it is not in the ASIC UART or I2C path.

### Other pi-sheet findings

The EMC2302 (`U32`) and both TMP1075 (`U37`, `U38`) live on the *pi* sheet, not
the power sheet. The board has **two fans** (`M1`, `M2`, 4-pin), with
`TACHO1` → `U32.TACH1` and `TACHO2` → `M2.Tacho` — which is why the fan
controller is a 2-channel EMC2302 rather than the Bitaxe's single-channel
EMC2101. The board file must expose two fans in telemetry.

## Track 1 — feature work on Linux / Raspberry Pi 4

### Finding: no port needed

`main` is already Linux-native (`nusb` + `udev`, `transport/usb/linux.rs`).
The feature branch `claude/autotuner-target-mode-478415` (fan control,
auto-tune supervisor, target mode, per-chip limits) is **already
cross-platform**: the Windows work was purely additive.

- `linux.rs`, `macos.rs`, `windows.rs` all coexist, selected by
  `#[cfg(target_os = ...)]`.
- The `usb.rs` change only adds a `#[cfg(target_os = "windows")]` block.
- `Cargo.toml` moves `nix`/`rustix` to `[target.'cfg(unix)']`; Windows deps
  are under `[target.'cfg(windows)']`.
- The only other Windows code is two gated `#[cfg(windows)]` blocks in
  `daemon.rs` and `transport/mod.rs`.

So the same branch compiles on Linux/aarch64 as-is. A separate "Linux branch"
would be byte-for-byte identical except for files that already compile out on
the wrong OS. Recommendation: keep one canonical branch, not a Linux/Windows
pair.

### Steps

1. Cut a canonical feature branch from `claude/autotuner-target-mode-478415`.
2. On the Pi 4 (64-bit OS): install Rust, `libudev-dev`, `libssl-dev`, then
   `cargo build --release` natively.
3. Add udev rules so the CDC serial nodes are openable without root.
4. Dashboard: it is not in git (a separate static frontend hitting the API on
   `:7785`). Both platforms share it automatically. Decide whether to serve it
   from the Pi or a separate host.

Open: confirm a clean Linux compile (can't be fully proven from a Windows dev
box; the Pi build is the confirmation).

## Track 2 — NerdQAxe++ as a supported board

### Architecture

Same as Bitaxe/emberOne: the host (Pi) runs Mujina and drives the ASICs
directly. The ESP32-S3 runs bitaxe-raw as a dumb bridge exposing GPIO, I2C,
and a raw UART data port over USB CDC. All chip- and peripheral-specific logic
lives on the host.

### Reuse map

- BM1370 protocol: `asic/bm13xx` — reuse.
- TMP1075: `peripheral/tmp1075.rs` — reuse.
- MCP1824 LDOs: GPIO enable only (like emberOne `VDDIO_EN`) — no driver.
- Board scaffold: copy-adapt `board/bitaxe.rs`; `board/emberone00.rs` is the
  reference for the bitaxe-raw + multi-chip pattern.

### New work

**1. Firmware (bitaxe-raw fork).** bitaxe-raw is Rust and chip-agnostic, so the
port is mostly a pin remap to the table above plus USB manufacturer/product
strings that Mujina's board matcher keys on. The TPS53647/EMC2302 drivers do
**not** go in firmware — the bridge only moves I2C/GPIO/UART bytes.

**2. `peripheral/tps53647.rs`** — device layer for the 4-phase core buck.
**Not a from-scratch driver.** `peripheral/pmbus.rs` (1196 lines) already
provides the generic layer: Linear11/Linear16 codecs, voltage/current/
temperature/frequency types, `StatusWord` decoding, and 69 standard PMBus
commands including `VoutCommand` (0x21), `ReadVin` (0x88), `ReadIout` (0x8C),
`ReadTemperature1` (0x8D), `ClearFaults` (0x03) and — relevant for a
multiphase part — `Phase` (0x04). The TPS53647 is a standard PMBus device, so
this is a config struct + init sequence over the existing layer, mirroring the
shape of `tps546.rs`. Reuse `pmbus.rs`; do not re-derive encodings. Pull exact
config (PMBus address, sense resistors, voltage limits) from `power.kicad_sch`
during implementation.

> **Deferrable.** For first bring-up (enumerate the chain, confirm 4 chips),
> the core rail comes up on its hardware default via `PWR_EN`/`LDO_EN` GPIO —
> which is how the stock firmware boots too. So chip discovery needs no PMBus
> driver at all. Defer this until after first light; it is required before
> *sustained hashing*, not before enumeration.

**3. EMC2302 fan control** — the board has two fans (`M1`/`M2`) on a 2-channel
EMC2302, so unlike the Bitaxe's single EMC2101 the driver and telemetry handle
a fan pair. `emc2101.rs` already has `new_with_address()` and the `Percent`
type; reuse `Percent` and the driver shape rather than duplicating them. The
EMC2302 uses different per-channel register blocks, so a separate module is
justified — but only the register map and channel indexing are new.

**4. Multi-chip chain bring-up in `asic/bm13xx/thread.rs`** — the long pole.
Today the thread brings up exactly one chip: `ChainInactive` then a single
`SetChipAddress 0x00`, everything broadcast (thread.rs:317). A real chain needs:
address enumeration (walk the chain, assign 0x00, 0x02, …), address-interval
setup, nonce-space division across the 4 chips, and the baud-rate step-up.
This is shared with emberOne (whose hash threads are still stubbed), so it
unblocks both boards.

**5. `board/nerdqaxe_pp.rs`** — inventory pattern on the firmware USB strings,
open control + data ports, init EMC2302 + TPS53647 + reset GPIO, discover 4×
BM1370, spawn one `BM13xxThread` over the chain, publish fan/temp/power
telemetry.

### Bring-up plan (on hardware, safety-ordered)

1. Flash firmware; confirm USB enumeration and that all 4 BM1370 are
   discovered over the chain.
2. Bring up power + thermal first (TPS53647 + EMC2302 + TMP1075). Never
   energize four BM1370 without working voltage and fan control.
3. Single-chip hash, then 4-chip chain hash.
4. Integrate with autotune + dashboard; tune power/thermal.

## Recommended sequence

Phase 0 (now, pre-hardware): cut the branch; finish reading `pi.kicad_sch` +
`power.kicad_sch`; this doc.

Phase 1 (pre-hardware software): firmware port (pin remap + USB strings) and a
minimal board-file scaffold that gets to chip discovery. Nothing else is on the
critical path to first light — see the TPS53647 deferral note.

Phase 2 (on the Pi + boards): enumerate 4 chips → fan/thermal (EMC2302 +
TMP1075) → TPS53647 voltage control → chain hash → integrate autotune.

Applying the ponytail ladder trimmed Phase 1: the TPS53647 work is a config
layer over existing `pmbus.rs` rather than a new driver, and it is not needed
to reach first light at all. Write it when enumeration proves the bridge works.

## Open items

- ~~Resolve TXD / RXD / RESET / SDA / SCL GPIOs~~ — done, pin map complete.
- `U33` (TXU0102) shifts the tacho lines. Check whether its enable needs
  asserting before EMC2302 tacho readings are valid.
- Confirm the ASIC UART baud rate the firmware should use at reset (BM1370
  boots at 115200 and is stepped up during chain init).
- TPS53647 PMBus address, phase config, sense resistors, core-voltage target
  and limits from `power.kicad_sch`.
- EMC2302 I2C address; register compatibility with the EMC2101 driver.
- TMP1075 addresses (U37/U38) on the NerdQAxe++ bus.
- Whether four NerdQAxe++ on one Pi 4 are all USB-attached (hub power budget)
  and how board identity/serials are assigned for the dashboard.
