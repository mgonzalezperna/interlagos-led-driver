#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]

mod telemetry;

use cyw43::JoinOptions;
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, PIO1};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use core::fmt::Write as _;

use embassy_futures::select::{select3, Either3};
use embassy_time::{Duration, Timer};
use smart_leds::RGB8;
use static_cell::StaticCell;

use telemetry::{Telemetry, LED_TEST_PORT, MAX_PACKET_SIZE, UDP_PORT};

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
});

const WIFI_NETWORK: &str = ""; // change to your network SSID
const WIFI_PASSWORD: &str = ""; // change to your network password

/// Number of addressable pixels on the strip (data line on GPIO 10). Each pixel
/// drives a group of three physical LEDs, so 90 LEDs give 30 pixels.
const NUM_LEDS: usize = 31;
/// Number of pixels lit behind the current position, as a fading trail.
const TRAIL_LEN: usize = 1;
/// Peak value of each colour channel. 90 LEDs at full white can draw over 5 A,
/// so keep this low unless the strip has its own power supply.
const MAX_BRIGHTNESS: u8 = 40;
/// If no packet arrives for this long, the strip goes back to the idle display.
const TELEMETRY_TIMEOUT: Duration = Duration::from_millis(1000);
/// Time the strip stays on (and then off) while blinking during boot.
const BOOT_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
/// Boot blink colour. Every LED is lit, so keep it dim to limit current draw.
const BOOT_BLINK_COLOUR: RGB8 = RGB8::new(0, 0, MAX_BRIGHTNESS / 4);
/// Colour of the LED lit via the LED test port. White, so it stands out.
const TEST_LED_COLOUR: RGB8 = RGB8::new(MAX_BRIGHTNESS, MAX_BRIGHTNESS, MAX_BRIGHTNESS);

/// Latest telemetry received over UDP. Only the newest sample matters, so a
/// `Signal` (which overwrites unread values) is used instead of a queue.
static TELEMETRY: Signal<CriticalSectionRawMutex, Telemetry> = Signal::new();
/// Index of the LED to light, received on the LED test port.
static LED_TEST: Signal<CriticalSectionRawMutex, usize> = Signal::new();

/// Create a new instance of the CYW43 driver.
#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

/// Start the Network stack.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

/// Receive telemetry packets from SimHub and publish the latest one.
#[embassy_executor::task]
async fn udp_task(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buffer = [0u8; 256];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buffer = [0u8; 16];
    let mut packet = [0u8; MAX_PACKET_SIZE];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(UDP_PORT).unwrap();
    defmt::info!("Listening for telemetry on UDP port {}", UDP_PORT);

    loop {
        let (len, _sender) = match socket.recv_from(&mut packet).await {
            Ok(received) => received,
            Err(_) => {
                // Packet was larger than our buffer, ignore it.
                defmt::warn!("Dropped oversized packet");
                continue;
            }
        };

        match postcard::from_bytes::<Telemetry>(&packet[..len]) {
            Ok(sample) if sample.lap_pct.is_finite() => TELEMETRY.signal(sample),
            Ok(_) => defmt::warn!("Ignoring packet with invalid lap position"),
            Err(_) => defmt::warn!("Ignoring undecodable packet of {} bytes", len),
        }
    }
}

/// Receive manual "light LED N" commands as ASCII text and reply with the result.
#[embassy_executor::task]
async fn led_test_task(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0u8; 64];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0u8; 128];
    let mut packet = [0u8; 16];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(LED_TEST_PORT).unwrap();
    defmt::info!("Listening for LED test commands on UDP port {}", LED_TEST_PORT);

    loop {
        let (len, sender) = match socket.recv_from(&mut packet).await {
            Ok(received) => received,
            Err(_) => {
                defmt::warn!("Dropped oversized LED test packet");
                continue;
            }
        };

        let index = core::str::from_utf8(&packet[..len])
            .ok()
            .and_then(|text| text.trim().parse::<usize>().ok());

        let mut reply: heapless::String<64> = heapless::String::new();
        // The reply always fits in 64 bytes, so the write cannot fail.
        let _ = match index {
            Some(index) if index < NUM_LEDS => {
                LED_TEST.signal(index);
                defmt::info!("LED test: lighting LED {}", index);
                writeln!(reply, "LED {} on", index)
            }
            Some(index) => {
                defmt::warn!("LED test: index {} out of range", index);
                writeln!(reply, "error: {} out of range, NUM_LEDS is {}", index, NUM_LEDS)
            }
            None => writeln!(reply, "error: send an LED index as text, e.g. 12"),
        };
        // Best effort: the command already took effect, a lost reply doesn't matter.
        let _ = socket.send_to(reply.as_bytes(), sender).await;
    }
}

/// What the LED strip is currently showing.
#[derive(Clone, Copy)]
enum Display {
    /// Blinking the whole strip until the first packet arrives.
    Booting { lit: bool },
    /// Showing the car's position from the latest telemetry.
    Position(Telemetry),
    /// Telemetry stopped: a single dim red LED at the start/finish line.
    Idle,
    /// A single LED chosen via the LED test port.
    TestLed(usize),
}

enum Event {
    Telemetry(Telemetry),
    TestLed(usize),
    Timeout,
}

/// Wait for the next telemetry sample or LED test command, or until `timeout`
/// expires (`None` waits forever).
async fn next_event(timeout: Option<Duration>) -> Event {
    let timer = async {
        match timeout {
            Some(timeout) => Timer::after(timeout).await,
            None => core::future::pending().await,
        }
    };
    match select3(TELEMETRY.wait(), LED_TEST.wait(), timer).await {
        Either3::First(sample) => Event::Telemetry(sample),
        Either3::Second(index) => Event::TestLed(index),
        Either3::Third(()) => Event::Timeout,
    }
}

/// Drive the LED strip: boot blink, track position, idle, or a test LED.
#[embassy_executor::task]
async fn led_task(mut leds: PioWs2812<'static, PIO0, 1, NUM_LEDS>) -> ! {
    let mut frame = [RGB8::default(); NUM_LEDS];
    let mut display = Display::Booting { lit: true };

    loop {
        frame.fill(RGB8::default());
        match display {
            Display::Booting { lit: true } => frame.fill(BOOT_BLINK_COLOUR),
            Display::Booting { lit: false } => {}
            Display::Position(sample) => render_position(&mut frame, sample.lap_pct),
            Display::Idle => frame[0] = RGB8::new(MAX_BRIGHTNESS / 4, 0, 0),
            Display::TestLed(index) => frame[index] = TEST_LED_COLOUR,
        }
        leds.write(&frame).await;

        // Only the boot blink and live telemetry change on their own. A test LED
        // stays lit until the next command or telemetry packet.
        let timeout = match display {
            Display::Booting { .. } => Some(BOOT_BLINK_HALF_PERIOD),
            Display::Position(_) => Some(TELEMETRY_TIMEOUT),
            Display::Idle | Display::TestLed(_) => None,
        };

        display = match next_event(timeout).await {
            Event::Telemetry(sample) => Display::Position(sample),
            Event::TestLed(index) => Display::TestLed(index),
            Event::Timeout => match display {
                Display::Booting { lit } => Display::Booting { lit: !lit },
                _ => Display::Idle,
            },
        };
    }
}

/// Light the pixel at `lap_pct` (0.0 to 1.0) with a fading trail behind it, and
/// let the next pixel ahead glow up as the position approaches it.
///
/// Brightness falls off with the square of the distance from the head. Perceived
/// brightness is non-linear, so a linear fade makes the tail look like it stays
/// bright; a squared fade looks like an even fade. The pixel ahead uses the same
/// curve: it starts dark when the position enters the head pixel and reaches full
/// brightness at the moment the position arrives at it.
fn render_position(frame: &mut [RGB8; NUM_LEDS], lap_pct: f32) {
    let position = lap_pct.clamp(0.0, 1.0) * NUM_LEDS as f32;
    let head = (position as usize).min(NUM_LEDS - 1);
    // How far the position has progressed through the head pixel, 0.0 ..= 1.0.
    let progress = (position - head as f32).clamp(0.0, 1.0);

    let steps = TRAIL_LEN + 1;
    for i in 0..=TRAIL_LEN {
        // Trail wraps around the end of the strip.
        let index = (head + NUM_LEDS - i) % NUM_LEDS;
        let remaining = steps - i;
        let level = (MAX_BRIGHTNESS as usize * remaining * remaining / (steps * steps)) as u8;
        frame[index] = RGB8::new(0, level, 0);
    }

    // Pixel ahead of the head, wrapping from the end of the strip to the start.
    let ahead = (head + 1) % NUM_LEDS;
    let level = (MAX_BRIGHTNESS as f32 * progress * progress) as u8;
    frame[ahead] = RGB8::new(0, level, 0);
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("Booting up!");

    let peripherals = embassy_rp::init(Default::default());
    defmt::debug!("Peripherals initialised successfully.");

    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");
    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download 43439A0.bin --binary-format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs download 43439A0_clm.bin --binary-format bin --chip RP2040 --base-address 0x10140000
    //let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 230321) };
    //let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    // LED strip on PIO1 (PIO0 is used by the Wi-Fi chip) with its own DMA channel.
    let mut pio0 = Pio::new(peripherals.PIO0, Irqs);
    let ws2812_program = PioWs2812Program::new(&mut pio0.common);
    let mut leds = PioWs2812::<_, 1, NUM_LEDS>::new(
        &mut pio0.common,
        pio0.sm1,
        peripherals.DMA_CH1,
        peripherals.PIN_10,
        &ws2812_program,
    );
    let mut frame = [RGB8::default(); NUM_LEDS];
    frame.fill(BOOT_BLINK_COLOUR);
    leds.write(&frame).await;
    spawner.spawn(led_task(leds)).unwrap();

    let pwr = Output::new(peripherals.PIN_23, Level::Low);
    let cs = Output::new(peripherals.PIN_25, Level::High);
    let spi = PioSpi::new(
        &mut pio0.common,
        pio0.sm0, // Here we are using sm0 from PIO0. Remember, there are 4sm per PIO, and 2 PIOs (PIO0 and PIO1)
        DEFAULT_CLOCK_DIVIDER,
        pio0.irq0,
        cs,
        peripherals.PIN_24,
        peripherals.PIN_29,
        peripherals.DMA_CH0,
    );

    // Start the networking driver.
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    spawner.spawn(cyw43_task(runner)).unwrap();

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::Performance)
        .await;

    let config = Config::dhcpv4(Default::default());
    // Use static IP configuration instead of DHCP
    //let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
    //    address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 69, 2), 24),
    //    dns_servers: Vec::new(),
    //    gateway: Some(Ipv4Address::new(192, 168, 69, 1)),
    //});

    // Generate random seed
    let mut rng = RoscRng;
    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net_task(runner)).unwrap();

    while let Err(err) = control
        .join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
        .await
    {
        defmt::info!("Join failed with status={}", err.status);
    }

    defmt::info!("Waiting for link...");
    stack.wait_link_up().await;

    defmt::info!("Waiting for DHCP...");
    stack.wait_config_up().await;

    // And now we can use it!
    defmt::info!("Stack is up!");
    if let Some(config) = stack.config_v4() {
        defmt::info!(
            "Send telemetry to {}:{}",
            config.address.address(),
            UDP_PORT
        );
        defmt::info!(
            "Light a single LED with: echo <index> | nc -u -w1 {} {}",
            config.address.address(),
            LED_TEST_PORT
        );
    }

    spawner.spawn(udp_task(stack)).unwrap();
    spawner.spawn(led_test_task(stack)).unwrap();
}
