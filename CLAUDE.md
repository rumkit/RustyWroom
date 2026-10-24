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

## Environment (Windows host)
- `espup` installed and initialized (provides the `esp` Rust toolchain + Xtensa GCC linker).
- esp-hal 1.2 requires Rust >= 1.95: check with `rustc +esp --version`, update with `espup update`.
- `espflash` installed (used as the cargo runner for flashing + serial monitor).

## Project status
- Scaffolded by hand following the official `esp-generate` 1.4 template.
- Builds cleanly and runs on the board (blinky verified).

## Approach and stack
- Bare-metal `no_std` / `no_main` (chosen over std/ESP-IDF to avoid the C SDK and Windows long-path issues).
- Crates:
  - `esp-hal` ~1.2.2 (features `esp32`, `unstable`): HAL. `unstable` is needed for LEDC (PWM); it also unlocks e.g. `esp_hal::delay::Delay`. Blocking examples still use a busy-wait on `time::Instant`.
  - `embedded-hal` 1.0: chip-independent traits (e.g. `pwm::SetDutyCycle` for fine-grained duty).
  - `esp-bootloader-esp-idf` 0.6: `esp_app_desc!()` app descriptor required by the bootloader.
  - `esp-println` 0.18: `println!` over UART.
  - `esp-backtrace` 0.20 (`panic-handler`, `println`): panic handler.
- Edition 2024.

## Layout
- `Cargo.toml`: package `rusty-wroom`; deps and profiles. Every `src/bin/*.rs` is auto-discovered as a binary named after the file. Dev profile uses `opt-level = "s"`; release uses fat LTO.
- `rust-toolchain.toml`: `channel = "esp"`.
- `.cargo/config.toml`: target `xtensa-esp32-none-elf`, `build-std = ["core"]`, `-nostartfiles`, runner `espflash flash --monitor --chip esp32 --port COM4`.
- `.cargo/esp-config.toml`: esp-hal build-time config env vars (sets the minimum chip revision).
- `build.rs`: adds `-Tlinkall.x` linker script.
- `src/bin/blinky.rs` (bin `blinky`): blocking blinky, toggles GPIO2 every 500 ms, prints state.
- `src/bin/pwm_fade.rs` (bin `pwm_fade`): LEDC low-speed timer 5 kHz / 13-bit, channel 0 on GPIO2, squared (gamma) fade in/out over 1 s each.

## Commands
- Build: `cargo build --release`
- Build + flash + serial monitor: `cargo run --release --bin <name>` (`--bin` is required now that there are several binaries)
- Exit monitor: Ctrl+C

## References
- esp-hal examples: https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.2.2/examples
- The Rust on ESP Book: https://docs.esp-rs.org/book/
- Project generator (for comparing with official templates): `esp-generate`

## Roadmap
Done: blocking blinky; blocking PWM fade (`pwm_fade`).

Order changed: PWM was done first (blocking); step 3 below now means adding button control to it.

Each experiment gets its own binary in `src/bin/` (run with `cargo run --release --bin <name>`) so earlier steps stay runnable. Use `esp-generate` output or esp-hal examples to pick crate versions matching esp-hal 1.2.

1. **Embassy blinky** (`embassy_blinky`): same 500 ms blink, async.
   - Add `esp-rtos` (feature `embassy`), `embassy-executor`, `embassy-time`. Likely requires enabling esp-hal `unstable`.
   - Start the time driver from a TIMG timer, `#[esp_rtos::main] async fn main(spawner: Spawner)`, blink in a spawned `#[embassy_executor::task]` with `Timer::after(...).await`.
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
