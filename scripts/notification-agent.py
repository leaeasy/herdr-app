#!/usr/bin/env python3
"""Harmless blocked-agent fixture. Run in Herdr, then switch away during --delay."""

import argparse
import json
import os
from pathlib import Path
import select
import signal
import sys
import termios
import time
import tty


def report(state, app):
    sys.stdout.write(f"\033]7501;state={state}:app={app}\033\\")
    sys.stdout.flush()


def receive(timeout, app):
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    received = bytearray()
    try:
        tty.setraw(fd)
        report("blocked", app)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not select.select([fd], [], [], min(.2, max(0, deadline - time.monotonic())))[0]:
                continue
            char = os.read(fd, 1)
            if not char or char in (b"\x03", b"\x04"):
                raise KeyboardInterrupt
            received.extend(char)
            if char in (b"\r", b"\n", b"\x1b"):
                cancel = char == b"\x1b"
                return {"action": "cancel" if cancel else "confirm",
                        "key": "esc" if cancel else "enter",
                        "received_hex": received.hex()}
        raise TimeoutError("No answer received before timeout")
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", choices=["codex", "opencode", "notification-test"], default="codex")
    parser.add_argument("--delay", type=float, default=8)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--result", type=Path)
    args = parser.parse_args()
    if args.delay < 0 or args.timeout <= 0:
        parser.error("delay must be non-negative and timeout must be positive")
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        parser.error("run this fixture in a Herdr terminal")

    def terminate(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGHUP, terminate)
    try:
        report("working", args.app)
        print(f"Test only. Switch to another app within {args.delay:g}s.", flush=True)
        print("Enter = confirm; Esc = cancel. No operation will be performed.", flush=True)
        time.sleep(args.delay)
        result = receive(args.timeout, args.app)
        if args.result:
            args.result.write_text(json.dumps(result) + "\n")
        print("\nReceived: " + json.dumps(result), flush=True)
    finally:
        try:
            report("idle", args.app)
        except OSError:
            pass


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nNotification test cancelled.")
    except Exception as error:
        print(f"Notification test failed: {error}", file=sys.stderr)
        sys.exit(1)
