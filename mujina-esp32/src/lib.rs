//! NerdQX (ESP32-S3R8) portable firmware logic. `no_std` unless testing.
#![cfg_attr(not(test), no_std)]

pub mod chip_tuner;
pub mod hw_pins;
pub mod lcd_hud;
pub mod power_cooling;

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
}

