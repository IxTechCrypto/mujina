//! NerdQX Rev 1.0 verified pinout and bus addresses.

pub const UART1_TX: u8 = 17;
pub const UART1_RX: u8 = 18;
pub const ASIC_RST: u8 = 1;

pub const I2C0_SDA: u8 = 44;
pub const I2C0_SCL: u8 = 43;

pub const PWR_EN: u8 = 10;
pub const VR_RDY: u8 = 11;
pub const LDO_EN: u8 = 13;

/// TMP451 mux select lines.
pub const TMUX_A0: u8 = 2;
pub const TMUX_A1: u8 = 3;

/// ST7789 8-bit i80 data lines D0..D7.
pub const LCD_DATA: [u8; 8] = [39, 40, 41, 42, 45, 46, 47, 48];
pub const LCD_WR: u8 = 8;
pub const LCD_RD: u8 = 9;
pub const LCD_CS: u8 = 6;
pub const LCD_DC: u8 = 7;
pub const LCD_RST: u8 = 5;
pub const LCD_BL: u8 = 38;
pub const LCD_PWR: u8 = 15;

pub const BTN1: u8 = 14;
pub const BTN2: u8 = 0;

pub const ADDR_TPS53647: u8 = 0x71;
pub const ADDR_TMP451: u8 = 0x4C;
pub const ADDR_TMP1075: [u8; 2] = [0x48, 0x49];
pub const ADDR_EMC2302: u8 = 0x2E;

pub const AF_BOARD_TAG: &str = "MUJINA-BOARD:nerdqx;";
pub const OTA_ENDPOINT: &str = "/api/v0/firmware/ota";

pub const LCD_WIDTH: u16 = 320;
pub const LCD_HEIGHT: u16 = 170;
