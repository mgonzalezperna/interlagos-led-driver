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
use embassy_time::{with_timeout, Duration};
use smart_leds::RGB8;
use static_cell::StaticCell;

use telemetry::{Telemetry, MAX_PACKET_SIZE, UDP_PORT};

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
});

const WIFI_NETWORK: &str = ""; // change to your network SSID
const WIFI_PASSWORD: &str = ""; // change to your network password

/// Number of LEDs on the strip (data line on GPIO 10).
const NUM_LEDS: usize = 45;
/// Number of LEDs lit behind the current position, as a fading trail.
const TRAIL_LEN: usize = 6;
/// Peak value of each colour channel. 90 LEDs at full white can draw over 5 A,
/// so keep this low unless the strip has its own power supply.
const MAX_BRIGHTNESS: u8 = 40;
/// If no packet arrives for this long, the strip goes back to the idle display.
const TELEMETRY_TIMEOUT: Duration = Duration::from_millis(1000);
/// Time the strip stays on (and then off) while blinking during boot.
const BOOT_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
/// Boot blink colour. Every LED is lit, so keep it dim to limit current draw.
const BOOT_BLINK_COLOUR: RGB8 = RGB8::new(0, 0, MAX_BRIGHTNESS / 4);

/// Latest telemetry received over UDP. Only the newest sample matters, so a
/// `Signal` (which overwrites unread values) is used instead of a queue.
static TELEMETRY: Signal<CriticalSectionRawMutex, Telemetry> = Signal::new();

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

/// Draw the track position on the LED strip.
#[embassy_executor::task]
async fn led_task(mut leds: PioWs2812<'static, PIO0, 1, NUM_LEDS>) -> ! {
    let mut frame = [RGB8::default(); NUM_LEDS];

    let mut sample = Some(blink_until_first_packet(&mut leds, &mut frame).await);

    loop {
        frame.fill(RGB8::default());
        match sample {
            Some(sample) => render_position(&mut frame, sample.lap_pct),
            // No data for a while: show a single dim red LED at the start/finish line.
            None => frame[0] = RGB8::new(MAX_BRIGHTNESS / 4, 0, 0),
        }
        leds.write(&frame).await;

        sample = with_timeout(TELEMETRY_TIMEOUT, TELEMETRY.wait()).await.ok();
    }
}

/// Blink the whole strip while booting (Wi-Fi join, DHCP, waiting for SimHub),
/// and return the first telemetry sample once it arrives.
async fn blink_until_first_packet(
    leds: &mut PioWs2812<'static, PIO0, 1, NUM_LEDS>,
    frame: &mut [RGB8; NUM_LEDS],
) -> Telemetry {
    let mut lit = true;
    loop {
        defmt::info!("Trying ...");
        let colour = if lit {
            BOOT_BLINK_COLOUR
        } else {
            RGB8::default()
        };
        defmt::info!("to ...");
        frame.fill(colour);
        defmt::info!("lit ...");
        leds.write(frame).await;
        defmt::info!("leds ...");
        lit = !lit;

        // Waiting on the signal doubles as the blink delay, so the first packet
        // ends the blinking immediately instead of after the current half-period.
        if let Ok(sample) = with_timeout(BOOT_BLINK_HALF_PERIOD, TELEMETRY.wait()).await {
            defmt::info!("First telemetry packet received");
            return sample;
        }
    }
}

/// Light the LED at `lap_pct` (0.0 to 1.0) with a fading trail behind it.
fn render_position(frame: &mut [RGB8; NUM_LEDS], lap_pct: f32) {
    let pct = lap_pct.clamp(0.0, 1.0);
    let head = ((pct * NUM_LEDS as f32) as usize).min(NUM_LEDS - 1);

    for i in 0..=TRAIL_LEN {
        // Trail wraps around the end of the strip.
        let index = (head + NUM_LEDS - i) % NUM_LEDS;
        let level = (MAX_BRIGHTNESS as usize * (TRAIL_LEN + 1 - i) / (TRAIL_LEN + 1)) as u8;
        frame[index] = RGB8::new(0, level, 0);
    }
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
    }

    spawner.spawn(udp_task(stack)).unwrap();
}
