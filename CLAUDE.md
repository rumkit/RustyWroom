# RustyWroom: handover notes

Learning project: Rust on an ESP32 (WROOM-32 DevKit). A blocking blinky works; see the Roadmap for the next experiments.

## About the developer
- Background is mostly .NET; learning Rust and embedded Rust.
- Prefers small, incremental steps with explanations over large code dumps. Explain *why* (ownership of peripherals, no_std constraints, etc.) when introducing something new.
- Ask before installing any tools or toolchains.

## Hardware
- Board: ESP32-WROOM-32 DevKit (chip: `esp32`, Xtensa LX6, dual core).
- Chip is silicon **revision v1.0** (early). esp-hal requires v3.0 by default, so `.cargo/esp-config.toml` sets `ESP_HAL_CONFIG_MIN_CHIP_REVISION = "100"`. Without it, espflash refuses to flash.
- Connected on Windows serial port **COM4**.
- Onboard blue LED on **GPIO2**.
- If flashing stalls at "Connecting...", hold the BOOT button.
- **Power:** WiFi start-up draws current spikes that brown out the board's CP210x USB-serial chip when powered through a USB hub (COM4 vanishes ~70 ms after the radio starts; espflash reports "os error 22"). Connect the board directly to the laptop's USB port.
- Opening the serial port can reset the board (DTR/RTS auto-reset circuit). To watch without resetting: `espflash monitor --chip esp32 --port COM4 --non-interactive --no-reset`.

## Environment (Windows host)
- `espup` installed and initialized (provides the `esp` Rust toolchain + Xtensa GCC linker).
- esp-hal 1.2 requires Rust >= 1.95: check with `rustc +esp --version`, update with `espup update`.
- `espflash` installed (used as the cargo runner for flashing + serial monitor).

## Project status
- Scaffolded by hand following the official `esp-generate` 1.4 template.
- All binaries build cleanly and run on the board, including WiFi (`wifi_scan`, `wifi_led`) on the rev v1.0 chip.

## Approach and stack
- Bare-metal `no_std` / `no_main` (chosen over std/ESP-IDF to avoid the C SDK and Windows long-path issues).
- Crates:
  - `esp-hal` ~1.2.2 (features `esp32`, `unstable`): HAL. `unstable` is needed for LEDC (PWM); it also unlocks e.g. `esp_hal::delay::Delay`. Blocking examples still use a busy-wait on `time::Instant`.
  - `embedded-hal` 1.0: chip-independent traits (e.g. `pwm::SetDutyCycle` for fine-grained duty).
  - `esp-rtos` 0.4 (features `esp32`, `embassy`, `esp-radio`, `esp-alloc`): async runtime; `#[esp_rtos::main]`, started with `esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0)`.
  - `embassy-executor` 0.10 (no `arch-*` feature; esp-rtos provides the executor) and `embassy-time` 0.5 (`Timer::after(...).await`).
  - `static_cell` 2.1: one-time init of statics, used to give spawned tasks `'static` data.
  - `embassy-sync` 0.8 (same version esp-hal uses): inter-task primitives; `Watch` for the current fade speed.
  - `esp-radio` 1.0.0-beta.1 (features `esp32`, `wifi`): WiFi driver. Needs opt-level >= 2, so `Cargo.toml` overrides `opt-level = 3` for the `esp-radio` package in both profiles.
  - `esp-alloc` 0.11: heap for the WiFi driver (`heap_allocator!` in the WiFi binaries only).
  - `embassy-net` 0.9 (features `proto-ipv4`, `medium-ethernet`, `tcp`, `udp`, `dhcpv4`, `dns`, `icmp`): TCP/IP stack (smoltcp). `embassy_net::new` returns a `Stack` (Copy handle for opening sockets) and a `Runner` (needs its own task). With `dns` on, every stack reserves one `StackResources` slot for its internal DNS socket; features are unified across all binaries, so count it in every binary.
  - `portable-atomic` 1.11: its `AtomicU32` is `Persistable`, so it can live in `#[esp_hal::ram(unstable(rtc_fast, persistent))]` (core's atomics can't).
  - `edge-dhcp` 0.8, `edge-captive` 0.8: DHCP server and catch-all captive-portal DNS. They use `edge-nal` 0.7 traits; `edge-nal-embassy` 0.9 (default features off; `proto-ipv4`, `medium-ethernet`, `udp`) adapts embassy-net UDP sockets to them.
  - `embassy-futures` 0.1: `select` (wait for whichever future finishes first).
  - `heapless` 0.9: fixed-capacity `String`/`Vec` without a heap.
  - `esp-bootloader-esp-idf` 0.6: `esp_app_desc!()` app descriptor required by the bootloader.
  - `esp-println` 0.18: `println!` over UART.
  - `esp-backtrace` 0.20 (`panic-handler`, `println`): panic handler.
- Edition 2024.

## Layout
- `Cargo.toml`: package `rusty-wroom`; deps and profiles. Every `src/bin/*.rs` is auto-discovered as a binary named after the file. Dev profile uses `opt-level = "s"`; release uses fat LTO.
- `rust-toolchain.toml`: `channel = "esp"`.
- `.cargo/config.toml`: target `xtensa-esp32-none-elf`, `build-std = ["alloc", "core"]` (alloc needed by esp-radio), `-nostartfiles`, runner `espflash flash --monitor --chip esp32 --port COM4`.
- `.cargo/esp-config.toml`: esp-hal build-time config env vars (sets the minimum chip revision).
- `.cargo/secrets.toml`: git-ignored local secrets as Cargo `[env]` vars (`RUSTYWROOM_SSID`, `RUSTYWROOM_PASSWORD`, `RUSTYWROOM_PROBE_HOST`), read in code with `env!`. Included from `.cargo/config.toml` as an optional include; without it `wifi_probe` fails to compile (and so does a plain `cargo build`), other binaries build with `--bin`. Template: `.cargo/secrets.example.toml` (committed). Never commit or print the real values.
- `build.rs`: adds `-Tlinkall.x` linker script.
- `src/bin/blinky.rs` (bin `blinky`): blocking blinky, toggles GPIO2 every 500 ms, prints state.
- `src/bin/pwm_fade.rs` (bin `pwm_fade`): LEDC low-speed timer 5 kHz / 13-bit, channel 0 on GPIO2, squared (gamma) fade in/out over 1 s each.
- `src/bin/embassy_fade.rs` (bin `embassy_fade`): same fade, but async under esp-rtos; `Timer::after(...).await` instead of busy-wait. The fade runs in a spawned `#[embassy_executor::task]`; the LEDC timer lives in a `StaticCell` so the channel is `'static`. `main` prints a heartbeat every 5 s.
- `src/bin/button_fade.rs` (bin `button_fade`): BOOT button (GPIO0) cycles fade speed Slow/Medium/Fast (4 s / 2 s / 0.5 s cycle). Three tasks: `button_task` (edge wait + 50 ms debounce, waits for release) sends to a `static SPEED: Watch<_, Speed, 2>`; `fade` polls `try_changed()` each step (only step time changes, so brightness does not jump); `main` awaits `changed()` and prints. Note: `Watch::new_with` initial value is not reported by `changed()`; use `get()`.
- `src/bin/wifi_scan.rs` (bin `wifi_scan`): 64 KB reclaimed + 36 KB heap, esp-rtos, `WifiController::new` (starts station mode), `scan_async` every 10 s, prints networks sorted by RSSI. Flash image ~490 KB.
- `src/bin/wifi_led.rs` (bin `wifi_led`): open access point "RustyWroom", static IP 192.168.4.1/24. Tasks: `net_task` (embassy-net runner), `dhcp_task` (edge-dhcp, leases .50-.200, advertises us as gateway + DNS), `dns_task` (edge-captive: every name resolves to us), `http_task` (`pool_size = 3`, hand-written HTTP/1.1, one request per connection), `led_task`. `static LED: Watch<_, LedState, 1>` (brightness 0-100 %, blink); HTTP tasks update it with `send_if_modified`, `led_task` uses `select(changed(), Timer)` to blink. Routes: `/` (page with slider + checkbox, JS keeps one request in flight), `/state` (JSON), `/set?b=..&blink=..` (GET on purpose: no body parsing), anything else 302 to the page, which makes phones show the captive-portal "sign in" prompt (on a Samsung S24 it can take a while to appear). `main` prints stations joining/leaving.
- `src/bin/wifi_probe.rs` (bin `wifi_probe`): monitoring probe, one straight pass per wake: join home WiFi (station, credentials from secrets via `env!`, compile-time `assert!` that they're non-empty) → DHCP (`Config::dhcpv4`) → DNS (`dns_query`, passes IPs through) → ping (`icmp::ping::PingManager`, 3 pings). Each step has a `with_timeout` and logs `[n/4]`; errors are a `ProbeError` enum propagated with `?`. LED: `led_task` fades while working, blinks after `static ERROR: Signal` is raised (blinks 10 s, then sleeps anyway). Wake counter `WAKE_COUNT: AtomicU32` in persistent RTC fast RAM; esp-hal zeroes persistent memory only on a `ChipPowerOn` reset, and other resets (seen after flashing) leave garbage, so `main` resets it unless `reset_reason` is `CoreDeepSleep`. Gotcha: on the ESP32, `RtcSleepConfig::deep()` powers RTC fast and slow memory down (`rtc_fastmem_pd_en`/`rtc_slowmem_pd_en`), wiping persistent statics; `wifi_probe` calls `set_rtc_fastmem_pd_en(false)` to keep them. Then `LowPower::set_wakeup_deadline` + `sleep_deep(RtcSleepConfig::deep())` for 60 s; the wake is a full reset.

## Commands
- Build: `cargo build --release`
- Build + flash + serial monitor: `cargo run --release --bin <name>` (`--bin` is required now that there are several binaries)
- Exit monitor: Ctrl+C

## References
- esp-hal examples: https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.2.2/examples
- The Rust on ESP Book: https://docs.esp-rs.org/book/
- Project generator (for comparing with official templates): `esp-generate`

## Roadmap
Done: blocking blinky; blocking PWM fade (`pwm_fade`); async PWM fade (`embassy_fade`, step 1); button cycles fade speed (`button_fade`, replaces step 2); WiFi scan (`wifi_scan`, step 4 stepping stone); WiFi LED web remote with captive portal (`wifi_led`, step 4).

Order changed: PWM was done first (blocking); step 3 below now means adding button control to it.

Each experiment gets its own binary in `src/bin/` (run with `cargo run --release --bin <name>`) so earlier steps stay runnable. Use `esp-generate` output or esp-hal examples to pick crate versions matching esp-hal 1.2.

1. ~~**Embassy blinky**~~ done as `embassy_fade` (the PWM fade, async).
   - Fade runs in a spawned task (LEDC timer in a `StaticCell`).
   - Concepts: executor/tasks vs .NET Task/async state machines, why tasks need `'static` args, no heap.
2. **Button changes blink frequency** (`button_blink`):
   - BOOT button = GPIO0, active low (pressed = Low), `Input` with pull-up.
   - Button task: `wait_for_falling_edge().await` + ~50 ms debounce, cycles 1000/500/200/100 ms.
   - Share the period with the blink task via `embassy_sync::Signal` (or an atomic) and `select` between timer and signal so changes apply immediately.
   - Concepts: sharing state between tasks without `Arc<Mutex>`, statics, debouncing.
3. **PWM brightness** (`pwm_dim`):
   - LEDC peripheral on GPIO2, e.g. 5 kHz, low-speed channel.
   - Short press steps brightness 0 → 10 → 30 → 60 → 100 %; stretch: long press = smooth breathing fade.
   - Concepts: timers vs channels, duty cycle, perceived brightness is non-linear (gamma).
4. **Wireless (fun example)** (`wifi_led`): ESP32 hosts its own WiFi access point ("RustyWroom"); a phone joins it and opens a tiny web page with a brightness slider + "blink" toggle that drives the LED from step 3. No router or credentials needed.
   - Stepping stone first: WiFi scan that prints nearby networks (proves the radio works on this rev v1.0 chip).
   - Stack: `esp-radio` (wifi), `esp-alloc` (heap), `embassy-net` (TCP/IP + DHCP server), minimal hand-written HTTP.
   - Alternative if WiFi is troublesome: BLE via `esp-radio` + `trouble-host`, a GATT characteristic for brightness written from the nRF Connect phone app, button presses sent as notifications.
