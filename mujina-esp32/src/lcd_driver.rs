//! ESP-IDF hardware i80 8-bit parallel ST7789V driver for LilyGO T-Display-S3 (320x170).

use core::ptr;
use esp_idf_svc::sys::{
    self, esp, esp_lcd_i80_bus_config_t, esp_lcd_i80_bus_handle_t, esp_lcd_new_i80_bus,
    esp_lcd_new_panel_io_i80, esp_lcd_new_panel_st7789, esp_lcd_panel_dev_config_t,
    esp_lcd_panel_disp_on_off, esp_lcd_panel_draw_bitmap, esp_lcd_panel_handle_t,
    esp_lcd_panel_init, esp_lcd_panel_invert_color, esp_lcd_panel_io_handle_t,
    esp_lcd_panel_io_i80_config_t, esp_lcd_panel_mirror, esp_lcd_panel_reset,
    esp_lcd_panel_set_gap, esp_lcd_panel_swap_xy, gpio_config, gpio_config_t, gpio_set_level,
    EspError,
};

use crate::hw_pins::{
    LCD_BL, LCD_CS, LCD_DATA, LCD_DC, LCD_HEIGHT, LCD_PWR, LCD_RD, LCD_RST, LCD_WIDTH, LCD_WR,
};

pub struct DmaFramebuffer {
    ptr: *mut u16,
    len: usize,
}

impl DmaFramebuffer {
    /// Allocate a 64-byte aligned framebuffer (PSRAM preferred, internal DMA RAM fallback).
    pub fn new(len: usize, fill: u16) -> Self {
        let bytes = len.checked_mul(2).expect("framebuffer size overflow");
        let caps_psram = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT;
        let caps_dma = sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL;
        let ptr = unsafe {
            let p = sys::heap_caps_aligned_alloc(64, bytes, caps_psram) as *mut u16;
            if p.is_null() {
                sys::heap_caps_aligned_alloc(64, bytes, caps_dma) as *mut u16
            } else {
                p
            }
        };
        assert!(!ptr.is_null(), "failed to allocate 64B-aligned DMA framebuffer");
        let slice = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        slice.fill(fill);
        Self { ptr, len }
    }

    pub fn as_slice(&self) -> &[u16] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u16] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for DmaFramebuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { sys::heap_caps_free(self.ptr as *mut _) };
            self.ptr = ptr::null_mut();
        }
    }
}

pub struct St7789Panel {
    _bus: esp_lcd_i80_bus_handle_t,
    _io: esp_lcd_panel_io_handle_t,
    panel: esp_lcd_panel_handle_t,
}

impl St7789Panel {
    /// Initialize LilyGO T-Display-S3 power rails, 8-bit i80 bus, and ST7789V panel.
    pub fn init() -> Result<Self, EspError> {
        unsafe {
            // Drive LCD_PWR (GPIO15), LCD_RD (GPIO9), and LCD_BL (GPIO38) high.
            let mask = (1u64 << LCD_PWR) | (1u64 << LCD_RD) | (1u64 << LCD_BL);
            let io_conf = gpio_config_t {
                pin_bit_mask: mask,
                mode: sys::gpio_mode_t_GPIO_MODE_OUTPUT,
                pull_up_en: sys::gpio_pullup_t_GPIO_PULLUP_DISABLE,
                pull_down_en: sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
                intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
            };
            esp!(gpio_config(&io_conf))?;
            esp!(gpio_set_level(LCD_PWR as i32, 1))?;
            esp!(gpio_set_level(LCD_RD as i32, 1))?;
            esp!(gpio_set_level(LCD_BL as i32, 1))?;

            let mut data_gpio_nums = [-1i32; 16];
            for (i, &pin) in LCD_DATA.iter().enumerate() {
                data_gpio_nums[i] = pin as i32;
            }

            let mut bus_config: esp_lcd_i80_bus_config_t = core::mem::zeroed();
            bus_config.dc_gpio_num = LCD_DC as i32;
            bus_config.wr_gpio_num = LCD_WR as i32;
            bus_config.clk_src = sys::soc_periph_lcd_clk_src_t_LCD_CLK_SRC_DEFAULT;
            bus_config.data_gpio_nums = data_gpio_nums;
            bus_config.bus_width = 8;
            bus_config.max_transfer_bytes = (LCD_WIDTH as usize) * (LCD_HEIGHT as usize) * 2;
            bus_config.psram_trans_align = 64;
            bus_config.sram_trans_align = 4;

            let mut bus: esp_lcd_i80_bus_handle_t = ptr::null_mut();
            esp!(esp_lcd_new_i80_bus(&bus_config, &mut bus))?;

            let mut io_config: esp_lcd_panel_io_i80_config_t = core::mem::zeroed();
            io_config.cs_gpio_num = LCD_CS as i32;
            io_config.pclk_hz = 10 * 1000 * 1000;
            io_config.trans_queue_depth = 2;
            io_config.lcd_cmd_bits = 8;
            io_config.lcd_param_bits = 8;
            io_config.dc_levels.set_dc_idle_level(0);
            io_config.dc_levels.set_dc_cmd_level(0);
            io_config.dc_levels.set_dc_dummy_level(0);
            io_config.dc_levels.set_dc_data_level(1);

            let mut io: esp_lcd_panel_io_handle_t = ptr::null_mut();
            esp!(esp_lcd_new_panel_io_i80(bus, &io_config, &mut io))?;

            let mut panel_config: esp_lcd_panel_dev_config_t = core::mem::zeroed();
            panel_config.reset_gpio_num = LCD_RST as i32;
            panel_config.__bindgen_anon_1.rgb_ele_order =
                sys::lcd_rgb_element_order_t_LCD_RGB_ELEMENT_ORDER_RGB;
            panel_config.bits_per_pixel = 16;

            let mut panel: esp_lcd_panel_handle_t = ptr::null_mut();
            esp!(esp_lcd_new_panel_st7789(io, &panel_config, &mut panel))?;

            esp!(esp_lcd_panel_reset(panel))?;
            esp!(esp_lcd_panel_init(panel))?;
            esp!(esp_lcd_panel_invert_color(panel, true))?;
            esp!(esp_lcd_panel_swap_xy(panel, true))?;
            esp!(esp_lcd_panel_mirror(panel, false, true))?;
            esp!(esp_lcd_panel_set_gap(panel, 0, 35))?;
            esp!(esp_lcd_panel_disp_on_off(panel, true))?;

            Ok(Self {
                _bus: bus,
                _io: io,
                panel,
            })
        }
    }

    /// Blit a full 320x170 RGB565 framebuffer to the display.
    pub fn draw_full(&mut self, fb: &[u16]) -> Result<(), EspError> {
        debug_assert_eq!(fb.len(), (LCD_WIDTH as usize) * (LCD_HEIGHT as usize));
        unsafe {
            esp!(esp_lcd_panel_draw_bitmap(
                self.panel,
                0,
                0,
                LCD_WIDTH as i32,
                LCD_HEIGHT as i32,
                fb.as_ptr() as *const _,
            ))
        }
    }
}

/// Pack 8-bit RGB into big-endian RGB565 for ST7789 8-bit i80 transfer.
#[inline]
pub const fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    let v = (((r as u16) & 0xF8) << 8) | (((g as u16) & 0xFC) << 3) | ((b as u16) >> 3);
    v.swap_bytes()
}

/// Fill a rectangle in the 320x170 RGB565 framebuffer.
pub fn fill_rect(fb: &mut [u16], x: i32, y: i32, w: i32, h: i32, color: u16) {
    let x0 = x.max(0).min(LCD_WIDTH as i32);
    let y0 = y.max(0).min(LCD_HEIGHT as i32);
    let x1 = (x + w).max(0).min(LCD_WIDTH as i32);
    let y1 = (y + h).max(0).min(LCD_HEIGHT as i32);
    for row in y0..y1 {
        let base = (row as usize) * (LCD_WIDTH as usize);
        for col in x0..x1 {
            fb[base + col as usize] = color;
        }
    }
}

/// Draw a thick ring (annulus) centered at `(cx, cy)`.
pub fn draw_ring(fb: &mut [u16], cx: i32, cy: i32, r_inner: i32, r_outer: i32, color: u16) {
    let r_in2 = r_inner * r_inner;
    let r_out2 = r_outer * r_outer;
    let y0 = (cy - r_outer).max(0);
    let y1 = (cy + r_outer).min(LCD_HEIGHT as i32 - 1);
    let x0 = (cx - r_outer).max(0);
    let x1 = (cx + r_outer).min(LCD_WIDTH as i32 - 1);
    for y in y0..=y1 {
        let dy = y - cy;
        let dy2 = dy * dy;
        let base = (y as usize) * (LCD_WIDTH as usize);
        for x in x0..=x1 {
            let dx = x - cx;
            let d2 = dx * dx + dy2;
            if d2 >= r_in2 && d2 <= r_out2 {
                fb[base + x as usize] = color;
            }
        }
    }
}
