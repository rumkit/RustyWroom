#![no_std]
#![no_main]

use core::{
    fmt::Write as _,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};

use edge_dhcp::{
    io::DEFAULT_SERVER_PORT,
    server::{Server, ServerOptions},
};
use edge_nal::UdpBind;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4, tcp::TcpSocket};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    watch::{Receiver, Watch},
};
use embassy_time::{Duration, Timer};
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
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{Config, Interface, WifiController, ap};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// Name of the WiFi network the ESP32 creates (open, no password).
const SSID: &str = "RustyWroom";
/// Our own address on that network. 192.168.4.1 is the ESP-IDF convention.
const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
/// Where unknown URLs get redirected to (must match IP).
const PORTAL_URL: &str = "http://192.168.4.1/";

/// How many HTTP connections can be served at once. Browsers often open a
/// second "spare" connection, which a single listener would refuse, and with
/// the captive DNS the phone's own background requests land here too.
const HTTP_TASKS: usize = 3;
/// Every socket needs a slot in the stack: DHCP + DNS + the HTTP ones, plus
/// one embassy-net reserves for its own DNS client (its "dns" feature is on
/// for wifi_probe, and features are shared by all binaries).
const SOCKETS: usize = 2 + HTTP_TASKS + 1;

/// Time the LED stays on (and then off) while blinking.
const BLINK_HALF_PERIOD: Duration = Duration::from_millis(250);

/// What the web page controls.
#[derive(Clone, Copy, PartialEq)]
struct LedState {
    /// Perceived brightness, 0..=100 %.
    brightness: u8,
    blink: bool,
}

/// The LED state. HTTP tasks change it, the LED task (its only receiver)
/// reacts to every change.
static LED: Watch<CriticalSectionRawMutex, LedState, 1> = Watch::new_with(LedState {
    brightness: 50,
    blink: false,
});

static STACK_RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
static LEDC_TIMER: StaticCell<timer::Timer<'static, LowSpeed>> = StaticCell::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // LED on GPIO2 via LEDC, same setup as the fade examples.
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
    spawner.spawn(led_task(led, LED.receiver().unwrap()).unwrap());

    // Same controller as in wifi_scan, but switched to access point mode.
    // `wifi` must stay alive: dropping the controller turns WiFi off.
    let mut wifi = WifiController::new(peripherals.WIFI, Default::default()).unwrap();
    wifi.set_config(&Config::AccessPoint(
        ap::AccessPointConfig::default().with_ssid(SSID.try_into().unwrap()),
    ))
    .unwrap();

    // The controller manages the radio; the Interface is the "network card":
    // it sends and receives raw Ethernet-like frames. embassy-net sits on
    // top of it and turns frames into IP packets and sockets.
    let device = Interface::access_point();

    // We are the network, so nobody hands *us* an address: it's static.
    // /24 means the network is 192.168.4.0 - 192.168.4.255.
    let net_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(IP, 24),
        gateway: Some(IP),
        dns_servers: Default::default(),
    });

    // The TCP stack wants a random seed (e.g. for initial sequence numbers).
    let rng = Rng::new();
    let seed = u64::from(rng.random()) << 32 | u64::from(rng.random());

    // embassy-net splits into two halves:
    // - Stack: a cheap, Copy handle that tasks use to open sockets.
    // - Runner: the engine that actually moves packets; it needs its own task.
    let (stack, runner) = embassy_net::new(
        device,
        net_config,
        STACK_RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(dhcp_task(stack).unwrap());
    spawner.spawn(dns_task(stack).unwrap());
    // pool_size on the task lets us spawn the same task more than once.
    for _ in 0..HTTP_TASKS {
        spawner.spawn(http_task(stack).unwrap());
    }

    println!("Join WiFi \"{}\" and open http://{}/", SSID, IP);

    // main only reports phones joining and leaving.
    loop {
        match wifi.wait_for_access_point_connected_event_async().await {
            Ok(ap::EventInfo::Connected(info)) => println!("joined: {:02x?}", info.mac),
            Ok(ap::EventInfo::Disconnected(info)) => println!("left:   {:02x?}", info.mac),
            Err(e) => println!("wifi event error: {:?}", e),
        }
    }
}

/// Shows the current LedState on the LED, blinking if asked to.
#[embassy_executor::task]
async fn led_task(
    mut led: Channel<'static, LowSpeed>,
    mut rx: Receiver<'static, CriticalSectionRawMutex, LedState, 1>,
) {
    let max = u32::from(led.max_duty_cycle());
    let mut state = rx.get().await;
    // While blinking: are we in the "on" half?
    let mut on = true;

    loop {
        let percent = if on { u32::from(state.brightness) } else { 0 };
        // Squared, like the fades: our eyes see 25 % duty as about half bright.
        let duty = max * percent * percent / (100 * 100);
        led.set_duty_cycle(duty as u16).unwrap();

        if state.blink {
            // Wait for whichever comes first: a new state from the page, or
            // the time to flip on/off. The loser is simply dropped.
            match select(rx.changed(), Timer::after(BLINK_HALF_PERIOD)).await {
                Either::First(new_state) => {
                    state = new_state;
                    // Show the change right away (and don't get stuck in
                    // the "off" half if blinking was just switched off).
                    on = true;
                }
                Either::Second(()) => on = !on,
            }
        } else {
            // Nothing to animate: sleep until the page changes something.
            state = rx.changed().await;
        }
    }
}

/// Drives the network stack: polls the WiFi interface, runs timers
/// (retransmits, ARP), and wakes sockets that got data. Never returns.
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// Hands out addresses 192.168.4.50 - .200 to devices that join.
#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    // Buffers for 1 UDP socket. They live inside this task's (static) state,
    // not on the heap.
    let buffers = UdpBuffers::<1, 1024, 1024, 2>::new();
    let udp = Udp::new(stack, &buffers);
    // DHCP clients don't have an address yet, so they broadcast to port 67;
    // we listen on all addresses.
    let mut socket = udp
        .bind(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            DEFAULT_SERVER_PORT,
        )))
        .await
        .unwrap();

    // Up to 4 leases at a time; the clock (embassy-time) expires old ones.
    let mut server = Server::<fn() -> u64, 4>::new_with_et(IP);
    // We tell clients that we are their gateway (and subnet 255.255.255.0)
    // and their DNS server, so every name lookup comes to dns_task.
    let mut gateway = [IP];
    let dns = [IP];
    let mut options = ServerOptions::new(IP, Some(&mut gateway));
    options.dns = &dns;
    let mut buf = [0u8; 1500];

    loop {
        // Only returns on a socket error; log it and start again.
        if let Err(e) = edge_dhcp::io::server::run(&mut server, &options, &mut socket, &mut buf).await
        {
            println!("DHCP error: {:?}", e);
            Timer::after(Duration::from_secs(1)).await;
        }
    }
}

/// Captive portal DNS: answers *every* name lookup with our own IP. Right
/// after joining, the phone checks e.g. connectivitycheck.gstatic.com; that
/// now reaches our HTTP server, gets an unexpected answer, and the phone
/// opens its "sign in to network" popup on our page.
#[embassy_executor::task]
async fn dns_task(stack: Stack<'static>) {
    // DNS messages over UDP are at most 512 bytes (without extensions).
    let buffers = UdpBuffers::<1, 512, 512, 2>::new();
    let udp = Udp::new(stack, &buffers);
    let mut tx_buf = [0u8; 512];
    let mut rx_buf = [0u8; 512];
    let local = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 53));
    // How long the phone may cache our fake answers.
    let ttl = core::time::Duration::from_secs(60);

    loop {
        // Like the DHCP server: only returns on an error.
        if let Err(e) = edge_captive::io::run(&udp, local, &mut tx_buf, &mut rx_buf, IP, ttl).await {
            println!("DNS error: {:?}", e);
            Timer::after(Duration::from_secs(1)).await;
        }
    }
}

/// The web page. A raw string (r#"..."#) needs no escaping of quotes.
/// The script asks for the current state on load, then sends the full state
/// on every change. Only one request is in flight at a time: dragging the
/// slider fires many events, and the latest value is all that matters.
const PAGE: &str = r#"<!doctype html>
<html><head>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>RustyWroom</title>
<style>
body{font-family:sans-serif;text-align:center;max-width:24em;margin:2em auto;padding:0 1em}
input[type=range]{width:100%}
label{font-size:1.3em}
</style>
</head><body>
<h1>RustyWroom</h1>
<p><label>Brightness: <span id=v>?</span>%<br>
<input id=b type=range min=0 max=100></label></p>
<p><label><input id=k type=checkbox> Blink</label></p>
<script>
const b=document.getElementById('b'),k=document.getElementById('k'),v=document.getElementById('v');
function show(s){b.value=s.b;v.textContent=s.b;k.checked=s.blink}
let busy=false,pending=false;
function send(){
  if(busy){pending=true;return}
  busy=true;
  fetch('/set?b='+b.value+'&blink='+(k.checked?1:0))
    .then(r=>r.json()).then(s=>{if(!pending)show(s)}).catch(()=>{})
    .finally(()=>{busy=false;if(pending){pending=false;send()}});
}
b.oninput=()=>{v.textContent=b.value;send()};
k.onchange=send;
fetch('/state').then(r=>r.json()).then(show);
</script>
</body></html>"#;

/// A minimal HTTP server: accept a connection, read the request, answer,
/// close. One request per connection keeps it simple.
#[embassy_executor::task(pool_size = HTTP_TASKS)]
async fn http_task(stack: Stack<'static>) {
    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];
    let mut request = [0u8; 1024];

    loop {
        // A socket borrows the buffers, so it's created fresh for each
        // connection and dropped at the end of the loop body.
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        // Don't let a silent client hold this task forever.
        socket.set_timeout(Some(Duration::from_secs(10)));

        if let Err(e) = socket.accept(80).await {
            println!("accept error: {:?}", e);
            continue;
        }

        // Read until the blank line that ends the HTTP headers.
        let mut len = 0;
        while len < request.len() {
            match socket.read(&mut request[len..]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => len += n,
            }
            if request[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        // First line is e.g. "GET /set?b=40&blink=0 HTTP/1.1".
        let first_line = request[..len].split(|&b| b == b'\r').next().unwrap_or(&[]);
        let first_line = core::str::from_utf8(first_line).unwrap_or("");
        println!("{}", first_line);

        if handle(&mut socket, first_line).await.is_ok() {
            // Wait until the phone has acknowledged everything we sent.
            let _ = socket.flush().await;
        }
        socket.close();
        // Give the "close" (FIN) a moment to go out before the socket is
        // dropped, which would otherwise reset the connection.
        Timer::after(Duration::from_millis(50)).await;
        socket.abort();
    }
}

/// Routes one request. (Changing state with GET isn't proper REST, but it
/// means there's no request body to parse.)
async fn handle(socket: &mut TcpSocket<'_>, first_line: &str) -> Result<(), embassy_net::tcp::Error> {
    let mut parts = first_line.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    // "/set?b=40&blink=0" -> ("/set", "b=40&blink=0")
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    match (method, path) {
        ("GET", "/") => respond(socket, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await,
        ("GET", "/state") => respond_state(socket).await,
        ("GET", "/set") => {
            apply_query(query);
            respond_state(socket).await
        }
        // Anything else (the phone's connectivity check, any website the
        // user types): send the browser to our page. The phone notices it
        // didn't get the answer it expected and shows the portal popup.
        _ => redirect(socket).await,
    }
}

async fn redirect(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    for part in [
        b"HTTP/1.1 302 Found\r\nLocation: ".as_slice(),
        PORTAL_URL.as_bytes(),
        b"\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    ] {
        write_all(socket, part).await?;
    }
    Ok(())
}

/// Updates LED from a query like "b=40&blink=1". Missing or invalid values
/// leave that part of the state unchanged.
fn apply_query(query: &str) {
    let mut brightness = None;
    let mut blink = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("b", value)) => brightness = value.parse::<u8>().ok().map(|b| b.min(100)),
            Some(("blink", value)) => blink = Some(value == "1"),
            _ => {}
        }
    }

    // Read-modify-write in one locked step, so two HTTP tasks can't
    // overwrite each other's change. Returning false (nothing changed)
    // doesn't wake the LED task.
    LED.sender().send_if_modified(|state| {
        let Some(state) = state else { return false };
        let old = *state;
        if let Some(b) = brightness {
            state.brightness = b;
        }
        if let Some(k) = blink {
            state.blink = k;
        }
        *state != old
    });
}

/// Replies with the current state as JSON, e.g. {"b":40,"blink":false}.
async fn respond_state(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    // new_with gave LED a value, so there always is one.
    let state = LED.try_get().unwrap();
    // A String with a fixed capacity of 32 bytes, no heap. write! fails
    // (instead of growing) if it doesn't fit; this always fits.
    let mut json = heapless::String::<32>::new();
    write!(json, r#"{{"b":{},"blink":{}}}"#, state.brightness, state.blink).unwrap();
    respond(socket, "200 OK", "application/json", json.as_bytes()).await
}

async fn respond(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    for part in [
        b"HTTP/1.1 ".as_slice(),
        status.as_bytes(),
        b"\r\nContent-Type: ",
        content_type.as_bytes(),
        // no-store: always ask us, so the page never shows a stale state.
        b"\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body,
    ] {
        write_all(socket, part).await?;
    }
    Ok(())
}

/// `write` may accept only part of the data (when the send buffer is full),
/// so keep going until all of it is queued.
async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let n = socket.write(data).await?;
        data = &data[n..];
    }
    Ok(())
}
