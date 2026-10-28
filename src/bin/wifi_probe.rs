#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{
    Runner, StackResources,
    dns::DnsQueryType,
    icmp::{
        PacketMetadata,
        ping::{PingError, PingManager, PingParams},
    },
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal::pwm::SetDutyCycle;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::DriveMode,
    ledc::{
        LSGlobalClkSource, Ledc, LowSpeed,
        channel::{self, Channel, ChannelIFace},
        timer::{self, TimerIFace},
    },
    peripherals::WIFI,
    rng::Rng,
    rtc_cntl::{
        SocResetReason, reset_reason,
        sleep::{LowPower, RtcSleepConfig},
        wakeup_cause,
    },
    system::Cpu,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ConnectionError, Interface, WifiController, WifiError,
    sta::StationConfig,
};
use portable_atomic::{AtomicU32, Ordering};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

// Baked in at compile time from .cargo/secrets.toml (Cargo [env]). The second
// argument is the compile error shown if a variable is missing (it must be a
// string literal, hence the repetition).
const SSID: &str = env!("RUSTYWROOM_SSID", "copy .cargo/secrets.example.toml to .cargo/secrets.toml and fill it in");
const PASSWORD: &str = env!("RUSTYWROOM_PASSWORD", "copy .cargo/secrets.example.toml to .cargo/secrets.toml and fill it in");
const PROBE_HOST: &str = env!("RUSTYWROOM_PROBE_HOST", "copy .cargo/secrets.example.toml to .cargo/secrets.toml and fill it in");
// An empty value passes env!, so check that at compile time too. (An empty
// PASSWORD is fine: it means an open network.)
const _: () = assert!(!SSID.is_empty(), "RUSTYWROOM_SSID is empty in .cargo/secrets.toml");
const _: () = assert!(!PROBE_HOST.is_empty(), "RUSTYWROOM_PROBE_HOST is empty in .cargo/secrets.toml");

/// Deep sleep between probes.
const SLEEP_SECS: u64 = 60;
/// How long to blink after a failure before going to sleep anyway (the next
/// wake retries).
const ERROR_DISPLAY_TIME: Duration = Duration::from_secs(10);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const PING_COUNT: u16 = 3;
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Wakes since the last non-sleep reset. Lives in RTC fast memory, which
/// stays powered in deep sleep. `persistent`: not initialized at startup
/// (main resets it unless we woke from deep sleep). Atomic so it can be a
/// plain (non-`mut`) static: no `unsafe`.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static WAKE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Raised once, on the first failure: switches the LED from fading to
/// blinking. A Signal fits a one-shot event better than a Watch: there is no
/// value to share, and only the LED task cares.
static ERROR: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// DHCP client + DNS client + ICMP (ping) socket.
static STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
static LEDC_TIMER: StaticCell<timer::Timer<'static, LowSpeed>> = StaticCell::new();

/// Why a probe failed. Each variant wraps the underlying error, so the log
/// shows the details (e.g. the WiFi disconnect reason).
#[derive(Debug)]
#[allow(dead_code)] // the fields are only read through Debug
enum ProbeError {
    Wifi(WifiError),
    Connect(ConnectionError),
    Timeout(&'static str),
    Dns(embassy_net::dns::Error),
    NoAddress,
    Ping(PingError),
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // esp-hal zeroes persistent memory only after a ChipPowerOn reset. Other
    // resets (e.g. the one after flashing) leave whatever bits were there, so
    // trust the counter only when we're waking from our own deep sleep.
    let reset = reset_reason(Cpu::ProCpu);
    if reset != Some(SocResetReason::CoreDeepSleep) {
        WAKE_COUNT.store(0, Ordering::Relaxed);
    }
    // fetch_add returns the old value.
    let wake = WAKE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    println!();
    println!(
        "=== wifi_probe: wake #{} (reset: {:?}, wakeup: {:?}) ===",
        wake,
        reset,
        wakeup_cause()
    );

    // LED: same LEDC setup as the fade examples.
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
    spawner.spawn(led_task(led).unwrap());

    match probe(spawner, peripherals.WIFI).await {
        Ok(()) => println!("result: OK"),
        Err(e) => {
            println!("result: FAILED: {:?}", e);
            ERROR.signal(());
            Timer::after(ERROR_DISPLAY_TIME).await;
        }
    }

    println!("sleeping {} s", SLEEP_SECS);
    // println! returns once the bytes are in the UART's small FIFO; give it a
    // moment to shift them out before the power goes.
    Timer::after(Duration::from_millis(20)).await;

    // Deep sleep: CPUs, RAM and WiFi off, only the RTC domain stays on. The
    // wake is a full reset, so this never returns.
    let mut low_power = LowPower::new(peripherals.LPWR);
    low_power.set_wakeup_deadline(
        esp_hal::time::Instant::now() + esp_hal::time::Duration::from_secs(SLEEP_SECS),
    );
    // The deep() preset powers *all* RTC memory down too, which would wipe
    // WAKE_COUNT. Keep RTC fast memory (where it lives) powered: a few µA.
    let mut sleep_config = RtcSleepConfig::deep();
    sleep_config.set_rtc_fastmem_pd_en(false);
    low_power.sleep_deep(sleep_config)
}

/// Join WiFi, get an address, resolve the host, ping it. Any failure returns
/// early with `?`. The WiFi controller is a local, so on every return path it
/// is dropped, which turns the radio off (like `using`/Dispose in .NET).
async fn probe(spawner: Spawner, wifi: WIFI<'static>) -> Result<(), ProbeError> {
    println!("[1/4] WiFi: joining \"{}\"", SSID);
    let mut controller = WifiController::new(wifi, Default::default()).map_err(ProbeError::Wifi)?;
    let auth = if PASSWORD.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        // Also joins WPA2/WPA3 "transition" networks.
        AuthenticationMethodConfig::Wpa2Personal(PASSWORD.try_into().map_err(ProbeError::Wifi)?)
    };
    let station = StationConfig::default()
        .with_ssid(SSID.try_into().map_err(ProbeError::Wifi)?)
        .with_authentication(auth);
    controller.set_config(&Config::Station(station)).map_err(ProbeError::Wifi)?;

    // Set up the network stack before connecting, so DHCP starts the moment
    // the link comes up. This time we are a normal client: DHCP gives us our
    // address, gateway and DNS server.
    let rng = Rng::new();
    let seed = u64::from(rng.random()) << 32 | u64::from(rng.random());
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(Default::default()),
        STACK_RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());

    // with_timeout gives Err(TimeoutError) if the inner future takes too long
    // (the inner future is dropped, i.e. cancelled). Otherwise it gives the
    // inner future's own Result, hence the two map_errs.
    let info = with_timeout(CONNECT_TIMEOUT, controller.connect_async())
        .await
        .map_err(|_| ProbeError::Timeout("WiFi connect"))?
        .map_err(ProbeError::Connect)?;
    println!("      connected: channel {}, access point {:02x?}", info.channel, info.bssid);

    println!("[2/4] DHCP: waiting for an address");
    with_timeout(DHCP_TIMEOUT, stack.wait_config_up())
        .await
        .map_err(|_| ProbeError::Timeout("DHCP"))?;
    // wait_config_up returned, so there is a config.
    let ip_config = stack.config_v4().unwrap();
    println!(
        "      address {}, gateway {:?}, DNS {:?}",
        ip_config.address, ip_config.gateway, ip_config.dns_servers
    );

    // dns_query returns an IP as-is without asking the server, so this step
    // works for both "192.168.1.1" and "example.com".
    println!("[3/4] DNS: resolving {}", PROBE_HOST);
    let addresses = with_timeout(DNS_TIMEOUT, stack.dns_query(PROBE_HOST, DnsQueryType::A))
        .await
        .map_err(|_| ProbeError::Timeout("DNS"))?
        .map_err(ProbeError::Dns)?;
    let target = *addresses.first().ok_or(ProbeError::NoAddress)?;
    println!("      {} is {}", PROBE_HOST, target);

    println!("[4/4] ping {} ({} times)", target, PING_COUNT);
    let mut rx_meta = [PacketMetadata::EMPTY];
    let mut rx_buf = [0u8; 256];
    let mut tx_meta = [PacketMetadata::EMPTY];
    let mut tx_buf = [0u8; 256];
    let mut pinger = PingManager::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    let mut params = PingParams::new(target);
    params.set_count(PING_COUNT).set_timeout(PING_TIMEOUT);
    let round_trip = pinger.ping(&params).await.map_err(ProbeError::Ping)?;
    println!("      reply, average round trip {} ms", round_trip.as_millis());

    Ok(())
}

/// Fades while all is well; after ERROR is signaled, blinks until sleep.
#[embassy_executor::task]
async fn led_task(mut led: Channel<'static, LowSpeed>) {
    const STEPS: u32 = 100;
    const STEP_TIME: Duration = Duration::from_millis(5); // 1 s per fade cycle
    const BLINK_HALF_PERIOD: Duration = Duration::from_millis(150);

    let max = u32::from(led.max_duty_cycle());

    // `'fade:` labels the outer loop, so `break 'fade` can leave both loops.
    'fade: loop {
        for step in (0..STEPS).chain((1..=STEPS).rev()) {
            // signaled() only peeks; it doesn't consume the signal.
            if ERROR.signaled() {
                break 'fade;
            }
            let duty = max * step * step / (STEPS * STEPS);
            led.set_duty_cycle(duty as u16).unwrap();
            Timer::after(STEP_TIME).await;
        }
    }

    loop {
        led.set_duty_cycle(max as u16).unwrap();
        Timer::after(BLINK_HALF_PERIOD).await;
        led.set_duty_cycle(0).unwrap();
        Timer::after(BLINK_HALF_PERIOD).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}
