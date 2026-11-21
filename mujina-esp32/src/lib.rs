//! NerdQX (ESP32-S3R8) portable firmware logic. `no_std` unless testing or targeting ESP-IDF.
#![cfg_attr(not(any(test, feature = "esp32s3")), no_std)]

pub mod bm1370;
#[cfg(any(test, feature = "esp32s3"))]
pub mod captive_dns;
pub mod chip_tuner;
#[cfg(feature = "esp32s3")]
pub mod hud_render;
#[cfg(any(test, feature = "esp32s3"))]
pub mod hud_data;
pub mod hw_pins;
#[cfg(feature = "esp32s3")]
pub mod i2c_bus;
#[cfg(feature = "esp32s3")]
pub mod lcd_driver;
pub mod lcd_hud;
#[cfg(any(test, feature = "esp32s3"))]
pub mod market;
#[cfg(any(test, feature = "esp32s3"))]
pub mod net_web;
pub mod power_cooling;
pub mod qr_code;
#[cfg(feature = "esp32s3")]
pub mod ui_assets;
#[cfg(feature = "esp32s3")]
pub mod ui_fonts;

#[cfg(test)]
mod tests {
    use super::chip_tuner::*;
    use super::hw_pins::*;
    use super::lcd_hud::*;
    use super::power_cooling::*;

    #[test]
    fn pins_and_constants() {
        assert_eq!((UART1_TX, UART1_RX, ASIC_RST), (17, 18, 1));
        assert_eq!((I2C0_SDA, I2C0_SCL), (44, 43));
        assert_eq!((PWR_EN, VR_RDY, LDO_EN), (10, 11, 13));
        assert_eq!((TMUX_A0, TMUX_A1), (2, 3));
        assert_eq!(LCD_DATA, [39, 40, 41, 42, 45, 46, 47, 48]);
        assert_eq!((LCD_WR, LCD_RD, LCD_CS, LCD_DC, LCD_RST, LCD_BL, LCD_PWR), (8, 9, 6, 7, 5, 38, 15));
        assert_eq!((BTN1, BTN2), (14, 0));
        assert_eq!((ADDR_TPS53647, ADDR_TMP451, ADDR_TMP1075, ADDR_EMC2302), (0x71, 0x4C, [0x48, 0x49], 0x2E));
        assert_eq!(AF_BOARD_TAG, "MUJINA-BOARD:nerdqx;");
        assert_eq!(OTA_ENDPOINT, "/api/v0/firmware/ota");
    }

    #[test]
    fn pins_unique() {
        let mut all = vec![UART1_TX, UART1_RX, ASIC_RST, I2C0_SDA, I2C0_SCL, PWR_EN, VR_RDY, LDO_EN,
            TMUX_A0, TMUX_A1, LCD_WR, LCD_RD, LCD_CS, LCD_DC, LCD_RST, LCD_BL, LCD_PWR, BTN1, BTN2];
        all.extend_from_slice(&LCD_DATA);
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n, "GPIO assigned twice");
    }

    #[test]
    fn partition_table_layout() {
        let csv = include_str!("../partitions.csv");
        let mut end = 0x9000u32;
        let mut rows = 0;
        for l in csv.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let f: Vec<&str> = l.split(',').map(|s| s.trim()).collect();
            let off = u32::from_str_radix(&f[3][2..], 16).unwrap();
            let size = u32::from_str_radix(&f[4][2..], 16).unwrap();
            assert!(off >= end, "{} overlaps", f[0]);
            end = off + size;
            rows += 1;
        }
        assert_eq!(rows, 8);
        assert_eq!(end, 0x1000000);
    }

    #[test]
    fn sdkconfig_keys() {
        let s = include_str!("../sdkconfig.defaults.nerdqx");
        for k in ["CONFIG_SPIRAM_MODE_OCT=y", "CONFIG_SPIRAM_MALLOC_RESERVE_INTERNAL=65536",
            "CONFIG_SPIRAM_TRY_ALLOCATE_WIFI_LWIP=y", "CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC=y",
            "CONFIG_ESPTOOLPY_FLASHSIZE_16MB=y", "CONFIG_ESPTOOLPY_FLASHMODE_DIO=y", "CONFIG_ESPTOOLPY_FLASHFREQ_80M=y"] {
            assert!(s.lines().any(|l| l == k), "missing {k}");
        }
    }

    #[test]
    fn cargo_config_wires_sdkconfig_and_partition_table() {
        let cfg = include_str!("../.cargo/config.toml");
        assert!(cfg.contains("sdkconfig.defaults.nerdqx"));
        assert!(cfg.contains("--partition-table partitions.csv"));
    }

    #[test]
    fn power_presets() {
        let c = PowerCoolingConfig::default();
        assert_eq!(c.power_ceiling_w, 144.0);
        assert_eq!(c.effective_ceiling_w(12.0), 144.0);
        assert_eq!(PowerCoolingConfig::for_preset(Preset::UpgradedAir, 20, true).power_ceiling_w, 192.0);
        assert_eq!(PowerCoolingConfig::for_preset(Preset::AiO, 20, true).power_ceiling_w, 240.0);
    }

    #[test]
    fn power_stock_clamps() {
        let mut c = PowerCoolingConfig::default();
        c.power_ceiling_w = 240.0;
        assert_eq!(c.max_ceiling_w(), 180.0);
        assert_eq!(c.effective_ceiling_w(12.0), 180.0);
        assert_eq!(c.effective_ceiling_w(11.0), 165.0); // 15*Vin
        c.power_ceiling_w = 10.0;
        assert_eq!(c.effective_ceiling_w(12.0), 60.0);
    }

    #[test]
    fn power_unlock_requires_fuse_and_confirm() {
        let mut c = PowerCoolingConfig::for_preset(Preset::UpgradedAir, 20, false);
        assert_eq!(c.effective_ceiling_w(12.0), 180.0); // unconfirmed
        c.fuse_upgrade_confirmed = true;
        assert_eq!(c.effective_ceiling_w(12.0), 192.0);
        c.power_ceiling_w = 300.0;
        assert_eq!(c.effective_ceiling_w(12.0), 240.0);
        c.fuse_amps = 15;
        assert_eq!(c.effective_ceiling_w(12.0), 180.0);
        let a = PowerCoolingConfig::for_preset(Preset::AiO, 25, false);
        assert_eq!(a.effective_ceiling_w(12.0), 180.0);
    }

    #[test]
    fn pump_guard() {
        let c = PowerCoolingConfig::for_preset(Preset::AiO, 20, true);
        assert_eq!(c.pump_w, 15.0);
        assert!(c.pump_guard_ok(220.0, 12.0)); // 20 W spare
        assert!(!c.pump_guard_ok(230.0, 12.0)); // 10 W spare < 15 W pump
        assert!(PowerCoolingConfig::default().pump_guard_ok(144.0, 12.0));
        assert_eq!(c.pump_headroom_w(200.0, 12.0), 25.0);
    }

    #[test]
    fn vr_register() {
        assert_eq!(vr_reg_for_hz(35_000), 0x15F1);
        assert_eq!(vr_reg_for_hz(0), 0);
    }

    #[test]
    fn ramp() {
        assert_eq!(ramp_freq(0, 500.0), 56.25);
        assert_eq!(ramp_freq(1, 500.0), 62.5);
        assert_eq!(ramp_len(56.25), 1);
        assert_eq!(ramp_len(62.5), 2);
        assert_eq!(ramp_len(500.0), 72); // 56.25 + 71*6.25 = 500
        let n = ramp_len(495.0); // non-multiple: last step clamps to target
        assert_eq!(ramp_freq(n - 1, 495.0), 495.0);
        assert_eq!(ramp_freq(n + 5, 495.0), 495.0);
    }

    #[test]
    fn vcore_follows_clock_and_hits_factory_point() {
        assert_eq!(vcore_for_freq_mv(400.0), 1150);
        assert_eq!(vcore_for_freq_mv(500.0), 1150);
        assert_eq!(vcore_for_freq_mv(START_FREQ_MHZ as f32), START_MV);
        assert_eq!(boot_point_for_profile("max_hash"), (777, 1250));
        for p in ["efficient", "balanced", "quiet", "anything"] {
            assert_eq!(boot_point_for_profile(p), (450, 1120));
        }
        assert_eq!(vcore_for_freq_mv(MAX_BASE_FREQ_MHZ), 1265);
        assert_eq!(vcore_for_freq_mv(2000.0), MAX_VCORE_MV);
        assert_eq!(vcore_for_freq_mv(f32::NAN), 1150);
        let mut prev = 0;
        for f in (400..=825).step_by(5) {
            let mv = vcore_for_freq_mv(f as f32);
            assert!(mv >= prev && mv % 5 == 0 && mv <= MAX_VCORE_MV);
            prev = mv;
        }
    }

    #[test]
    fn vr_fan_never_below_40_and_fails_safe() {
        assert_eq!(vr_fan_duty_pct(Some(20.0)), 40);
        assert_eq!(vr_fan_duty_pct(Some(35.0)), 40);
        assert_eq!(vr_fan_duty_pct(Some(40.0)), 70);
        assert_eq!(vr_fan_duty_pct(Some(45.0)), 100);
        assert_eq!(vr_fan_duty_pct(Some(70.0)), 100);
        assert_eq!(vr_fan_duty_pct(None), 100);
        assert_eq!(vr_fan_duty_pct(Some(f32::NAN)), 100);
    }

    #[test]
    fn tmux_clamp() {
        assert_eq!(tmux_safe_clamp(false, 600.0, 1300), (495.0, 1150));
        assert_eq!(tmux_safe_clamp(false, 400.0, 1100), (400.0, 1100));
        assert_eq!(tmux_safe_clamp(true, 600.0, 1300), (600.0, 1300));
    }

    fn inp() -> TuneInput {
        TuneInput {
            chip_temps_c: [55.0; 4],
            chip_hashrates_ghs: [500.0; 4],
            chip_error_pct: [0.1; 4],
            pin_w: 100.0,
            effective_ceiling_w: 144.0,
            vr_temp_c: 60.0,
        }
    }

    #[test]
    fn tuner_ramps_up_cool_chips_in_steps() {
        let mut e = ChipTuneEngine::new();
        let o = e.step(&inp());
        assert_eq!(o.offsets_mhz, [6.25; 4]);
        assert!(!o.board_limited && !o.critical);
    }

    #[test]
    fn tuner_clamps_at_plus_50() {
        let mut e = ChipTuneEngine::new();
        let mut o = e.step(&inp());
        for _ in 0..30 {
            o = e.step(&inp());
        }
        assert_eq!(o.offsets_mhz, [50.0; 4]);
    }

    #[test]
    fn tuner_per_chip_independent() {
        let mut e = ChipTuneEngine::new();
        let mut i = inp();
        i.chip_temps_c[2] = 68.0; // hot
        i.chip_error_pct[3] = 5.0; // erroring
        let o = e.step(&i);
        assert_eq!(o.offsets_mhz, [6.25, 6.25, -6.25, -6.25]);
    }

    #[test]
    fn tuner_clamps_at_minus_50() {
        let mut e = ChipTuneEngine::new();
        let mut i = inp();
        i.pin_w = 200.0;
        let mut o = e.step(&i);
        for _ in 0..30 {
            o = e.step(&i);
        }
        assert_eq!(o.offsets_mhz, [-50.0; 4]);
        assert!(o.board_limited);
    }

    #[test]
    fn tuner_vr_hot_and_critical_and_hold() {
        let mut e = ChipTuneEngine::new();
        let mut i = inp();
        i.vr_temp_c = 101.0;
        assert!(e.step(&i).board_limited);
        e.reset();
        let mut i = inp();
        i.chip_temps_c[0] = 80.0;
        assert!(e.step(&i).critical);
        e.reset();
        let mut i = inp();
        i.pin_w = 142.0; // within 3% of ceiling: hold
        assert_eq!(e.step(&i).offsets_mhz, [0.0; 4]);
    }

    #[test]
    fn tuner_fails_closed_on_nan() {
        let mut i = inp();
        i.pin_w = f32::NAN;
        let o = ChipTuneEngine::new().step(&i);
        assert_eq!(o.offsets_mhz, [-6.25; 4]);
        assert!(o.board_limited);
        let mut i = inp();
        i.effective_ceiling_w = f32::NAN;
        assert!(ChipTuneEngine::new().step(&i).board_limited);
        let mut i = inp();
        i.vr_temp_c = f32::NAN;
        assert!(ChipTuneEngine::new().step(&i).board_limited);
        let mut i = inp();
        i.chip_temps_c[1] = f32::NAN;
        i.chip_error_pct[2] = f32::NAN;
        let o = ChipTuneEngine::new().step(&i);
        assert_eq!(o.offsets_mhz, [6.25, -6.25, -6.25, 6.25]);
    }

    #[test]
    fn power_fails_closed_on_bad_vin() {
        let c = PowerCoolingConfig::default();
        assert_eq!(c.effective_ceiling_w(f32::NAN), 60.0);
        assert_eq!(c.effective_ceiling_w(0.0), 60.0);
        assert_eq!(c.effective_ceiling_w(-12.0), 60.0);
        let a = PowerCoolingConfig::for_preset(Preset::AiO, 20, true);
        assert_eq!(a.effective_ceiling_w(f32::INFINITY), 60.0);
    }

    #[test]
    fn power_unlocked_budget_includes_pump() {
        let a = PowerCoolingConfig::for_preset(Preset::AiO, 20, true);
        assert_eq!(a.effective_ceiling_w(12.0), 225.0);
        assert_eq!(PowerCoolingConfig::for_preset(Preset::AiO, 25, true).effective_ceiling_w(12.0), 240.0);
        assert_eq!(PowerCoolingConfig::for_preset(Preset::AiO, 30, true).effective_ceiling_w(12.0), 240.0);
        assert_eq!(PowerCoolingConfig::for_preset(Preset::UpgradedAir, 20, true).effective_ceiling_w(12.0), 192.0);
    }

    #[test]
    fn sdkconfig_console_not_on_i2c_uart() {
        assert!(include_str!("../sdkconfig.defaults.nerdqx").lines().any(|l| l == "CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y"));
    }

    #[test]
    fn tuner_laggard_not_boosted() {
        let mut e = ChipTuneEngine::new();
        let mut i = inp();
        i.chip_hashrates_ghs[1] = 100.0;
        assert_eq!(e.step(&i).offsets_mhz[1], 0.0);
    }

    fn tap(h: &mut Hud, b: Button, t: u32) -> Option<HudAction> {
        h.on_button(b, Edge::Press, t);
        h.on_button(b, Edge::Release, t + 50)
    }

    #[test]
    fn hud_rectangular_tdisplay_geometry_and_layouts() {
        let g = DisplayGeometry::NERDQX_TDISPLAY_S3;
        assert_eq!((g.width, g.height), (320, 170));
        assert_eq!(g.shape, DisplayShape::Rectangular);
        assert!(g.swap_xy && g.gap_y == 35);
        assert_eq!(HEADER_RECT.bottom(), PAGE_BOUNDS.y);
        assert_eq!(PAGE_BOUNDS.bottom(), FOOTER_RECT.y);
        assert_eq!(FOOTER_RECT.bottom(), 170);

        assert!(BOOT_SPLASH_LAYOUT.raccoon_box.fits_in(SCREEN_BOUNDS));
        assert!(BOOT_SPLASH_LAYOUT.title_box.fits_in(SCREEN_BOUNDS));
        assert!(BOOT_SPLASH_LAYOUT.progress_bar.fits_in(SCREEN_BOUNDS));
        assert!(!BOOT_SPLASH_LAYOUT.raccoon_box.overlaps(BOOT_SPLASH_LAYOUT.title_box));
        assert!(!BOOT_SPLASH_LAYOUT.title_box.overlaps(BOOT_SPLASH_LAYOUT.progress_bar));

        for screen in Screen::ALL {
            match screen.layout() {
                ScreenLayout::MainHud { dial, preset_badge, ip_badge, callout_cards } => {
                    assert!(dial.cx - (dial.r_outer as i16) < 0); // left cutoff
                    assert!(dial.cy + (dial.r_outer as i16) > LCD_HEIGHT as i16); // bottom cutoff
                    assert!(preset_badge.fits_in(HEADER_RECT));
                    assert!(ip_badge.fits_in(HEADER_RECT));
                    assert!(!preset_badge.overlaps(ip_badge));
                    for i in 0..4 {
                        assert!(callout_cards[i].fits_in(PAGE_BOUNDS));
                        assert!(dial.right_edge() < callout_cards[i].x as i16);
                        for j in (i + 1)..4 {
                            assert!(!callout_cards[i].overlaps(callout_cards[j]));
                        }
                    }
                }
                ScreenLayout::ChipDieGrid { cards, load_bars } => {
                    for i in 0..4 {
                        assert!(cards[i].fits_in(PAGE_BOUNDS));
                        assert!(load_bars[i].fits_in(cards[i]));
                        for j in (i + 1)..4 {
                            assert!(!cards[i].overlaps(cards[j]));
                        }
                    }
                }
                ScreenLayout::TunerPsu { psu_card, power_bar, tuner_card } => {
                    assert!(psu_card.fits_in(PAGE_BOUNDS));
                    assert!(tuner_card.fits_in(PAGE_BOUNDS));
                    assert!(power_bar.fits_in(psu_card));
                    assert!(!psu_card.overlaps(tuner_card));
                }
                ScreenLayout::PoolShares { summary_bar, rows } => {
                    assert!(summary_bar.fits_in(PAGE_BOUNDS));
                    for i in 0..2 {
                        assert!(rows[i].fits_in(PAGE_BOUNDS));
                        assert!(!summary_bar.overlaps(rows[i]));
                        for j in (i + 1)..2 {
                            assert!(!rows[i].overlaps(rows[j]));
                        }
                    }
                }
                ScreenLayout::ClockBtc { clock_box, sec_bar, left_card, right_card } => {
                    for r in [clock_box, sec_bar, left_card, right_card] {
                        assert!(r.fits_in(PAGE_BOUNDS));
                    }
                    assert!(!clock_box.overlaps(sec_bar));
                    assert!(!left_card.overlaps(right_card));
                    assert!(!sec_bar.overlaps(left_card));
                }
                ScreenLayout::SystemQr { qr_box, info_card } => {
                    assert_eq!(qr_box.w, qr_box.h); // QR stays square
                    assert!(qr_box.fits_in(PAGE_BOUNDS));
                    assert!(info_card.fits_in(PAGE_BOUNDS));
                    assert!(!qr_box.overlaps(info_card));
                }
            }
        }
    }

    #[test]
    fn hud_short_navigation() {
        let mut h = Hud::new();
        assert_eq!(tap(&mut h, Button::Btn1, 0), Some(HudAction::ScreenChanged(Screen::ChipDieGrid)));
        assert_eq!(tap(&mut h, Button::Btn2, 100), Some(HudAction::ScreenChanged(Screen::MainHud)));
        assert_eq!(tap(&mut h, Button::Btn2, 200), Some(HudAction::ScreenChanged(Screen::SystemQr)));
        assert_eq!(tap(&mut h, Button::Btn1, 300), Some(HudAction::ScreenChanged(Screen::MainHud)));
        for _ in 0..6 {
            tap(&mut h, Button::Btn1, 400);
        }
        assert_eq!(h.screen, Screen::MainHud);
    }

    #[test]
    fn hud_long_btn1_toggles_backlight_and_wake() {
        let mut h = Hud::new();
        h.on_button(Button::Btn1, Edge::Press, 0);
        assert_eq!(h.tick(1499), None);
        assert_eq!(h.tick(1500), Some(HudAction::BacklightOff));
        assert_eq!(h.tick(3000), None); // fires once
        assert_eq!(h.on_button(Button::Btn1, Edge::Release, 3100), None);
        assert!(!h.backlight_on);
        // short press wakes without navigating
        assert_eq!(tap(&mut h, Button::Btn2, 4000), Some(HudAction::BacklightOn));
        assert_eq!(h.screen, Screen::MainHud);
        assert!(h.backlight_on);
    }

    #[test]
    fn hud_long_btn2_retune() {
        let mut h = Hud::new();
        h.on_button(Button::Btn2, Edge::Press, 10);
        assert_eq!(h.tick(1510), Some(HudAction::Retune));
        assert_eq!(h.on_button(Button::Btn2, Edge::Release, 1600), None);
        assert_eq!(h.screen, Screen::MainHud);
    }

    #[test]
    fn hud_dual_press_safe_power_down() {
        let mut h = Hud::new();
        h.on_button(Button::Btn1, Edge::Press, 0);
        h.on_button(Button::Btn2, Edge::Press, 100);
        assert_eq!(h.tick(2000), None); // no single long press while dual
        assert_eq!(h.tick(5099), None);
        assert_eq!(h.tick(5100), Some(HudAction::SafePowerDown));
        assert_eq!(h.tick(6000), None);
        assert_eq!(h.on_button(Button::Btn1, Edge::Release, 6100), None);
        assert_eq!(h.on_button(Button::Btn2, Edge::Release, 6100), None);
        // gesture state reset
        assert_eq!(tap(&mut h, Button::Btn1, 7000), Some(HudAction::ScreenChanged(Screen::ChipDieGrid)));
    }

    #[test]
    fn hud_aborted_dual_no_short() {
        let mut h = Hud::new();
        h.on_button(Button::Btn1, Edge::Press, 0);
        h.on_button(Button::Btn2, Edge::Press, 10);
        assert_eq!(h.on_button(Button::Btn1, Edge::Release, 100), None);
        assert_eq!(h.on_button(Button::Btn2, Edge::Release, 120), None);
        assert_eq!(h.screen, Screen::MainHud);
    }

    #[test]
    fn tps53647_vid_and_linear11_helpers() {
        assert_eq!(mv_to_vr12_vid(1150), Some(181));
        assert_eq!(mv_to_vr12_vid(1200), Some(191));
        assert_eq!(mv_to_vr12_vid(1000), Some(0x96)); // factory-default 0x97 avoided
        assert_eq!(mv_to_vr12_vid(750), None);
        assert_eq!(mv_to_vr12_vid(1450), None);
        assert!((vr12_vid_to_volts(181) - 1.15).abs() < 1e-4);
        assert_eq!(vr12_vid_to_volts(0), 0.0);

        // Q7.9 MFR_VOUT_MEASURED decode (589 / 512 = 1.1504 V), plus 0xFFFF fallback
        let v = decode_tps53647_vout_measured(589, 181);
        assert!((v - 1.15039).abs() < 1e-3);
        let fb = decode_tps53647_vout_measured(0xFFFF, 181);
        assert!((fb - 1.15).abs() < 1e-4);

        for target in [95.0f32, 125.0, 142.0, 12.66] {
            let decoded = decode_linear11(encode_linear11(target));
            assert!((decoded - target).abs() < 0.05, "target {target} decoded {decoded}");
        }
    }

    #[test]
    fn tmp451_and_fail_closed_safety_cutoff() {
        assert_eq!(decode_tmp451_remote_c(0x80, 0x3C, 0x00), None); // BUSY
        assert_eq!(decode_tmp451_remote_c(0x04, 0x3C, 0x00), None); // REMOTE_OPEN
        let cal = decode_tmp451_remote_c(0x00, 0x3C, 0x00).expect("60 C raw diode");
        assert!((cal - 35.9).abs() < 1e-3);

        let good_temps = [Some(55.0), Some(56.0), Some(57.0), Some(58.0)];
        assert_eq!(evaluate_safety_cutoff(true, 12.5, 90.0, 144.0, Some(52.0), &good_temps, 0), None);
        assert_eq!(
            evaluate_safety_cutoff(true, 12.5, 145.0, 144.0, Some(52.0), &good_temps, 0),
            Some("power_or_vin_fault")
        );
        assert_eq!(
            evaluate_safety_cutoff(true, 12.5, 90.0, 144.0, Some(95.0), &good_temps, 0),
            Some("vr_overtemp_or_invalid")
        );
        assert_eq!(
            evaluate_safety_cutoff(true, 12.5, 90.0, 144.0, Some(52.0), &[Some(55.0), Some(85.0), None, None], 0),
            Some("asic_overtemp")
        );
        assert_eq!(
            evaluate_safety_cutoff(true, 12.5, 90.0, 144.0, Some(52.0), &[None; 4], 0),
            Some("all_temp_sensors_missing")
        );
        assert_eq!(
            evaluate_safety_cutoff(true, 12.5, 90.0, 144.0, Some(52.0), &good_temps, 3),
            Some("tps53647_i2c_lost")
        );

        assert_eq!(compute_fan_duty_pct(Some(45.0), Some(40.0), false), 25);
        assert_eq!(compute_fan_duty_pct(Some(58.0), Some(40.0), false), 25);
        assert_eq!(compute_fan_duty_pct(Some(59.5), Some(40.0), false), 35);
        assert_eq!(compute_fan_duty_pct(Some(61.0), Some(40.0), false), 45);
        assert_eq!(compute_fan_duty_pct(Some(72.0), Some(40.0), false), 100);
        assert_eq!(compute_fan_duty_pct(None, Some(40.0), false), 100);
        assert_eq!(compute_fan_duty_pct(Some(60.0), Some(80.0), false), 100);
        assert_eq!(compute_fan_duty_pct(Some(50.0), Some(40.0), true), 100);
    }
}

