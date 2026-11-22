//! Telemetry packet sent from the PC (SimHub) to the Pico over UDP.
//!
//! The packet is encoded with `postcard`. For this struct that means a fixed
//! 9 byte layout, which is easy to produce from any language:
//!
//! | offset | size | type | field       | notes                          |
//! |--------|------|------|-------------|--------------------------------|
//! | 0      | 4    | f32  | `lap_pct`   | little endian, 0.0 ..= 1.0     |
//! | 4      | 4    | f32  | `speed_kmh` | little endian                  |
//! | 8      | 1    | u8   | `lap`       | current lap, wraps after 255   |
//!
//! Do not add integer fields wider than `u8` without updating the sender:
//! postcard encodes those as variable-length integers.

use serde::{Deserialize, Serialize};

/// UDP port the Pico listens on.
pub const UDP_PORT: u16 = 5005;

/// UDP port for manually lighting a single LED, e.g. to count the LEDs on a strip.
///
/// The packet is the LED index as ASCII decimal text (0-based, surrounding
/// whitespace ignored), so it can be sent by hand:
///
/// ```text
/// echo 12 | nc -u -w1 <pico-ip> 5006
/// ```
///
/// The Pico replies with a short text message saying whether it worked. The LED
/// stays lit until another index or a telemetry packet arrives.
pub const LED_TEST_PORT: u16 = 5006;

/// Size of the largest packet we accept.
pub const MAX_PACKET_SIZE: usize = 32;

// This file is also compiled on the PC by the `sender` crate, where defmt is not
// available, so the defmt derive is only applied on the bare-metal target.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[cfg_attr(target_os = "none", derive(defmt::Format))]
pub struct Telemetry {
    /// Position on the lap, 0.0 (start/finish line) to 1.0.
    pub lap_pct: f32,
    pub speed_kmh: f32,
    pub lap: u8,
}
