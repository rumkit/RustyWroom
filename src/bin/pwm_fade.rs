#![no_std]
#![no_main]

use embedded_hal::pwm::SetDutyCycle; // trait must be in scope to call its methods
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::DriveMode,
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
    },
    main,
    time::{Duration, Instant, Rate},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

/// Brightness steps per half-cycle (off -> full or full -> off).
const STEPS: u32 = 200;
/// Time per step: 200 steps * 5 ms = 1 s fade in, 1 s fade out.
const STEP_TIME: Duration = Duration::from_millis(5);

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // LEDC = the ESP32's "LED controller" PWM peripheral.
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    // A timer sets the PWM frequency and resolution; channels share timers.
    // 5 kHz is far above what the eye can see; 13 bits = 8192 duty levels.
    let mut timer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    timer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty13Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(5),
        })
        .unwrap();

    // A channel drives one pin, using a timer's frequency plus its own duty.
    let mut led = ledc.channel(channel::Number::Channel0, peripherals.GPIO2);
    led.configure(channel::config::Config {
        timer: &timer0,
        duty_pct: 0,
        drive_mode: DriveMode::PushPull,
    })
    .unwrap();

    let max = u32::from(led.max_duty_cycle());
    println!("PWM fade started (max duty = {})", max);

    loop {
        // Ramp up 0..STEPS, then back down STEPS..1, so the ends aren't repeated.
        for step in (0..STEPS).chain((1..=STEPS).rev()) {
            // Eyes perceive brightness roughly logarithmically, so a linear
            // duty ramp looks like it jumps to bright and then barely changes.
            // Squaring the position (simple gamma) makes the fade look even.
            let duty = max * step * step / (STEPS * STEPS);
            led.set_duty_cycle(duty as u16).unwrap();
            wait(STEP_TIME);
        }
    }
}

/// Busy-wait for the given duration using the HAL's monotonic timer.
fn wait(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {}
}
