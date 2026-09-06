#!/usr/bin/env python3
"""A minimal RFC 6455 WebSocket client, for driving the fork's gateway.

The fork has no WebSocket client dependency and does not want one: the gateway
is tested and validated through the real binary, and the only thing missing is
something that can speak the handshake and read frames. This is that stand-in.
It uses the python standard library only (>= 3.10) so it runs on any machine
that can run the repo's other maintenance scripts.

Output contract — one line per *message* on stdout, and downstream tooling
(E3's integration tests, E4's browser work, the epic validation) parses it, so
flags may be added but these three line shapes never change:

    text <payload>          one text message; raw newlines become "\\n"
    binary <hex|length>     one binary message; hex with --binary hex, else
                            the byte count
    close <code>            the peer's close frame (its reason goes to stderr)

Control frames are protocol noise, not messages: a ping is answered with a
pong and reported on stderr, and neither is counted by --max-messages.

Exit codes:

    0   the peer closed, or --max-messages messages were printed
    1   a usage or transport error (the message is on stderr)
    2   the HTTP handshake did not become a 101 (the status is on stdout as
        "handshake <status>" and on stderr with the reason)
    3   --timeout elapsed

Usage:

    ws-client.py URL [--header 'Name: value']... [--send JSON]... [--send-stdin]
                     [--max-messages N] [--timeout SECS] [--binary hex|len]
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import os
import socket
import ssl
import struct
import sys
import time
from urllib.parse import urlsplit

# The RFC 6455 handshake constant.
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

# Frame opcodes.
OP_CONTINUATION = 0x0
OP_TEXT = 0x1
OP_BINARY = 0x2
OP_CLOSE = 0x8
OP_PING = 0x9
OP_PONG = 0xA

# Refuse to buffer more than this from the peer. The gateway's own messages are
# far smaller; a bigger one is a bug or a hostile server, and either way this
# process should not grow without bound.
MAX_MESSAGE_BYTES = 8 * 1024 * 1024


class ProtocolError(Exception):
    """The peer sent something that is not a well-formed frame."""


class HandshakeError(Exception):
    """The HTTP upgrade did not happen. Carries the status when there was one."""

    def __init__(self, message: str, status: int | None = None) -> None:
        super().__init__(message)
        self.status = status


class Timeout(Exception):
    """The overall deadline elapsed."""


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def emit(line: str) -> None:
    print(line, flush=True)


class Connection:
    """One WebSocket connection over a blocking socket with a deadline."""

    def __init__(self, sock: socket.socket, deadline: float | None) -> None:
        self.sock = sock
        self.deadline = deadline
        self.buffer = bytearray()

    # -- plumbing ---------------------------------------------------------

    def _arm(self) -> None:
        """Set the socket timeout from the remaining time, or raise."""
        if self.deadline is None:
            self.sock.settimeout(None)
            return
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise Timeout("the deadline elapsed")
        self.sock.settimeout(remaining)

    def read_exactly(self, count: int) -> bytes:
        while len(self.buffer) < count:
            self._arm()
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout as error:
                raise Timeout("the deadline elapsed") from error
            if not chunk:
                raise ProtocolError("the peer closed the connection mid-frame")
            self.buffer.extend(chunk)
        taken = bytes(self.buffer[:count])
        del self.buffer[:count]
        return taken

    def read_line(self) -> bytes:
        """One CRLF-terminated line, for the handshake response."""
        while True:
            index = self.buffer.find(b"\r\n")
            if index >= 0:
                line = bytes(self.buffer[:index])
                del self.buffer[: index + 2]
                return line
            self._arm()
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout as error:
                raise Timeout("the deadline elapsed") from error
            if not chunk:
                raise HandshakeError("the peer closed before the response ended")
            self.buffer.extend(chunk)

    def send_all(self, data: bytes) -> None:
        self._arm()
        try:
            self.sock.sendall(data)
        except socket.timeout as error:
            raise Timeout("the deadline elapsed") from error

    # -- frames -----------------------------------------------------------

    def send_frame(self, opcode: int, payload: bytes) -> None:
        """One masked, unfragmented frame — a client must always mask."""
        header = bytearray()
        header.append(0x80 | opcode)
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < (1 << 16):
            header.append(0x80 | 126)
            header.extend(struct.pack("!H", length))
        else:
            header.append(0x80 | 127)
            header.extend(struct.pack("!Q", length))
        mask = os.urandom(4)
        header.extend(mask)
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.send_all(bytes(header) + masked)

    def read_frame(self) -> tuple[bool, int, bytes]:
        """`(fin, opcode, payload)` for one frame, unmasked if it was masked."""
        first, second = self.read_exactly(2)
        fin = bool(first & 0x80)
        if first & 0x70:
            raise ProtocolError("a reserved bit was set")
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        if length == 126:
            (length,) = struct.unpack("!H", self.read_exactly(2))
        elif length == 127:
            (length,) = struct.unpack("!Q", self.read_exactly(8))
        if length > MAX_MESSAGE_BYTES:
            raise ProtocolError(f"the peer sent a {length}-byte frame")
        mask = self.read_exactly(4) if masked else b""
        payload = self.read_exactly(length)
        if masked:
            payload = bytes(
                byte ^ mask[index % 4] for index, byte in enumerate(payload)
            )
        return fin, opcode, payload

    def close(self, code: int = 1000) -> None:
        try:
            self.send_frame(OP_CLOSE, struct.pack("!H", code))
        except (OSError, Timeout, ProtocolError):
            pass
        try:
            self.sock.close()
        except OSError:
            pass


def handshake(url: str, headers: list[str], deadline: float | None) -> Connection:
    parts = urlsplit(url)
    if parts.scheme not in ("ws", "wss"):
        raise HandshakeError(f"not a WebSocket URL: {url}")
    secure = parts.scheme == "wss"
    host = parts.hostname
    if not host:
        raise HandshakeError(f"no host in {url}")
    port = parts.port or (443 if secure else 80)
    target = parts.path or "/"
    if parts.query:
        target = f"{target}?{parts.query}"

    timeout = None if deadline is None else max(deadline - time.monotonic(), 0.001)
    sock = socket.create_connection((host, port), timeout=timeout)
    if secure:
        context = ssl.create_default_context()
        sock = context.wrap_socket(sock, server_hostname=host)

    key = base64.b64encode(os.urandom(16)).decode("ascii")
    host_header = host if parts.port is None else f"{host}:{parts.port}"
    lines = [
        f"GET {target} HTTP/1.1",
        f"Host: {host_header}",
        "Upgrade: websocket",
        "Connection: Upgrade",
        f"Sec-WebSocket-Key: {key}",
        "Sec-WebSocket-Version: 13",
    ]
    for header in headers:
        name, separator, value = header.partition(":")
        if not separator:
            raise HandshakeError(f"a header must be 'Name: value': {header}")
        lines.append(f"{name.strip()}: {value.strip()}")
    request = ("\r\n".join(lines) + "\r\n\r\n").encode("utf-8")

    connection = Connection(sock, deadline)
    connection.send_all(request)

    status_line = connection.read_line().decode("latin-1")
    fields = status_line.split(" ", 2)
    status: int | None = None
    if len(fields) >= 2 and fields[1].isdigit():
        status = int(fields[1])
    response_headers: dict[str, str] = {}
    while True:
        line = connection.read_line()
        if not line:
            break
        name, separator, value = line.decode("latin-1").partition(":")
        if separator:
            response_headers[name.strip().lower()] = value.strip()

    if status != 101:
        connection.sock.close()
        raise HandshakeError(f"the server answered {status_line.strip()}", status)

    expected = base64.b64encode(hashlib.sha1((key + GUID).encode("ascii")).digest())
    accept = response_headers.get("sec-websocket-accept", "")
    if accept != expected.decode("ascii"):
        connection.sock.close()
        raise HandshakeError("the Sec-WebSocket-Accept header did not match", status)
    return connection


def format_text(payload: bytes) -> str:
    """One line for a text message, with raw newlines escaped.

    The gateway's messages are newline-free by contract; escaping keeps the
    one-line-per-message shape honest if that ever stops being true.
    """
    text = payload.decode("utf-8", errors="replace")
    return text.replace("\r\n", "\\n").replace("\n", "\\n").replace("\r", "\\n")


def run(args: argparse.Namespace) -> int:
    deadline = None if args.timeout <= 0 else time.monotonic() + args.timeout
    try:
        connection = handshake(args.url, args.header, deadline)
    except HandshakeError as error:
        if error.status is not None:
            emit(f"handshake {error.status}")
        log(f"handshake failed: {error}")
        return 2
    except Timeout:
        log("handshake timed out")
        return 3
    except OSError as error:
        log(f"could not connect: {error}")
        return 1

    try:
        for payload in args.send:
            connection.send_frame(OP_TEXT, payload.encode("utf-8"))
        if args.send_stdin:
            connection.send_frame(OP_TEXT, sys.stdin.buffer.read())

        printed = 0
        opcode: int | None = None
        message = bytearray()
        while True:
            if args.max_messages is not None and printed >= args.max_messages:
                connection.close(1000)
                return 0
            fin, frame_opcode, payload = connection.read_frame()

            if frame_opcode == OP_PING:
                log("ping")
                connection.send_frame(OP_PONG, payload)
                continue
            if frame_opcode == OP_PONG:
                log("pong")
                continue
            if frame_opcode == OP_CLOSE:
                code = struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 1005
                reason = payload[2:].decode("utf-8", errors="replace")
                emit(f"close {code}")
                if reason:
                    log(f"close reason: {reason}")
                connection.close(1000)
                return 0

            if frame_opcode == OP_CONTINUATION:
                if opcode is None:
                    raise ProtocolError("a continuation frame began a message")
            elif frame_opcode in (OP_TEXT, OP_BINARY):
                if opcode is not None:
                    raise ProtocolError("a new message began inside another")
                opcode = frame_opcode
            else:
                raise ProtocolError(f"unknown opcode {frame_opcode}")

            message.extend(payload)
            if len(message) > MAX_MESSAGE_BYTES:
                raise ProtocolError("the peer sent an oversized message")
            if not fin:
                continue

            if opcode == OP_TEXT:
                emit(f"text {format_text(bytes(message))}")
            elif args.binary == "hex":
                emit(f"binary {bytes(message).hex()}")
            else:
                emit(f"binary {len(message)}")
            printed += 1
            opcode = None
            message = bytearray()
    except Timeout:
        log("timed out")
        connection.close(1001)
        return 3
    except ProtocolError as error:
        log(f"protocol error: {error}")
        connection.close(1002)
        return 1
    except OSError as error:
        log(f"transport error: {error}")
        connection.close(1001)
        return 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="ws-client.py",
        description="A stdlib-only WebSocket client for the herdr gateway.",
    )
    parser.add_argument("url", help="ws:// or wss:// URL")
    parser.add_argument(
        "-H",
        "--header",
        action="append",
        default=[],
        metavar="'Name: value'",
        help="an extra request header; repeatable",
    )
    parser.add_argument(
        "--send",
        action="append",
        default=[],
        metavar="JSON",
        help="a text message to send after connecting; repeatable",
    )
    parser.add_argument(
        "--send-stdin",
        action="store_true",
        help="send everything on stdin as one text message",
    )
    parser.add_argument(
        "--max-messages",
        type=int,
        default=None,
        metavar="N",
        help="stop after N messages have been printed",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=10.0,
        metavar="SECS",
        help="overall deadline in seconds (0 waits forever); default 10",
    )
    parser.add_argument(
        "--binary",
        choices=("hex", "len"),
        default="len",
        help="print binary messages as hex or as a byte count; default len",
    )
    args = parser.parse_args(argv)
    if args.max_messages is not None and args.max_messages <= 0:
        parser.error("--max-messages must be positive")
    return run(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
