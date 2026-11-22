use defmt::{debug, info};
use embassy_executor::{SendSpawner, SpawnToken};
use embassy_futures::select::{Either, select};
use embassy_rp::{Peripherals, peripherals::{self, PIN_15, PIO0}, pio::{Instance, Pio}, pio_programs::ws2812};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::{self, Channel}, signal::Signal};
use embassy_time::{Duration, Ticker, Timer};
use lamarrs_utils::{ColourRgb, LedSpecialAction, Params};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use smart_leds::{RGB8, SmartLedsWrite};

use crate::WS2812_12_LED_RING;

/// Events that worker tasks send to the WS2812_Channel
pub enum Ws2812LedRingEvents {
    ChangeColour(ColourRgb), // Request to change the colours. By now uses RGB, even tho RGBW is possible. 
}

pub const NUM_LEDS_RING: usize = 12;
// Channel to send colouring updates to the Ring
static UPDATE_12_LED_RING: channel::Channel<CriticalSectionRawMutex, RGB8, 10> =
    channel::Channel::new();
// Cancellation channel, this stops any task spawned previously.
static STOP_12_LED_RING: Signal<CriticalSectionRawMutex, bool> = Signal::new();

// Worker that handles a WS2812 leds.
#[embassy_executor::task]
pub async fn ws2812_12_led_ring(
    // spawner: SendSpawner,
    mut ws2812: PioWs2812<'static, embassy_rp::peripherals::PIO0, 1, NUM_LEDS_RING>,
) { 
    defmt::info!("WS2812 12 leds ring driver initialising");
    //spawner.spawn(ws2812_12_led_ring_handler(ws2812));
    let inbound = WS2812_12_LED_RING.receiver();
    // We won´t cancel any previous event processing as this is the first event or we didn´t spawn any
    // task before.
    // If we do send a cancel signal to the channel always, we will murder the event we just spawned.
    let mut previously_spawned_token= false;
    let mut current_colour = RGB8::default();
    loop {
        // Do nothing until we receive any event
        let event = inbound.receive().await;
        if previously_spawned_token { STOP_12_LED_RING.signal(false); previously_spawned_token=false}
        match event {
            Ws2812LedRingEvents::ChangeColour(colour_rgb) => {
                match colour_rgb.special_action {
                    Some(LedSpecialAction::Pulse(parameters)) => {
                        todo!();
                        // previously_spawned_token = true;
                        // spawner.spawn(process_pulse(colour_rgb.r, colour_rgb.g, colour_rgb.b, parameters));
                    }
                    Some(LedSpecialAction::Boom(parameters)) => {
                        todo!();
                        // previously_spawned_token = true;
                        // spawner.spawn(process_boom(colour_rgb.r, colour_rgb.g, colour_rgb.b, parameters));
                    }
                    Some(LedSpecialAction::Transition(parameters)) => {}
                    None => {
                        info!("Colouring WS2812 12 led ring R: {} G: {} B: {}", colour_rgb.r, colour_rgb.g, colour_rgb.b);
                        let new_colour = RGB8::new(colour_rgb.r, colour_rgb.g, colour_rgb.b);
                        fade_to_new_colour(&mut ws2812, current_colour, new_colour, 2000).await;
                        current_colour = new_colour;
                        // let mut data = [RGB8::default(); NUM_LEDS_RING];
                        // for led in 0..NUM_LEDS_RING {
                        //     data[led] = RGB8::new(colour_rgb.r, colour_rgb.g, colour_rgb.b);
                        // }
                        // ws2812.write(&data).await;
                    }
                };
            },
        }
    }
}

async fn fade_to_new_colour(
    ws2812: &mut PioWs2812<'static, embassy_rp::peripherals::PIO0, 1, NUM_LEDS_RING>,
    from: RGB8,
    to: RGB8,
    total_transition_duration_ms: u64,
) {
    let steps = 40; // higher = smoother
    let dt = total_transition_duration_ms / steps;

    info!("Fading into new colour");
    for i in 0..=steps {
        // check for cancellation
        if STOP_12_LED_RING.signaled() {
            return;
        }

        let t = i as f32 / steps as f32;

        let frame = RGB8 {
            r: linear_interpolation(from.r, to.r, t),
            g: linear_interpolation(from.g, to.g, t),
            b: linear_interpolation(from.b, to.b, t),
        };

        let mut data = [frame; NUM_LEDS_RING];
        ws2812.write(&data).await;

        Timer::after(Duration::from_millis(dt)).await;
    }
}

fn linear_interpolation(start: u8, end: u8, t: f32) -> u8 {
    (start as f32 + t * (end as f32 - start as f32)) as u8
}