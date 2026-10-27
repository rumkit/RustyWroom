#![no_std]
#![no_main]

use core::cmp::Reverse;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{clock::CpuClock, timer::timg::TimerGroup};
use esp_println::println;
use esp_radio::wifi::{AuthenticationMethod, WifiController, scan::ScanConfig};

esp_bootloader_esp_idf::esp_app_desc!();

const SCAN_INTERVAL: Duration = Duration::from_secs(10);

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // The WiFi driver allocates buffers at runtime, so this example needs a
    // heap (Vec, Box...). esp-alloc lets us hand it regions of RAM:
    // - "reclaimed" RAM was used by the bootloader and is free once our app
    //   runs; it can't hold statics, but works fine as heap.
    // - a second, regular region for the rest.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // The scheduler must be running before the radio is initialized: the
    // WiFi driver runs its own background tasks on top of it.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Takes ownership of the WIFI peripheral and starts the radio in station
    // (client) mode, which is the mode that can scan.
    let mut wifi = WifiController::new(peripherals.WIFI, Default::default()).unwrap();
    let scan_config = ScanConfig::default().with_max(20);

    loop {
        // The radio hops through all channels listening for beacons; this
        // task sleeps until the driver reports "scan done".
        let mut networks = wifi.scan_async(&scan_config).await.unwrap();

        // Strongest first. RSSI is negative dBm: -40 is strong, -90 is weak.
        // Reverse(..) flips the ordering so higher values come first.
        networks.sort_unstable_by_key(|ap| Reverse(ap.signal_strength));

        println!();
        println!("Found {} networks:", networks.len());
        println!("{:>4}  {:>2}  {:<9}  {}", "dBm", "ch", "security", "SSID");
        for ap in &networks {
            println!(
                "{:>4}  {:>2}  {:<9}  {}",
                ap.signal_strength,
                ap.channel,
                security(ap.auth_method),
                // Hidden networks broadcast an empty name.
                if ap.ssid.as_str().is_empty() { "<hidden>" } else { ap.ssid.as_str() },
            );
        }

        Timer::after(SCAN_INTERVAL).await;
    }
}

/// Short label for a network's security type.
fn security(auth: Option<AuthenticationMethod>) -> &'static str {
    match auth {
        Some(AuthenticationMethod::None) => "open",
        Some(AuthenticationMethod::Wep) => "WEP",
        Some(AuthenticationMethod::Wpa) => "WPA",
        Some(AuthenticationMethod::Wpa2Personal) => "WPA2",
        Some(AuthenticationMethod::WpaWpa2Personal) => "WPA/WPA2",
        Some(AuthenticationMethod::Wpa3Personal) => "WPA3",
        Some(AuthenticationMethod::Wpa2Wpa3Personal) => "WPA2/WPA3",
        Some(AuthenticationMethod::Wpa2Enterprise) => "WPA2-Ent",
        // Anything rarer (WAPI, OWE...) or unknown.
        _ => "other",
    }
}
