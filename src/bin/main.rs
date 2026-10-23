#![no_std] // no standard library: there is no OS underneath us
#![no_main] // the runtime calls our #[main] fn instead of the usual Rust entry point

use esp_backtrace as _; // links in a panic handler that prints a backtrace
use esp_hal::{
    clock::CpuClock,
    gpio::{Level, Output, OutputConfig},
    main,
    time::{Duration, Instant},
};
use esp_println::println;

// Image header the ESP-IDF bootloader checks before jumping into our app.
esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    // Bring up clocks and take ownership of all peripherals (singletons).
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Onboard blue LED on most WROOM-32 DevKit boards is on GPIO2.
    let mut led = Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default());

    println!("Blinky started");

    loop {
        led.toggle();
        println!("LED is {}", if led.is_set_high() { "ON" } else { "OFF" });
        wait(Duration::from_millis(500));
    }
}

/// Busy-wait for the given duration using the HAL's monotonic timer.
fn wait(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {}
}
