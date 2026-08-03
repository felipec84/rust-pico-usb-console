#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = ["pyserial"]
# ///
"""Hardware smoke test for the USB-CDC console skeleton.

Finds a connected Pico running this firmware and exercises the built-in
commands (help/info/temp/uptime) plus the DTR session-boundary behavior
documented in the README, then reports pass/fail per check.

Two use cases:

1. Human, after flashing:
       uv run test/console_test.py

2. Coding agent (Claude Code or similar), before claiming a change is
   hardware-verified:
       uv run test/console_test.py --check
   This does discovery ONLY — no serial traffic — and exits 0 if exactly one
   matching device is present, 1 otherwise (including "board is present but
   currently sitting in BOOTSEL", which shows up as "not found" here since
   BOOTSEL mode doesn't expose a CDC port). Use this to decide whether to
   claim "verified on hardware" or "not hardware-tested" instead of guessing.

Discovery matches USB VID:PID 2E8A:000A (this skeleton's stock values) by
default. If you changed the VID/PID in main.rs for your own project, set
PICO_VID/PICO_PID (hex, no "0x" prefix) to match, or just pass --port /
set PICO_PORT to skip discovery entirely — same convention as flash.sh.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import time

import serial
import serial.tools.list_ports

DEFAULT_VID = os.environ.get("PICO_VID", "2E8A")
DEFAULT_PID = os.environ.get("PICO_PID", "000A")
BAUD = 115200


def find_ports(vid: str, pid: str) -> list[str]:
    """Ports whose VID:PID matches (case-insensitive). Doesn't open anything."""
    needle = f"{vid}:{pid}".upper()
    matches = []
    for p in serial.tools.list_ports.comports():
        hwid = (p.hwid or "").upper()
        if needle in hwid:
            matches.append(p.device)
    return sorted(matches)


def resolve_port(args: argparse.Namespace) -> str | None:
    if args.port:
        return args.port
    env_port = os.environ.get("PICO_PORT")
    if env_port:
        return env_port
    matches = find_ports(args.vid, args.pid)
    if len(matches) == 1:
        return matches[0]
    if len(matches) > 1:
        print(f"AMBIGUOUS: multiple devices match {args.vid}:{args.pid}: {matches}")
        print("Set PICO_PORT or pass --port to disambiguate.")
        return None
    return None


class Check:
    def __init__(self, name: str):
        self.name = name
        self.ok: bool | None = None
        self.detail = ""

    def passed(self, detail: str = ""):
        self.ok = True
        self.detail = detail

    def failed(self, detail: str = ""):
        self.ok = False
        self.detail = detail

    def report(self):
        mark = "PASS" if self.ok else "FAIL"
        line = f"[{mark}] {self.name}"
        if self.detail:
            line += f" — {self.detail}"
        print(line)


def read_until_quiet(ser: serial.Serial, quiet_ms: int = 300, max_s: float = 2.0) -> str:
    """Reads whatever the firmware sends until `quiet_ms` passes with no new
    bytes, or `max_s` total elapsed — mirrors the firmware's own "drain until
    quiet" logic (see serial_task's post-open purge in console.rs)."""
    ser.timeout = quiet_ms / 1000.0
    deadline = time.monotonic() + max_s
    chunks = []
    while time.monotonic() < deadline:
        data = ser.read(4096)
        if data:
            chunks.append(data)
        else:
            if chunks:
                break
    return b"".join(chunks).decode("utf-8", errors="replace")


def send_command(ser: serial.Serial, cmd: str, max_s: float = 1.5) -> str:
    ser.reset_input_buffer()
    ser.write((cmd + "\r\n").encode("ascii"))
    return read_until_quiet(ser, quiet_ms=250, max_s=max_s)


def open_fresh_session(port: str) -> serial.Serial:
    """Opens the port and forces a real DTR low->high edge.

    pyserial raises DTR automatically on open, but if the line was already
    high — a lingering handle from a previous run, or a host process like
    ModemManager that auto-probes new ttyACM devices — that "raise" isn't a
    detectable edge, and the firmware (which sends the banner on DTR
    low->high, see serial_task in console.rs) never notices a new session.
    Forcing the drop ourselves guarantees the firmware sees a fresh open
    regardless of whatever state the line was already in.
    """
    ser = serial.Serial(port, BAUD, timeout=1)
    ser.dtr = False
    time.sleep(0.1)
    ser.reset_input_buffer()
    ser.dtr = True
    return ser


def run_tests(port: str, include_bootsel: bool) -> list[Check]:
    checks: list[Check] = []

    c = Check(f"open {port} and read initial banner")
    try:
        ser = open_fresh_session(port)
    except serial.SerialException as e:
        c.failed(str(e))
        checks.append(c)
        return checks

    # Opening the port raises DTR — same trigger the firmware's serial_task
    # waits on (class.dtr()). Give it a moment, then read the banner + the
    # post-open purge window (~300ms, see console.rs).
    banner = read_until_quiet(ser, quiet_ms=400, max_s=2.0)
    if "help" in banner.lower() or banner.strip():
        c.passed(repr(banner.strip()[:60]))
    else:
        c.failed("no banner received — is the firmware actually running (not BOOTSEL)?")
    checks.append(c)

    c = Check("'help' lists commands")
    resp = send_command(ser, "help")
    if "comandos" in resp.lower() or "help" in resp.lower():
        c.passed()
    else:
        c.failed(repr(resp[:120]))
    checks.append(c)

    c = Check("'info' reports flash UID and reset reason")
    resp = send_command(ser, "info")
    if re.search(r"flash uid", resp, re.IGNORECASE) and re.search(
        r"reset", resp, re.IGNORECASE
    ):
        c.passed()
    else:
        c.failed(repr(resp[:200]))
    checks.append(c)

    c = Check("'temp' reports a plausible temperature")
    resp = send_command(ser, "temp")
    m = re.search(r"(-?\d+\.\d)\s*C", resp)
    if m and -20.0 <= float(m.group(1)) <= 85.0:
        c.passed(f"{m.group(1)} C")
    else:
        c.failed(repr(resp[:120]))
    checks.append(c)

    c = Check("'uptime' reports increasing milliseconds")
    resp1 = send_command(ser, "uptime")
    m1 = re.search(r"Uptime:\s*(\d+)\s*ms", resp1)
    time.sleep(0.3)
    resp2 = send_command(ser, "uptime")
    m2 = re.search(r"Uptime:\s*(\d+)\s*ms", resp2)
    if m1 and m2 and int(m2.group(1)) > int(m1.group(1)):
        c.passed(f"{m1.group(1)}ms -> {m2.group(1)}ms")
    else:
        c.failed(f"{resp1[:60]!r} / {resp2[:60]!r}")
    checks.append(c)

    c = Check("unknown command gets a clean error, not silence")
    resp = send_command(ser, "definitely-not-a-real-command")
    if "desconocido" in resp.lower() or "unknown" in resp.lower():
        c.passed()
    else:
        c.failed(repr(resp[:120]))
    checks.append(c)

    ser.close()

    # ── DTR session-boundary check ──────────────────────────────────────
    # README claims: reopening the port doesn't glue leftover echo/garbage
    # onto the first real command. Reopen twice quickly (simulating a
    # terminal program probing the port) then confirm a real command still
    # gets a clean reply.
    c = Check("reopen-then-command has no stale-prefix garbage")
    try:
        for _ in range(2):
            probe = serial.Serial(port, BAUD, timeout=0.3)
            time.sleep(0.15)
            probe.close()
        ser = open_fresh_session(port)
        read_until_quiet(ser, quiet_ms=400, max_s=2.0)  # banner + purge
        resp = send_command(ser, "uptime")
        if re.search(r"Uptime:\s*\d+\s*ms", resp) and "desconocido" not in resp.lower():
            c.passed()
        else:
            c.failed(repr(resp[:120]))
        ser.close()
    except serial.SerialException as e:
        c.failed(str(e))
    checks.append(c)

    if include_bootsel:
        c = Check("'bootsel' reboots into BOOTSEL (device will disappear)")
        try:
            ser = open_fresh_session(port)
            read_until_quiet(ser, quiet_ms=400, max_s=2.0)
            ser.write(b"bootsel\r\n")
            time.sleep(0.5)
            ser.close()
            # Give the reboot a moment, then confirm the CDC port is gone.
            time.sleep(1.5)
            still_there = port in find_ports(DEFAULT_VID, DEFAULT_PID)
            if not still_there:
                c.passed("port disappeared — reboot to BOOTSEL succeeded")
                print("  NOTE: board is now in BOOTSEL. Run ./flash.sh to restore firmware.")
            else:
                c.failed("port is still present — reboot may not have happened")
        except serial.SerialException as e:
            c.passed(f"port dropped mid-command ({e}) — consistent with reboot")
        checks.append(c)

    return checks


def run_info(port: str) -> int:
    """Opens the port, forces a fresh DTR edge, sends 'info' and prints the
    raw response. Exits 0 if the board answered, 1 otherwise. Meant for
    scripted measurement (see reenum_loop.sh), not for asserting content."""
    try:
        ser = open_fresh_session(port)
    except serial.SerialException as e:
        print(f"ERROR: could not open {port}: {e}")
        return 1

    try:
        read_until_quiet(ser, quiet_ms=400, max_s=2.0)  # banner + purge
        resp = send_command(ser, "info")
    finally:
        ser.close()

    if resp.strip():
        print(resp.strip())
        return 0
    print("ERROR: no response to 'info'")
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", help="Serial port to use, skips discovery (same as PICO_PORT env var)")
    ap.add_argument("--vid", default=DEFAULT_VID, help=f"USB VID to match (default {DEFAULT_VID})")
    ap.add_argument("--pid", default=DEFAULT_PID, help=f"USB PID to match (default {DEFAULT_PID})")
    ap.add_argument(
        "--check",
        action="store_true",
        help="Discovery only: print the matching port and exit 0/1. No serial traffic.",
    )
    ap.add_argument(
        "--include-bootsel",
        action="store_true",
        help="Also test the 'bootsel' command. Leaves the board in BOOTSEL afterward — "
        "you'll need to run ./flash.sh again to restore the firmware.",
    )
    ap.add_argument(
        "--info",
        action="store_true",
        help="Connect, send 'info', print the raw response and exit 0/1. "
        "No pass/fail checks — meant for scripted measurement (see reenum_loop.sh).",
    )
    args = ap.parse_args()

    if args.check:
        port = resolve_port(args)
        if port:
            print(f"FOUND {port}")
            return 0
        print("NOT FOUND")
        return 1

    if args.info:
        port = resolve_port(args)
        if not port:
            print(f"No device found matching VID:PID {args.vid}:{args.pid}.")
            return 1
        return run_info(port)

    port = resolve_port(args)
    if not port:
        print(f"No device found matching VID:PID {args.vid}:{args.pid}.")
        print("Is the Pico connected and running this firmware (not sitting in BOOTSEL)?")
        print("Set PICO_PORT or pass --port to override discovery.")
        return 1

    print(f"Testing console on {port}...\n")
    checks = run_tests(port, args.include_bootsel)
    print()
    for c in checks:
        c.report()

    failed = [c for c in checks if not c.ok]
    print(f"\n{len(checks) - len(failed)}/{len(checks)} checks passed.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
