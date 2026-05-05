//! Tang Nano FPGA miner backend.
//!
//! This backend speaks the tiny UART protocol used by the open Tang Nano
//! bitstream: `TNJ || midstate || tail || target`, then waits
//! for `F || nonce || hash`.

pub mod config;
pub mod thread;

pub use config::TangNano9kConfig;
pub use thread::TangNano9kHashThread;
