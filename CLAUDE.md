# Project instructions — rust-pico-usb-console

## Hardware verification

This firmware has a USB-CDC console over which you (or anyone) can drive
real hardware checks — don't guess whether a change works, check.

Before claiming a change is "hardware-verified" or "tested on the Pico":

```sh
uv run test/console_test.py --check
```

This does discovery only (matches the firmware's USB VID:PID, no serial
traffic) and exits 0 if exactly one Pico running this firmware is currently
connected, 1 otherwise. If it returns 1, say plainly that the change is
**not** hardware-tested rather than assuming success from a clean build —
`cargo build`/`clippy` only prove the code compiles, not that it behaves
correctly on the chip.

If a device **is** found, and the change is meaningful enough to warrant it:

```sh
./flash.sh                        # build + flash
uv run test/console_test.py       # exercises help/info/temp/uptime + the
                                   # DTR session-boundary behavior
```

Add `--include-bootsel` only if you intend to also verify the `bootsel`
command — it reboots the board into BOOTSEL and leaves it there, requiring
another `./flash.sh` afterward. Don't pass it by default.

When you add your own console commands (per the README's "Using this as a
template" section), add a matching check to `test/console_test.py` in the
same change — the test script is meant to track whatever commands actually
exist, not just the four built-in examples.

## Branches

`develop` is the buildable reference (real names, what you build/flash/test
against). `master` is the cargo-generate template (has `{{product-name}}` /
`{{crate_name}}` / `{{manufacturer}}` placeholders, does not build). Work on
`develop`; never commit to `master` directly. See README.md for the full
merge flow.
