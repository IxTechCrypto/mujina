# Changelog

## v5.0.6 (2026-11-12)
- **LED Web UI (`led.html`)**:
  - Preserved the full `.menubar` / `.menubtn` navigation bar and `1120px` layout width on `/led` to match `/dashboard`, eliminating parent menu shrinking on desktop and mobile.
  - Prevented accidental effect selection during touch scrolling/finger movement on phones by gating `:hover` to `@media (hover: hover) and (pointer: fine)`, adding drag-cancelling tap handling (`wireTapButton`), and guarding `pollState()` against racing with local `pushState()` updates.
- **LED Effects (`nano3s.rs` & `led.html`)**:
  - Upgraded `ChaseRainbow` (`chase_rainbow`) to a Cylon-style directional comet sweep with a 3-pixel decaying tail (`[1.0, 0.55, 0.25, 0.08]`) bouncing smoothly end-to-end across a full `360°` rainbow spectrum (`40°` per LED with rotating phase).
  - Extracted shared `AnimState::advance_scanner` and `AnimState::comet_level` helpers and added unit test `anim_state_chase_rainbow_head_tail_and_spectrum`.
