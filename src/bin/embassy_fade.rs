#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embedded_hal::pwm::SetDutyCycle;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::DriveMode,
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

/// Brightness steps per half-cycle (off -> full or full -> off).
const STEPS: u32 = 200;
/// Time per step: 200 steps * 5 ms = 1 s fade in, 1 s fade out.
const STEP_TIME: Duration = Duration::from_millis(5);

// Unlike #[main], this entry point is an async fn: esp-rtos creates an
// executor and runs this function as its first task.
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-rtos needs a hardware timer to know when sleeping tasks should wake
    // up, plus a software interrupt to kick the scheduler. It takes ownership
    // of both, so nothing else can reconfigure them behind its back.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // LEDC setup is identical to pwm_fade.
    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut timer0 = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    timer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty13Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(5),
        })
        .unwrap();

    let mut led = ledc.channel(channel::Number::Channel0, peripherals.GPIO2);
    led.configure(channel::config::Config {
        timer: &timer0,
        duty_pct: 0,
        drive_mode: DriveMode::PushPull,
    })
    .unwrap();

    let max = u32::from(led.max_duty_cycle());
    println!("Embassy PWM fade started (max duty = {})", max);

    loop {
        for step in (0..STEPS).chain((1..=STEPS).rev()) {
            let duty = max * step * step / (STEPS * STEPS);
            led.set_duty_cycle(duty as u16).unwrap();
            // The only real change from pwm_fade: instead of spinning, this
            // task yields to the executor until the timer fires. With no other
            // tasks ready, the CPU just idles.
            Timer::after(STEP_TIME).await;
        }
    }
}
