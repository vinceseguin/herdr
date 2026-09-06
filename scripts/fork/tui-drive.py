#!/usr/bin/env python3
"""Drive a full-screen herdr client through a real PTY.

The Fleet console is a TUI: the only honest way to validate it is to give it a
terminal, wait for something to appear on the screen, type into it and look
again.  This is that harness, for manual runs and for the fork's validation
recipes; `tests/support/fleet_tui.rs` is its Rust twin for the integration
tests.

Two details are load-bearing:

* the window size is set with `TIOCSWINSZ` **before** the child is forked, so
  the client never sees the 0x0 grid it refuses to start on ("terminal
  reported a zero-sized grid");
* the output is stripped of ANSI escapes into plain text, so an expectation is
  matched against what a human would read rather than against styling bytes.

Steps run in the order they appear on the command line::

    tui-drive.py --cols 120 --rows 40 --timeout 30 \\
        --expect 'herdr-fleet-lab:lab-1' \\
        --keys 'echo from-console\\r' \\
        --expect 'from-console' \\
        --keys '\\x02q' --dump \\
        -- target/debug/herdr fleet

Exit status is 0 when every expectation was met, 1 otherwise.  Python 3.10+,
standard library only.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

# How much stripped text to keep.  Generous, but bounded: a chatty client can
# emit megabytes of frames and nobody greps the whole history.
MAX_TEXT = 1 << 20
# How long to wait for the child after the last step before killing it.
EXIT_GRACE_SECONDS = 5.0

# CSI, OSC/DCS/APC/PM (string sequences), charset designators, and the plain
# two-character escapes.  Ordered so the longest form wins.
ANSI = re.compile(
    r"""
    \x1b\[[0-?]*[ -/]*[@-~]          # CSI
  | \x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?  # OSC, terminated by BEL or ST
  | \x1b[P^_X][^\x1b]*(?:\x1b\\)?    # DCS / PM / APC / SOS
  | \x1b[()*+][\x20-\x7e]            # charset designator
  | \x1b[@-Z\\-_]                    # single-character escape
  | \x1b                             # a lone escape
    """,
    re.VERBOSE,
)
# Everything else that is not text.  Newline and tab survive; carriage returns
# become newlines so a redrawn line does not glue itself to the previous one.
CONTROL = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")


def strip_ansi(chunk: str) -> str:
    """Turn terminal output into the text a human would read."""
    return CONTROL.sub("", ANSI.sub("", chunk).replace("\r", "\n"))


def decode_keys(value: str) -> bytes:
    r"""Decode `\r`, `\x02`, `\n`, `\t` and friends into raw bytes."""
    return value.encode("utf-8").decode("unicode_escape").encode("latin-1")


class Step(argparse.Action):
    """Collects --expect/--keys into one ordered list."""

    def __call__(self, parser, namespace, values, option_string=None):
        namespace.steps.append((self.dest, values))


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="tui-drive.py", description="Drive a herdr TUI through a PTY."
    )
    parser.add_argument("--cols", type=int, default=120, help="terminal columns")
    parser.add_argument("--rows", type=int, default=40, help="terminal rows")
    parser.add_argument(
        "--timeout",
        type=float,
        default=30.0,
        help="seconds allowed for each expectation",
    )
    parser.add_argument(
        "--expect",
        action=Step,
        metavar="TEXT",
        help="wait until TEXT appears (repeatable, ordered)",
    )
    parser.add_argument(
        "--keys",
        action=Step,
        metavar="BYTES",
        help=r"send BYTES to the terminal, with \r and \xNN escapes",
    )
    parser.add_argument(
        "--redraw",
        action=Step,
        nargs=0,
        help=(
            "force a full repaint (resize the window and back) and forget the "
            "text so far, so a --dump after it reads as one whole screen"
        ),
    )
    parser.add_argument(
        "--dump", action="store_true", help="print the final screen text to stdout"
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="-- followed by the command to run",
    )
    parser.set_defaults(steps=[])
    args = parser.parse_args(argv)
    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("no command given (put it after --)")
    if args.cols < 1 or args.rows < 1:
        parser.error("--cols and --rows must be positive")
    args.command = command
    return args


def set_winsize(fd: int, cols: int, rows: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def spawn(command: list[str], cols: int, rows: int) -> tuple[int, int]:
    """Fork `command` on a PTY that already has a real window size."""
    master, slave = pty.openpty()
    # Before the fork: the child must never observe a 0x0 grid.
    set_winsize(master, cols, rows)
    pid = os.fork()
    if pid == 0:  # child
        try:
            os.close(master)
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            for target in (0, 1, 2):
                os.dup2(slave, target)
            if slave > 2:
                os.close(slave)
            os.execvp(command[0], command)
        except BaseException:  # pragma: no cover - the child cannot report
            os._exit(127)
        os._exit(127)
    os.close(slave)
    return pid, master


class Screen:
    """The stripped text seen so far, plus the fd it comes from."""

    def __init__(self, fd: int) -> None:
        self.fd = fd
        self.text = ""
        self.closed = False

    def pump(self, timeout: float) -> None:
        """Read whatever is ready, for at most `timeout` seconds."""
        if self.closed:
            time.sleep(min(timeout, 0.05))
            return
        try:
            ready, _, _ = select.select([self.fd], [], [], max(timeout, 0.0))
        except OSError:
            self.closed = True
            return
        if not ready:
            return
        try:
            chunk = os.read(self.fd, 65536)
        except OSError as error:
            # EIO is how a PTY master reports "the child hung up".
            if error.errno not in (errno.EIO, errno.EBADF):
                raise
            self.closed = True
            return
        if not chunk:
            self.closed = True
            return
        self.text += strip_ansi(chunk.decode("utf-8", "replace"))
        if len(self.text) > MAX_TEXT:
            self.text = self.text[-MAX_TEXT:]


def wait_for(screen: Screen, needle: str, start: int, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while True:
        if screen.text.find(needle, start) >= 0:
            return True
        if time.monotonic() >= deadline:
            return False
        if screen.closed and screen.text.find(needle, start) < 0:
            # One last look: the child may have exited right after printing it.
            return False
        screen.pump(0.05)


def reap(pid: int, screen: Screen) -> int | None:
    """Wait a little for the child, then make sure it is gone."""
    deadline = time.monotonic() + EXIT_GRACE_SECONDS
    while time.monotonic() < deadline:
        try:
            done, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return None
        if done == pid:
            return status
        screen.pump(0.05)
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            break
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            try:
                done, status = os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                return None
            if done == pid:
                return status
            screen.pump(0.05)
    return None


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    pid, master = spawn(args.command, args.cols, args.rows)
    screen = Screen(master)
    failed: list[str] = []
    status: int | None = None

    try:
        for kind, value in args.steps:
            if kind == "expect":
                start = len(screen.text)
                if not wait_for(screen, value, start, args.timeout):
                    failed.append(value)
                    print(
                        f"tui-drive: timed out waiting for {value!r}",
                        file=sys.stderr,
                    )
                    break
            elif kind == "redraw":
                # A client draws frame diffs, so a screen that changed one
                # character only ever wrote that character.  Resizing forces a
                # whole frame; dropping the text so far makes the next --dump
                # exactly that frame.
                screen.pump(0.2)
                set_winsize(master, args.cols - 1, args.rows)
                for _ in range(6):
                    screen.pump(0.1)
                screen.text = ""
                set_winsize(master, args.cols, args.rows)
                for _ in range(10):
                    screen.pump(0.1)
            else:
                # Give the client a beat to consume what it was shown before
                # typing into it: input written into a PTY that is not being
                # read yet is buffered, but a TUI that is still painting can
                # drop a mouse report.
                screen.pump(0.1)
                try:
                    os.write(master, decode_keys(value))
                except OSError as error:
                    failed.append(value)
                    print(f"tui-drive: could not send keys: {error}", file=sys.stderr)
                    break
        # Let the last frame (and any exit output) land.
        screen.pump(0.3)
    finally:
        # On every way out — an expectation that failed, a broken PTY, a
        # KeyboardInterrupt — the child is waited for and, failing that,
        # killed: a herdr left behind would keep its hosts' sockets open.
        status = reap(pid, screen)
        screen.pump(0.05)
        try:
            os.close(master)
        except OSError:
            pass

    if args.dump:
        print(screen.text)
    if status is None:
        print("tui-drive: child did not exit on its own", file=sys.stderr)
    elif os.WIFEXITED(status):
        print(f"tui-drive: child exited {os.WEXITSTATUS(status)}", file=sys.stderr)
    else:
        print(f"tui-drive: child killed by signal {os.WTERMSIG(status)}", file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
