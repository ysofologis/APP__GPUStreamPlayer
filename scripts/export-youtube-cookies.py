#!/usr/bin/env python3
"""Export YouTube cookies from a running Chrome/Chromium via CDP.

YouTube rate-limits datacenter IPs and answers with *"Sign in to confirm you're
not a bot"*. Cookies from a browser session where you are signed in avoid that.

This script talks to Chrome's DevTools protocol (the `--remote-debugging-port`
that chrome-devtools-mcp or your own launch flags enable), reads the YouTube
cookies, and writes them in Netscape format for yt-dlp:

    ~/.config/stream-player/cookies.txt

Usage:
    python3 scripts/export-youtube-cookies.py [--port 9232] [--out PATH]

If Chrome is not running with a debug port, start it with:

    google-chrome --remote-debugging-port=9232
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import struct
import sys
import urllib.request
from pathlib import Path

DEFAULT_PORT = 9232
DEFAULT_OUT = Path.home() / ".config" / "stream-player" / "cookies.txt"


def cdp_call(port: int, method: str, params: dict | None = None):
    """Send one CDP command over the page target's WebSocket."""
    targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json"))
    page = next((t for t in targets if t.get("type") == "page"), None)
    if not page:
        raise RuntimeError("no page target; is Chrome running with --remote-debugging-port?")

    ws_url = page["webSocketDebuggerUrl"]
    hostport, _, path = ws_url.split("://", 1)[1].partition("/")
    host, port_str = hostport.split(":")
    sock = socket.create_connection((host, int(port_str)), timeout=15)

    key = base64.b64encode(os.urandom(16)).decode()
    sock.send(
        (
            f"GET /{path} HTTP/1.1\r\n"
            f"Host: {hostport}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )

    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("connection closed during handshake")
        buf += chunk

    payload = json.dumps({"id": 1, "method": method, "params": params or {}}).encode()
    mask = os.urandom(4)
    header = bytes([0x81])
    n = len(payload)
    if n < 126:
        header += bytes([0x80 | n])
    elif n < 65536:
        header += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        header += bytes([0x80 | 127]) + struct.pack(">Q", n)
    sock.send(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def read_frame(sock):
        head = sock.recv(2)
        if len(head) < 2:
            return None
        length = head[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", sock.recv(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", sock.recv(8))[0]
        data = b""
        while len(data) < length:
            chunk = sock.recv(length - len(data))
            if not chunk:
                break
            data += chunk
        return data

    try:
        while True:
            frame = read_frame(sock)
            if frame is None:
                raise RuntimeError("connection closed while waiting for response")
            try:
                message = json.loads(frame)
            except json.JSONDecodeError:
                continue
            if message.get("id") == 1:
                return message
    finally:
        sock.close()


def to_netscape(cookies: list[dict]) -> str:
    lines = [
        "# Netscape HTTP Cookie File",
        "# Exported for stream-player by scripts/export-youtube-cookies.py",
        "",
    ]
    for c in cookies:
        domain = c.get("domain", "")
        include_sub = "TRUE" if domain.startswith(".") else "FALSE"
        path = c.get("path", "/")
        secure = "TRUE" if c.get("secure") else "FALSE"
        expires = int(c.get("expires") or 0)
        if expires < 0:
            expires = 0
        lines.append(
            "\t".join(
                [
                    domain,
                    include_sub,
                    path,
                    secure,
                    str(expires),
                    c.get("name", ""),
                    c.get("value", ""),
                ]
            )
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT, help="CDP port (default 9232)")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="output file")
    args = parser.parse_args()

    try:
        response = cdp_call(args.port, "Network.getAllCookies")
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        print(
            "\nStart Chrome with a debug port first:\n"
            "  google-chrome --remote-debugging-port=9232",
            file=sys.stderr,
        )
        return 1

    cookies = response.get("result", {}).get("cookies", [])
    youtube = [c for c in cookies if "youtube" in c.get("domain", "")]
    if not youtube:
        print("error: no YouTube cookies found in that browser", file=sys.stderr)
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(to_netscape(youtube))
    print(f"wrote {len(youtube)} YouTube cookies to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
