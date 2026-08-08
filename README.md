# rust-pico-usb-console

Skeleton firmware for the Raspberry Pi Pico (RP2040) in Rust, using
[embassy](https://embassy.dev/). Use this as the starting point for a new
RP2040 project: a working USB-CDC console, crash reporting, and
`picotool`-driven reprogramming are already wired up — add your application
logic in one place and go.

## What's included

- **USB-CDC interactive console** — line-buffered input with backspace
  support, dispatches full command lines to `app_task` (`src/console.rs`)
  over an `embassy_sync::channel::Channel` (and gets responses back over a
  second channel) — see [Built-in console commands](#built-in-console-commands).
- **Per-subsystem peripheral ownership** — `embassy_rp::init()`'s single
  `Peripherals` struct is split by `assign_resources!` (`src/resources.rs`)
  into named groups (`UsbConsoleResources`, `SensorsResources`, ...), one per
  module. `main.rs` never touches `p.PIN_XX` directly; each module owns
  exactly the pins it needs, declared in one place, so a pin used twice is a
  compile error instead of a runtime conflict.
- **"Module owns the peripheral, consumers read cached state" pattern** — see
  `src/sensors.rs`: a dedicated task samples the ADC on its own schedule and
  publishes into a `static` behind a mutex; `console.rs` only ever calls the
  non-blocking `sensors::get_status()`, never touches the ADC. Copy this
  pattern for any sensor you add — it's what lets a slow peripheral (I2C,
  1-Wire) live behind a fast console without blocking command handling.
- **Watchdog + bootloop guard** (`src/watchdog.rs`) — a task feeds the
  hardware watchdog periodically so a hung executor self-recovers by reset.
  A reboot counter stored in the `.uninit` RAM section (survives soft-resets,
  cleared by real power-on) escalates to a clean `panic!()` after 3
  consecutive watchdog-triggered resets, instead of reset-looping forever —
  important if a reset has a physical side effect (relay chatter, etc.) on
  your hardware.
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
- [`uv`](https://docs.astral.sh/uv/) — only needed to run
  `test/console_test.py` (see [Hardware test](#hardware-test)); it resolves
  the script's `pyserial` dependency on its own, no venv/pip setup required.

## Build & flash

```sh
./flash.sh
```

This builds in release, converts to UF2, triggers a 1200-baud reset if the
device is already enumerated as a serial port, waits for BOOTSEL, then loads
via `picotool`. First flash on a blank board still needs the physical BOOTSEL
button (hold it while plugging in USB).

The script discovers the port under `/dev/serial/by-id/` (matching any CDC
interface, `-if01`) instead of assuming `/dev/ttyACM0` — if more than one
serial device is connected, it aborts rather than risk resetting the wrong
one; set `PICO_PORT=/dev/ttyACMx` to pick one explicitly. Pass `--force`/`-f`
to skip port discovery and the 1200-baud reset entirely and just wait for a
board that's already sitting in BOOTSEL (manual button press).

Manual build only: `cargo build --release`.

Serial monitor: `python3 -m serial.tools.miniterm /dev/ttyACM0 115200`.

**Known flaky spot (under investigation):** occasionally, after `picotool
load -x` reports success and reboots the board into the app, the device
doesn't re-enumerate at all — not as the app, not back in BOOTSEL, nothing
in `lsusb`/`dmesg` — for a long time (60s+). It has always recovered with a
physical unplug/replug (sometimes into a different USB port), and once
recovered, the same firmware runs correctly and repeatably. Not yet
reproduced with a clear trigger; current suspicion is a `picotool -x` /
host USB re-enumeration quirk rather than a firmware defect — the console
itself has passed the full `test/console_test.py` suite, including a real
`bootsel` reboot cycle, multiple times back to back once the board is
actually enumerated. If you hit it, unplug/replug (a different port seems
to help) rather than assume the firmware is broken.

## Hardware test

`test/console_test.py` is a [uv](https://docs.astral.sh/uv/) script (no
manual venv/pip setup — `uv run` resolves its one dependency, `pyserial`,
on the fly) that drives the console over USB and checks the replies look
right, instead of eyeballing a terminal by hand:

```sh
uv run test/console_test.py
```

It finds the board automatically by USB VID:PID (the stock `2E8A:000A`
values, or your own if you changed them — override with `PICO_VID`/
`PICO_PID`, or bypass discovery entirely with `PICO_PORT`/`--port`, same
convention as `flash.sh`). It exercises `help`/`info`/`temp`/`uptime`, an
unknown-command error path, and the DTR session-boundary behavior described
below (reopening the port doesn't glue stale input onto the next command).

For a **fast, side-effect-free presence check** — useful for a coding agent
deciding whether it can claim a change is hardware-verified, or a script
gating on "is a Pico plugged in right now" — use:

```sh
uv run test/console_test.py --check   # exit 0 + prints the port if found, 1 if not
```

This does discovery only; it never opens the serial port. See `CLAUDE.md`
for how this is meant to fit into a Claude Code session working on this
repo.

`--include-bootsel` additionally tests the `bootsel` command, which reboots
the board into BOOTSEL and leaves it there — you'll need to run `./flash.sh`
again afterward to restore the firmware, so it's opt-in, not part of the
default run.

If you add your own console commands when using this as a template, extend
`test/console_test.py` with matching checks in the same change.

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
3. Add your peripherals to a group in `src/resources.rs` (`assign_resources!`
   macro — see the comments there), one group per module you're going to
   write.
4. Write your module (`src/your_module.rs`), following the pattern in
   `src/sensors.rs`: a `#[embassy_executor::task]` that owns the
   `YourModuleResources` struct and samples/drives the hardware on its own
   schedule, plus a non-blocking `pub fn get_status()` that other tasks call
   to read the last cached value. If a command instead needs to *trigger* an
   action in that module (not just read its state), use an
   `embassy_sync::signal::Signal` request/response pair — see the comment
   above `app_task` in `src/console.rs` for the shape of that pattern.
5. Write your actual console commands in `app_task()`'s `match`
   (`src/console.rs`). It already receives full command lines from
   `RX_CHANNEL` and answers through `TX_CHANNEL`.
6. Spawn your new task from `main()` (`src/main.rs`), passing it the resource
   group from step 3 — same pattern as `sensors::sensors_task(r.sensors)`.
7. Adjust `memory.x` only if you change flash size or need a bigger `PANDUMP`
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
| `info` | Program name/version, flash unique ID (hex), last reset reason, watchdog boot count, and the binary's provenance (`git describe` + commit date, injected at build time by `build.rs`) |
| `temp` | Reads the RP2040's internal temperature sensor — cached value from `sensors.rs`'s background task (async ADC read, EMA-filtered, RP2040 datasheet §4.9.5 calibration formula), not read live inside the command handler |
| `uptime` | Milliseconds since boot |
| `bootsel` | Reboots into BOOTSEL mode (same `rom_data::reset_to_usb_boot` call used by the 1200-baud trick). PICOBOOT only — the RPI-RP2 mass-storage disk stays hidden, which is what `picotool` wants and what keeps `usb-storage` I/O errors out of the host's log |
| `bootsel disk` | Same, but with the RPI-RP2 disk visible. For hosts **without** `picotool` (e.g. a Raspberry Pi that flashes by copying the `.uf2`), which otherwise have no remote way to reprogram the board at all |

These exist to demonstrate reading real chip info and dispatching commands
over embassy channels — replace them with your own commands in `app_task`'s
`match` when using this as a template. The response channel (`TX_CHANNEL` in
`console.rs`) holds 32 pending lines, so a command that replies with several
`TX_CHANNEL.send(...).await` calls (e.g. a multi-line dump) doesn't silently
drop lines the way a shallower channel combined with `try_send` would.

Two honest limitations worth knowing:

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
| `src/main.rs` | Hardware init (USB, flash, watchdog), picotool reset handler, task spawning — no application logic |
| `src/resources.rs` | `assign_resources!` peripheral groups, one per module |
| `src/console.rs` | USB-CDC transport tasks (`usb_task`, `serial_task`) and the command dispatcher (`app_task`) — add your own commands here |
| `src/sensors.rs` | Example of the "module owns the peripheral, consumers read cached state" pattern (internal temp sensor via ADC) — copy this shape for your own sensors |
| `src/watchdog.rs` | Watchdog feed task + consecutive-reset bootloop guard |
| `memory.x` | Linker script — flash/RAM layout, `PANDUMP` region for panic-persist |
| `flash.sh` | Build + port discovery + auto-reset + `picotool load` in one step |
| `test/console_test.py` | Hardware smoke test — drives the console over USB and checks the replies, see [Hardware test](#hardware-test) |
| `CLAUDE.md` | Instructs Claude Code sessions in this repo to use the hardware test before claiming a change is verified |
| `build.rs` | Reruns build on `memory.x` changes and dynamically detects parent configurations to configure target linker flags without duplicating them |
| `.cargo/config.toml` | Target, runner, and commented target flags (handled dynamically by `build.rs`) |
| `embassy-rp2040-usb-guia.md` | Deep-dive walkthrough (Spanish) of how the USB-CDC + panic-persist setup was built, including picotool install/udev rules |
