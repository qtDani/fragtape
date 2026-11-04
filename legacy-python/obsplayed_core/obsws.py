"""Minimal obs-websocket v5 client (stdlib only).

Only what OBSPlayed needs: connect + authenticate, send requests and wait for
their responses, and receive events on a callback. Runs one reader thread.
"""
from __future__ import annotations

import base64
import hashlib
import itertools
import json
import os
import socket
import struct
import threading
from typing import Any, Callable

# Event subscription bits (obs-websocket EventSubscription)
SUB_OUTPUTS = 1 << 6


class ObsWsError(Exception):
    pass


def auth_string(password: str, salt: str, challenge: str) -> str:
    secret = base64.b64encode(hashlib.sha256((password + salt).encode()).digest()).decode()
    return base64.b64encode(hashlib.sha256((secret + challenge).encode()).digest()).decode()


def encode_frame(payload: bytes, opcode: int = 1, mask: bytes | None = None) -> bytes:
    """A single, final, masked client frame (RFC 6455)."""
    mask = mask if mask is not None else os.urandom(4)
    n = len(payload)
    head = bytes([0x80 | opcode])
    if n < 126:
        head += bytes([0x80 | n])
    elif n < 1 << 16:
        head += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        head += bytes([0x80 | 127]) + struct.pack(">Q", n)
    body = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return head + mask + body


class ObsWs:
    def __init__(self, host: str, port: int, password: str, on_event: Callable[[str, dict], None] | None = None,
                 on_close: Callable[[], None] | None = None, timeout: float = 10.0) -> None:
        self.host, self.port, self.password = host, port, password
        self.on_event = on_event or (lambda t, d: None)
        self.on_close = on_close or (lambda: None)
        self.timeout = timeout
        self._sock: socket.socket | None = None
        self._send_lock = threading.Lock()
        self._pending: dict[str, tuple[threading.Event, list]] = {}
        self._ids = itertools.count(1)
        self._identified = threading.Event()
        self._reader: threading.Thread | None = None
        self.closed = True

    # ---- connection --------------------------------------------------------
    def connect(self) -> None:
        try:
            self._connect()
        except BaseException:
            self.close()
            raise

    def _connect(self) -> None:
        sock = socket.create_connection((self.host, self.port), timeout=self.timeout)
        self._sock = sock
        key = base64.b64encode(os.urandom(16)).decode()
        sock.sendall((
            f"GET / HTTP/1.1\r\nHost: {self.host}:{self.port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
            "Sec-WebSocket-Protocol: obswebsocket.json\r\n\r\n"
        ).encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = sock.recv(4096)
            if not chunk:
                raise ObsWsError("connection closed during handshake")
            resp += chunk
        head, _, rest = resp.partition(b"\r\n\r\n")
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ObsWsError(f"handshake refused: {head[:80]!r}")
        sock.settimeout(None)
        self._buf = rest
        self.closed = False
        hello = self._read_message()
        if hello.get("op") != 0:
            raise ObsWsError("expected Hello")
        identify: dict[str, Any] = {"rpcVersion": 1, "eventSubscriptions": SUB_OUTPUTS}
        auth = hello["d"].get("authentication")
        if auth:
            identify["authentication"] = auth_string(self.password, auth["salt"], auth["challenge"])
        self._send({"op": 1, "d": identify})
        msg = self._read_message()
        if msg.get("op") != 2:
            raise ObsWsError("authentication failed")
        self._reader = threading.Thread(target=self._read_loop, name="obsws-reader", daemon=True)
        self._reader.start()

    def close(self) -> None:
        sock, self._sock = self._sock, None
        self.closed = True
        if sock:
            try:
                with self._send_lock:
                    sock.sendall(encode_frame(b"", opcode=8))
            except OSError:
                pass
            try:
                sock.close()
            except OSError:
                pass

    # ---- requests ------------------------------------------------------------
    def request(self, request_type: str, data: dict | None = None, timeout: float | None = None) -> dict:
        if self.closed:
            raise ObsWsError("not connected")
        rid = str(next(self._ids))
        done = threading.Event()
        slot: list = []
        self._pending[rid] = (done, slot)
        d: dict[str, Any] = {"requestType": request_type, "requestId": rid}
        if data:
            d["requestData"] = data
        try:
            self._send({"op": 6, "d": d})
            if not done.wait(timeout or self.timeout):
                raise ObsWsError(f"{request_type}: timed out")
        finally:
            self._pending.pop(rid, None)
        if not slot:
            raise ObsWsError(f"{request_type}: connection closed")
        resp = slot[0]
        status = resp.get("requestStatus", {})
        if not status.get("result"):
            raise ObsWsError(f"{request_type}: {status.get('comment') or status.get('code')}")
        return resp.get("responseData") or {}

    # ---- framing ---------------------------------------------------------------
    def _send(self, msg: dict) -> None:
        if not self._sock:
            raise ObsWsError("not connected")
        with self._send_lock:
            self._sock.sendall(encode_frame(json.dumps(msg).encode()))

    def _recv_exact(self, n: int) -> bytes:
        while len(self._buf) < n:
            chunk = self._sock.recv(65536) if self._sock else b""
            if not chunk:
                raise ConnectionError("closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _read_frame(self) -> tuple[int, bool, bytes]:
        b1, b2 = self._recv_exact(2)
        fin, opcode, n = bool(b1 & 0x80), b1 & 0x0F, b2 & 0x7F
        if n == 126:
            n = struct.unpack(">H", self._recv_exact(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", self._recv_exact(8))[0]
        mask = self._recv_exact(4) if b2 & 0x80 else None
        data = self._recv_exact(n)
        if mask:
            data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        return opcode, fin, data

    def _read_message(self) -> dict:
        parts: list[bytes] = []
        while True:
            opcode, fin, data = self._read_frame()
            if opcode == 8:
                raise ConnectionError("closed by server")
            if opcode == 9:  # ping
                with self._send_lock:
                    if self._sock:
                        self._sock.sendall(encode_frame(data, opcode=10))
                continue
            if opcode == 10:
                continue
            parts.append(data)
            if fin:
                return json.loads(b"".join(parts))

    def _read_loop(self) -> None:
        try:
            while not self.closed:
                msg = self._read_message()
                op, d = msg.get("op"), msg.get("d") or {}
                if op == 7:
                    waiter = self._pending.get(d.get("requestId"))
                    if waiter:
                        waiter[1].append(d)
                        waiter[0].set()
                elif op == 5:
                    try:
                        self.on_event(d.get("eventType", ""), d.get("eventData") or {})
                    except Exception:
                        pass
        except (OSError, ConnectionError, ValueError):
            pass
        finally:
            self.closed = True
            for done, _ in list(self._pending.values()):
                done.set()
            self.on_close()
