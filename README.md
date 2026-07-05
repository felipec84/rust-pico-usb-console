# rust-pico-usb-console

Skeleton firmware for the Raspberry Pi Pico (RP2040) in Rust, using
[embassy](https://embassy.dev/). Use this as the starting point for a new
RP2040 project: a working USB-CDC console, crash reporting, and
`picotool`-driven reprogramming are already wired up — add your application
logic in one place and go.

## What's included

- **USB-CDC interactive console** — line-buffered input with backspace
  support, dispatches full command lines to `app_task` over an
  `embassy_sync::channel::Channel` (and gets responses back over a second
  channel) — see [Built-in console commands](#built-in-console-commands).
- **Crash reporting via `panic-persist`** — on panic, the message is saved to
  a small RAM region (`PANDUMP`, see `memory.x`) and the chip soft-resets.
  The message is replayed over USB-CDC on the next boot, so you can see why
  it crashed without a debug probe attached.
- **Two independent BOOTSEL reset paths:**
  - **1200-baud trick**: opening the serial port at 1200 baud reboots the
    device into BOOTSEL, same as stock Pico boards. Used by `flash.sh`.
  - **`picotool -f` / `--force`**: a vendor USB interface (class `0xFF`) lets
    `picotool` reboot the device into BOOTSEL directly, without needing the
    board to already be stuck in a serial-openable state. See
    [picotool -f](#picotool--f-how-it-works) below — this one has sharp edges.
- **No physical BOOTSEL button needed** for normal iteration, once the first
  flash is done.

## Prerequisites

- Rust with the `thumbv6m-none-eabi` target (`rustup target add thumbv6m-none-eabi`)
- [`elf2uf2-rs`](https://github.com/JoNil/elf2uf2-rs) (`cargo install elf2uf2-rs`)
- [`picotool`](https://github.com/raspberrypi/picotool), built from source or
  packaged — needed for `-f` reprogramming and `info`/`reboot`. See
  `embassy-rp2040-usb-guia.md` §3 for build + udev rules instructions.

## Build & flash

```sh
./flash.sh
```

This builds in release, converts to UF2, triggers a 1200-baud reset if the
device is already enumerated as a serial port, waits for BOOTSEL, then loads
via `picotool`. First flash on a blank board still needs the physical BOOTSEL
button (hold it while plugging in USB).

Manual build only: `cargo build --release`.

Serial monitor: `python3 -m serial.tools.miniterm /dev/ttyACM0 115200`.

## Using this as a template for a new project

This repo is maintained on two branches:

- **`master` — the cargo-generate template.** It carries liquid
  placeholders (`project-name`, `crate_name`, etc. in double curly braces)
  plus a `cargo-generate.toml`, so it does **not** build directly; it
  exists to be consumed by `cargo generate`.
- **`develop` — the buildable reference.** Real names, flashable with
  `./flash.sh`. All generic improvements happen (and get hardware-verified)
  here. To update the template afterwards:
  `git checkout master && git merge develop && git checkout develop`.
  **Never commit directly to `master`** — keeping the placeholder lines
  untouched on `develop` is what keeps these merges conflict-free.

To start a new project from the template:

```bash
cargo install cargo-generate   # once

# from GitHub:
cargo generate --git git@github.com:felipec84/rust-pico-usb-console.git --name my-project

# or from this local checkout (--branch master matters: a local clone would
# otherwise use whatever branch happens to be checked out here):
cargo generate --git ~/Desarrollos/pico_proyects/rust-pico-usb-console --branch master --name my-project
```

cargo-generate prompts for the USB product/manufacturer strings (defaults
are the stock Pico values) and substitutes the project name into
`Cargo.toml` and `flash.sh`. Then, in the generated project:

1. If you ship this commercially, set your own USB VID/PID in the
   `CUSTOMIZE PER PROJECT` block in `main()`. The defaults
   (`0x2E8A`/`0x000A`) are the stock Raspberry Pi values for a USB-CDC Pico
   and work fine for development.
2. Leave `config.serial_number` alone — it's derived from the flash's unique
   ID at boot (see below), not something to hardcode.
3. Write your actual logic in `app_task()` (`src/main.rs`). It already
   receives full command lines from the console via `RX_CHANNEL` and answers
   through `TX_CHANNEL`; add your own commands to its `match`, and add
   GPIO/I2C/SPI/ADC peripherals to its signature as needed (own them in
   `main()` and pass them in, same pattern as the ADC/watchdog below).
4. Adjust `memory.x` only if you change flash size or need a bigger `PANDUMP`
   region — the rest (boot2, `.bi_entries`, panic dump symbols) is
   boilerplate every RP2040 project needs.

### Parent Cargo Config Merging (Workspace Compatibility)

Cargo searches for and merges target-specific configurations (like `[target.thumbv6m-none-eabi].rustflags`) recursively from parent directories. If you nest a project generated from this template inside another Cargo repository that also defines `rustflags` for the same target, Cargo concatenates the flags, causing linker scripts to run twice. This results in a linker error (`region 'BOOT2' already defined`).

To prevent this, this template:
1. Comments out `rustflags` in `.cargo/config.toml`.
2. Employs [build.rs](build.rs) to walk up parent folders and check for ancestor configurations. If no parent configuration defines `rustflags` (standalone build), it dynamically outputs the required link arguments (`cargo:rustc-link-arg=...`). Otherwise, it lets the parent configuration handle it.

## Built-in console commands

Connect with a serial monitor (`python3 -m serial.tools.miniterm /dev/ttyACM0
115200`) and type a command followed by Enter:

| Command | What it does |
|---|---|
| `help` | Lists the available commands |
| `info` | Program name/version, flash unique ID (hex), and last reset reason |
| `temp` | Reads the RP2040's internal temperature sensor via the ADC (async, RP2040 datasheet §4.9.5 calibration formula) |
| `uptime` | Milliseconds since boot |
| `bootsel` | Reboots into BOOTSEL mode (same `rom_data::reset_to_usb_boot` call used by the 1200-baud trick) |

These exist to demonstrate reading real chip info and dispatching commands
over embassy channels — replace them with your own commands in `app_task`'s
`match` when using this as a template. Two honest limitations worth knowing:

- No ANSI escape handling (arrow keys etc. type garbage into the line, use
  plain typing + backspace).
- The reset-reason reported by `info` can't distinguish a power-on reset
  from our own software resets (panic-persist, `SCB::sys_reset()`) — the
  RP2040's watchdog register only records watchdog-triggered resets.

A third limitation used to live here: the first command typed after boot
occasionally came back "unknown command" with garbage glued as an invisible
prefix. Two stacked root causes, both fixed:

1. `CdcAcmClass::wait_connection()` only waits for USB *enumeration*, not
   for a program opening the port — and `read_packet` does **not** error
   when the host closes the port. So the firmware never noticed port
   opens/closes: if a host process (e.g. ModemManager probing the new
   ttyACM) opened the port before you did, its session and yours looked
   like one continuous stream, and bytes received during the probe stayed
   in the line buffer, prefixing your first command. The firmware now
   tracks **DTR** (raised on open, dropped on close) as the real session
   boundary, and clears the line buffer and both command channels at each
   new session.
2. When a port is opened there is a brief window, before the terminal
   program switches the tty to raw mode, where the kernel line discipline
   still has `ECHO` enabled — anything the Pico sends in that window (the
   banner) comes back as fake input. After the banner, the firmware drains
   and discards input until the line is quiet (max ~300 ms).

As a bonus, the short banner now prints on *every* port open, so an
ephemeral host probe can no longer steal the boot's only banner.

## `picotool -f` — how it works (and why it's finicky)

`picotool -f`/`--force` is supposed to let you run any `picotool` command
against a device that's currently *running* your firmware (not sitting in
BOOTSEL) by asking it to reboot into BOOTSEL first. Getting this working
correctly required two non-obvious fixes, both already applied here:

1. **The reset request is CLASS type, not VENDOR.** picotool sends
   `bmRequestType = CLASS | INTERFACE` to the vendor-class reset interface —
   despite the interface itself being vendor-class, the *request* isn't. The
   handler in `main.rs` (`PicotoolResetHandler`) matches on recipient +
   interface index only, not request type, mirroring pico-sdk's own
   reference driver.

2. **The USB serial number must be the flash's unique ID, in hex.** For
   RP2040, `picotool` does not trust the USB serial string reported by a
   device sitting in BOOTSEL mode. Instead, after asking a running device to
   reboot, it reads the *actual* flash unique ID over the PICOBOOT protocol
   and compares it against the serial number that device reported *while
   running* (parsed as hex). If those don't match — e.g. if the running
   firmware reports an arbitrary string like `"MY-DEVICE-01"` — picotool
   never recognizes the rebooted device as the one it was tracking, and gives
   up after ~6 seconds of retries, even though the reboot itself succeeded
   and the device is sitting right there in BOOTSEL. This is why `main.rs`
   computes the serial from `embassy_rp::flash::Flash::blocking_unique_id()`
   at boot instead of using a fixed string — the same convention pico-sdk
   boards follow via `pico_get_unique_board_id()`.

If you ever see `picotool -f` claim "no accessible devices ... found" but a
*second* manual call immediately succeeds, check the serial number first —
that mismatch is almost always the cause.

## Files

| File | Purpose |
|---|---|
| `src/main.rs` | Firmware: USB setup, console task, picotool reset handler, app task |
| `memory.x` | Linker script — flash/RAM layout, `PANDUMP` region for panic-persist |
| `flash.sh` | Build + auto-reset + `picotool load` in one step |
| `build.rs` | Reruns build on `memory.x` changes and dynamically detects parent configurations to configure target linker flags without duplicating them |
| `.cargo/config.toml` | Target, runner, and commented target flags (handled dynamically by `build.rs`) |
| `embassy-rp2040-usb-guia.md` | Deep-dive walkthrough (Spanish) of how the USB-CDC + panic-persist setup was built, including picotool install/udev rules |
