//! Environment configuration for the Tang Nano FPGA miner.

/// Configuration for a Tang Nano connected over USB-UART.
#[derive(Debug, Clone)]
pub struct TangNano9kConfig {
    /// UART device path, e.g. `/dev/cu.usbserial-1101`.
    pub port: String,

    /// UART baud rate. The current FPGA bitstream defaults to 115200 8N1.
    pub baud: u32,
}

impl TangNano9kConfig {
    /// Build config from environment.
    ///
    /// Set `MUJINA_TANG_NANO_PORT` to enable this backend.
    /// Optional: `MUJINA_TANG_NANO_BAUD`, default `115200`.
    ///
    /// The older `MUJINA_TANG_NANO_9K_*` names are still accepted for existing
    /// scripts and shells.
    pub fn from_env() -> Option<Self> {
        let port = std::env::var("MUJINA_TANG_NANO_PORT")
            .or_else(|_| std::env::var("MUJINA_TANG_NANO_9K_PORT"))
            .ok()?;
        let baud = std::env::var("MUJINA_TANG_NANO_BAUD")
            .or_else(|_| std::env::var("MUJINA_TANG_NANO_9K_BAUD"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(115200);

        Some(Self { port, baud })
    }
}
