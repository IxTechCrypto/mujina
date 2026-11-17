# Changelog

## v5.1.0 (2026-11-17)
- **Core 1 Freeze Recovery, Silence Escalation & Stall Resilience (`nano3s.rs`, `nano3s_tuner_core.rs`, `nano3s_ipc_shim`)**:
  - Implemented Core 1 status age and absence monitoring (`nano3s_ipc_has_received_status`, `nano3s_ipc_status_age_ms`). When Core 1 stops sending telemetry (>60s) or has never sent status (>180s), Core 0 immediately forces fan to 100% and skips AutoTune iterations.
  - After 5 minutes of total Core 1 silence or stale status, escalates to `request_recovery_reboot("core1_stale_status")` with a 1-hour minimum suppression backoff to protect flash storage (`/data/userdata/nano/last_recovery_reboot`).
  - Added deadlock resolution for stuck paused chains: tracks consecutive paused ticks, retries `resume_from_idle()` at tick 8 (~2 min), and escalates to recovery reboot at tick 20 (~5 min).
  - Escalated `StallStage::GaveUp` in `nano3s_tuner_core.rs` (`TuneAction::RebootRecovery`) to trigger recovery reboot after 40 consecutive stalled ticks following 2 failed chain re-inits.
  - Added reboot suppression log throttling (at most once per 60s) and call-site throttling to prevent console flooding.
  - Added fan state restoration: on recovery from a stale episode, invalidates AutoTune fan cache (`invalidate_fan_cache()`) so the next tick immediately applies calculated thermal duty or restores manual duty.
- **Auto-Tuner & Power Envelope (`mujina-miner/src/board/`)**:
  - Restored persistent target mode (`power:<W>` / `hashrate:<TH/s>`) across power cycles and added warm boot recovery from the last converged `SearchMemory` setpoint (`target_memory` in `/data/userconfig/tuning.conf`) via atomic `fsync` + rename writes.
  - Scaled the instantaneous PSU safety trip per mode (`min(caps.power_w * 1.03, power_safety_w)`) and added window-mean over-cap backoff so warmup spikes do not prematurely throttle clocks while preserving per-profile limits on `Balanced`/`Quiet`.
  - Automatically resets all 12 per-chip frequency offsets (`[0; 12]`) and defaults `per_chip` tuning (`chip_tune`) to `OFF` whenever switching profiles or targets, with per-tick IPC retry until hardware acknowledges zeroed offsets.
  - Added a one-time `'warmup stall unresolved after 2 re-inits'` event when the chain stays stalled after two `ReinitChain` attempts.
  - Allowed stalled chain telemetry (`0 GH/s`, valid power, `temp_max = 0.0°C` during UART brown-outs) to emit `0.0 TH/s` metrics so `AutoTuner` advances `AVG x/20`, reverts to best, and triggers `ReinitChain` instead of freezing at `AVG 0/20`.
  - Cleared persisted `chip_offset_mhz` and `chip_tune` when disabling AutoTune (`auto_tune:off`) or switching to manual tuning (`tune:` / power-target / temp-target / autotune-hashrate).
  - Embedded and auto-provisioned `rtos_core.elf` (`sha256: 2e88ff5f2b58acbe3761a1ab083ad3c46b89b296f7491f05ca1e0970717299d1`, built from `86a64eb`) and `deploy/mujina_display_startup.sh` in `companion_binaries::sync_companion_binaries()` with `sync_all()` and a one-time reboot (`/sharefs/reboot_request` + `/sbin/reboot -f`) when `rtos_core.elf` is updated on disk during OTA bundle upgrades.
  - Added proactive rising-temperature fan pre-ramping (`dt > 0.25°C/tick` within `6°C` of target), softened cool-down damping near target (`-3%/tick` max and `28%` equilibrium floor within `4.5°C` below target), and wired `SetFanMinPercent` + proportional boost across `cap .. cap + 3°C` so mild thermal overshoots ramp smoothly instead of slamming to `100%` (reserving `100%` for `>= cap + 3°C` or missing sensors).
- **Manual Fan Arbitration & Thermal Breach Safety (`nano3s.rs`, `nano3s_auto_tune.rs`)**:
  - `PATCH /api/v0/boards/{name}/fan` with `mode: "manual"` pauses the AutoTune chip-temp fan PI loop below the thermal cap so it no longer clobbers manual fan settings on `/tmp/harness_control`.
  - Closed the loop-top race between concurrent API fan writes and `fan_tick()` by syncing `MANUAL_FAN_DUTY` at loop top and before `fan_tick()` and re-asserting `fan:manual,<duty>` on changes.
  - Preserved thermal breach protection in manual fan mode: `temp >= cap`, `SetFanFull`, or missing temperature readings force `100%` fan (`hold_full`), hold `100%` across the `cap - 5°C` release band, and restore the user's manual duty (`fan:manual,<duty>`, never `fan:auto,`) on cooldown.
- **Manual Tuning API & Web Dashboard UX (`api/v0.rs`, `dashboard.html`)**:
  - Raised manual tuning bounds in `PATCH /api/v0/boards/{name}/tuning` to match `NANO3S_ENVELOPE` (`3912 mV` max voltage, `580 MHz` max PLL domain) and rejected the forbidden `3600 mV` grid point.
  - Added `autocomplete="off"` and non-email `name` attributes to numeric tuning inputs in `dashboard.html`.
  - Redesigned `body.theme-parasite` CSS in `dashboard.html` to single `1px solid #d4d4d4` borders matching `Parasite.space`.
  - Removed unconditional `body.per_chip = true` from `applyOwner()` in `dashboard.html` so profile switches honor the backend `per_chip: false` default.
  - Updated empty tuner event log placeholder in `dashboard.html` (`warmupText`) to explain why the chain is not hashing (`"No status from the hash chain..."` when Core 1 `input_v` is missing on Nano 3S, `"Not hashing (0 GH/s)..."` at `0 GH/s`, or `"Warming up…"`).
- **Direction A Theme-Synced Circular LCD UI & Live Display API (`api/v0.rs`, `nano3s.rs`, `fb_button.rs`, `nano3s_ui`)**:
  - Added `GET /api/v0/display` and `PATCH /api/v0/display` (returning `400 Bad Request` vs `500 Internal Server Error`, atomically persisted with `sync_all()` to `/data/userconfig/theme.conf` and `/tmp/ui_theme`) synced live with the dashboard `#uitheme` selector (`cyberpunk`, `minimal`, `parasite`).
  - Re-themed all `240x240` circular LCD screens (`mujina-logo` splash, `nano3s` main mining stats, `nano3s-tuner` 12-chip thermal corona, `ip` network/RSSI screen, Parasite-only `parasite-logo` screen after `ip`, and shared `plasma` Bitcoin Plasma disk across all themes; removed `nano3s-diag` and `pizza`).
  - Added 25 FPS animated perimeter rings on `mujina-logo` and `parasite-logo` and single-border `Parasite.space` dial arcs/cards.
  - Optimized `ui_draw_radial_ticks` to a single-pass `O(ring_lut_len)` LUT traversal and added early radial bounds rejection in `ui_draw_smooth_arc`, cutting 25 FPS splash CPU load in half on K230.
  - Replaced the legacy LVGL `wifi-setup` screen in `nano3s_ui` with a 20 FPS theme-synced (`cyberpunk`, `minimal`, `parasite`) screen featuring an anti-aliased animated Bluetooth symbol and expanding radio wave arcs.
  - Fixed the `ble_setup` `"wifi-setup"` display page respawn loop (`ble_setup.rs`, `mujina_display_startup.sh`, and `companion_binaries` auto-sync of `/data/ble_setup`) so provisioned units wait for `wlan0` IP before starting BLE setup and restore `fb_page` to `nano3s` rather than overwriting the screen back to `"wifi-setup"`.
  - Committed standalone asset generator `nano3s_ui/tools/generate_firmware_ui_assets.py` (`numpy`, `pymupdf`, `Pillow`) and `nano3s_ui/tools/assets/parasite_logo.png`.

## v5.0.8 (2026-11-13)
- **Streaming OTA Firmware Upload (`mujina-miner/src/api/v0.rs`)**:
  - `POST /api/v0/firmware/bundle` and `POST /api/v0/firmware/{target}` now stream the upload to `<live_path>.new` instead of buffering the whole body in RAM, fixing the out-of-memory kill of `mujina-minerd` on 94 MB Nano 3S boards during ~21.5 MB OTA uploads.
  - Validates `\x7fELF` and `nano3s_ipc_` incrementally (including across chunk boundaries), rejects truncated streams and trailing bytes, `fsync`s staged files and the parent directory, and only renames once every section has staged successfully.
  - A process-wide upload lock returns `409 CONFLICT` ("firmware update already in progress") for concurrent uploads so a retry cannot swap a partial `.new` over the live binary.
  - Stale `*.new` files are unlinked before the free-space check and at daemon startup, so an interrupted upload no longer causes a false `507 INSUFFICIENT_STORAGE`.

## v5.0.7 (2026-11-12)
- **Unified Web UI Navigation & Layout (`dashboard.html`, `led.html`, `firmware.html`, `info.html`, `pool.html`, `tuning.html`)**:
  - Standardized all six web pages onto the unified 10-item `.menubar` (`Dashboard`, `Power`, `Tuning`, `Fan`, `Fan Curve`, `Pool`, `LED`, `Firmware`, `Display`, `Info`) and `1120px` `.wrap` container so menus never disappear or shrink when switching tabs.
  - Extended `openFromHash()` in `dashboard.html` to support `#power`, `#tuning`, `#fan`, `#curve`, `#pool`, and `#display` deep links from subpages.
- **Toggle Switches & Mobile UX**:
  - Replaced the raw `#ownContinuous` checkbox in `dashboard.html` and upgraded `#atEnable` in `tuning.html` with `44px`-tall `.toggle-row` pill switches and explicit `ON`/`OFF` state indicators.
  - Synchronized `mujina-theme` (`cyberpunk`, `minimal`, `parasite`) across all subpages, added `viewport-fit=cover`, gated `:hover` states behind `@media (hover: hover) and (pointer: fine)`, enlarged mobile form inputs/selects (`44px`, `16px` font) and LED slider thumbs (`24px`), and added instant `MJBUNDL1` header validation on file selection in `firmware.html`.

## v5.0.6 (2026-11-12)
- **LED Web UI (`led.html`)**:
  - Preserved the full `.menubar` / `.menubtn` navigation bar and `1120px` layout width on `/led` to match `/dashboard`, eliminating parent menu shrinking on desktop and mobile.
  - Prevented accidental effect selection during touch scrolling/finger movement on phones by gating `:hover` to `@media (hover: hover) and (pointer: fine)`, adding drag-cancelling tap handling (`wireTapButton`), and guarding `pollState()` against racing with local `pushState()` updates.
- **LED Effects (`nano3s.rs` & `led.html`)**:
  - Upgraded `ChaseRainbow` (`chase_rainbow`) to a Cylon-style directional comet sweep with a 3-pixel decaying tail (`[1.0, 0.55, 0.25, 0.08]`) bouncing smoothly end-to-end across a full `360°` rainbow spectrum (`40°` per LED with rotating phase).
  - Extracted shared `AnimState::advance_scanner` and `AnimState::comet_level` helpers and added unit test `anim_state_chase_rainbow_head_tail_and_spectrum`.
