#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    watch::{Receiver, Watch},
};
use embassy_time::{Duration, Timer};
use embedded_hal::pwm::SetDutyCycle;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{DriveMode, Input, InputConfig, Pull},
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, Channel, ChannelIFace},
        timer::{self, TimerIFace},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// Brightness steps per half-cycle (off -> full or full -> off).
const STEPS: u32 = 200;
/// How long to ignore the button after it changes state (contact bounce).
const DEBOUNCE: Duration = Duration::from_millis(50);

/// The fade speeds the button cycles through.
// Copy: it's just a tag, so it can be passed around by value like an int.
// Debug: lets println! print it with {:?}.
#[derive(Clone, Copy, Debug)]
enum Speed {
    Slow,
    Medium,
    Fast,
}

impl Speed {
    /// The speed after this one, wrapping around.
    fn next(self) -> Self {
        // `match` must cover every variant; adding a 4th speed without
        // handling it here is a compile error, not a silent bug.
        match self {
            Speed::Slow => Speed::Medium,
            Speed::Medium => Speed::Fast,
            Speed::Fast => Speed::Slow,
        }
    }

    /// Time per brightness step. A full in+out cycle is 2 * STEPS steps.
    fn step_time(self) -> Duration {
        match self {
            Speed::Slow => Duration::from_millis(10),  // 4 s cycle
            Speed::Medium => Duration::from_millis(5), // 2 s cycle
            Speed::Fast => Duration::from_micros(1250), // 0.5 s cycle
        }
    }
}

/// Number of tasks that read SPEED: the fade task and main.
const SPEED_RECEIVERS: usize = 2;

/// The current speed. The button task sends; every receiver gets told about
/// each change on its own (unlike a Signal, where one read consumes it).
/// `new_with` gives it a starting value, so readers never see "nothing yet".
static SPEED: Watch<CriticalSectionRawMutex, Speed, SPEED_RECEIVERS> =
    Watch::new_with(Speed::Medium);

static LEDC_TIMER: StaticCell<timer::Timer<'static, LowSpeed>> = StaticCell::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let mut ledc = Ledc::new(peripherals.LEDC);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let timer0 = LEDC_TIMER.init(ledc.timer::<LowSpeed>(timer::Number::Timer0));
    timer0
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty13Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_khz(5),
        })
        .unwrap();
    let timer0: &'static _ = timer0;

    let mut led = ledc.channel(channel::Number::Channel0, peripherals.GPIO2);
    led.configure(channel::config::Config {
        timer: timer0,
        duty_pct: 0,
        drive_mode: DriveMode::PushPull,
    })
    .unwrap();

    // BOOT button: connects GPIO0 to ground when pressed, so the pin reads
    // High when released and Low when pressed. The pull-up keeps it High
    // instead of floating while released.
    let button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

    // receiver() returns None once SPEED_RECEIVERS receivers exist, so asking
    // for one too many is caught here rather than silently missing updates.
    spawner.spawn(fade(led, SPEED.receiver().unwrap()).unwrap());
    spawner.spawn(button_task(button).unwrap());

    // main only reports changes. changed().await parks it until the button
    // task sends a new speed; in the meantime it costs no CPU at all.
    let mut speed_rx = SPEED.receiver().unwrap();
    // The initial value doesn't count as a "change", so report it explicitly.
    // get() returns the current value right away (it only waits while empty).
    println!("speed: {:?} (press BOOT to change)", speed_rx.get().await);
    loop {
        let speed = speed_rx.changed().await;
        println!("speed: {:?}", speed);
    }
}

/// Fades the LED in and out forever, at whatever speed SPEED currently says.
#[embassy_executor::task]
async fn fade(
    mut led: Channel<'static, LowSpeed>,
    mut speed_rx: Receiver<'static, CriticalSectionRawMutex, Speed, SPEED_RECEIVERS>,
) {
    let max = u32::from(led.max_duty_cycle());
    let mut step_time = speed_rx.get().await.step_time();

    loop {
        for step in (0..STEPS).chain((1..=STEPS).rev()) {
            // Non-blocking check: Some(new speed) only if it changed since we
            // last looked. Only the step time changes, so the fade carries on
            // from its current brightness instead of jumping.
            if let Some(speed) = speed_rx.try_changed() {
                step_time = speed.step_time();
            }

            let duty = max * step * step / (STEPS * STEPS);
            led.set_duty_cycle(duty as u16).unwrap();
            Timer::after(step_time).await;
        }
    }
}

/// Waits for button presses and moves SPEED to the next speed on each one.
#[embassy_executor::task]
async fn button_task(mut button: Input<'static>) {
    let sender = SPEED.sender();
    let mut speed = Speed::Medium; // matches SPEED's initial value

    loop {
        // Sleeps until the pin goes High -> Low (pressed); a GPIO interrupt
        // wakes the task, so there is no polling.
        button.wait_for_falling_edge().await;
        // A mechanical switch "bounces" between High and Low for a few ms.
        // Ignore that noise so one press doesn't count as several.
        Timer::after(DEBOUNCE).await;

        speed = speed.next();
        sender.send(speed);

        // Holding the button counts as one press: wait for the release,
        // then let the release bounce settle too.
        button.wait_for_high().await;
        Timer::after(DEBOUNCE).await;
    }
}
