//! Simulated telemetry sender.
//!
//! Drives a virtual car around a track and sends the same UDP packets the Pico
//! expects from SimHub, so the LED strip can be tested without running a game.
//!
//! Usage: interlagos-telemetry-sender <pico-ip> [--port N] [--rate HZ] [--lap-time SECS]

// Single source of truth for the packet format, shared with the firmware.
#[path = "../../firmware/src/telemetry.rs"]
mod telemetry;

use std::f32::consts::TAU;
use std::net::UdpSocket;
use std::process::exit;
use std::thread::sleep;
use std::time::{Duration, Instant};

use telemetry::{Telemetry, MAX_PACKET_SIZE, UDP_PORT};

/// Interlagos lap length in metres.
const TRACK_LENGTH_M: f32 = 4309.0;
const MIN_SPEED_KMH: f32 = 90.0;
const MAX_SPEED_KMH: f32 = 290.0;

struct Options {
    pico_ip: String,
    port: u16,
    rate_hz: f32,
    /// Approximate lap time, used to scale the simulated speed.
    lap_time_s: f32,
}

fn usage() -> ! {
    eprintln!(
        "Usage: interlagos-telemetry-sender <pico-ip> [--port N] [--rate HZ] [--lap-time SECS]"
    );
    eprintln!("  --port      UDP port on the Pico (default {UDP_PORT})");
    eprintln!("  --rate      packets per second (default 60)");
    eprintln!("  --lap-time  simulated lap time in seconds (default 70)");
    exit(2);
}

fn parse_args() -> Options {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        pico_ip: String::new(),
        port: UDP_PORT,
        rate_hz: 60.0,
        lap_time_s: 70.0,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => options.port = value(&mut args, &arg),
            "--rate" => options.rate_hz = value(&mut args, &arg),
            "--lap-time" => options.lap_time_s = value(&mut args, &arg),
            "-h" | "--help" => usage(),
            _ if arg.starts_with('-') => {
                eprintln!("Unknown option: {arg}");
                usage();
            }
            _ if options.pico_ip.is_empty() => options.pico_ip = arg,
            _ => usage(),
        }
    }

    if options.pico_ip.is_empty() || options.rate_hz <= 0.0 || options.lap_time_s <= 0.0 {
        usage();
    }
    options
}

fn value<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, name: &str) -> T {
    args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        eprintln!("Missing or invalid value for {name}");
        usage();
    })
}

/// Speed at a position on the lap: three fast sections and three slow corners.
fn profile_kmh(lap_pct: f32) -> f32 {
    let wave = (lap_pct * 3.0 * TAU).sin() * 0.5 + 0.5; // 0.0 ..= 1.0
    MIN_SPEED_KMH + wave * (MAX_SPEED_KMH - MIN_SPEED_KMH)
}

/// A car that speeds up and slows down around the lap.
struct Car {
    lap_pct: f32,
    lap: u8,
    /// Scales speed so one lap takes roughly `lap_time_s`.
    speed_scale: f32,
}

impl Car {
    fn new(lap_time_s: f32) -> Self {
        // Time to drive one lap with the unscaled profile. The car spends longer
        // in the slow sections, so this is not simply length / average speed.
        const STEPS: usize = 10_000;
        let unscaled_lap_time_s: f32 = (0..STEPS)
            .map(|i| {
                let speed_ms = profile_kmh((i as f32 + 0.5) / STEPS as f32) / 3.6;
                TRACK_LENGTH_M / STEPS as f32 / speed_ms
            })
            .sum();
        Self {
            lap_pct: 0.0,
            lap: 1,
            speed_scale: unscaled_lap_time_s / lap_time_s,
        }
    }

    fn speed_kmh(&self) -> f32 {
        profile_kmh(self.lap_pct) * self.speed_scale
    }

    fn step(&mut self, dt_s: f32) -> Telemetry {
        let speed_kmh = self.speed_kmh();
        self.lap_pct += speed_kmh / 3.6 * dt_s / TRACK_LENGTH_M;
        while self.lap_pct >= 1.0 {
            self.lap_pct -= 1.0;
            self.lap = self.lap.wrapping_add(1);
        }
        Telemetry {
            lap_pct: self.lap_pct,
            speed_kmh,
            lap: self.lap,
        }
    }
}

fn main() {
    let options = parse_args();
    let target = format!("{}:{}", options.pico_ip, options.port);

    let socket = UdpSocket::bind("0.0.0.0:0").expect("could not open a UDP socket");
    socket
        .connect(&target)
        .unwrap_or_else(|e| panic!("invalid Pico address {target}: {e}"));

    println!(
        "Sending telemetry to {target} at {} Hz, ~{} s laps. Ctrl+C to stop.",
        options.rate_hz, options.lap_time_s
    );

    let interval = Duration::from_secs_f32(1.0 / options.rate_hz);
    let mut car = Car::new(options.lap_time_s);
    let mut buffer = [0u8; MAX_PACKET_SIZE];
    let mut last = Instant::now();
    let mut last_report = Instant::now();

    loop {
        let now = Instant::now();
        let sample = car.step((now - last).as_secs_f32());
        last = now;

        let packet = postcard::to_slice(&sample, &mut buffer).expect("packet does not fit buffer");
        if let Err(e) = socket.send(packet) {
            eprintln!("send failed: {e}");
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            println!(
                "lap {:>3}  position {:>5.1}%  speed {:>5.1} km/h",
                sample.lap,
                sample.lap_pct * 100.0,
                sample.speed_kmh
            );
            last_report = Instant::now();
        }

        sleep(interval.saturating_sub(now.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_has_the_documented_wire_format() {
        let mut buffer = [0u8; MAX_PACKET_SIZE];
        let packet = postcard::to_slice(
            &Telemetry {
                lap_pct: 0.5,
                speed_kmh: 100.0,
                lap: 7,
            },
            &mut buffer,
        )
        .unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&0.5f32.to_le_bytes());
        expected.extend_from_slice(&100.0f32.to_le_bytes());
        expected.push(7);
        assert_eq!(packet, expected.as_slice());
    }

    #[test]
    fn car_completes_a_lap_in_about_the_requested_time() {
        let mut car = Car::new(70.0);
        let dt = 0.01;
        let mut elapsed = 0.0;
        while car.lap == 1 {
            let sample = car.step(dt);
            assert!((0.0..1.0).contains(&sample.lap_pct));
            elapsed += dt;
        }
        assert!((elapsed - 70.0).abs() < 5.0, "lap took {elapsed} s");
    }
}
