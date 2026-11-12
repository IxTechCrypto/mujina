# Changelog

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
