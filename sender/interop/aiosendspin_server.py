"""
Interop check: drive a running AES67 Sender with aiosendspin's own server.

aiosendspin is the Sendspin server library Music Assistant uses. This script dials
one output's Sendspin endpoint, approves it as Music Assistant does for an unpaired
guest player, streams two seconds of a 1 kHz tone, and listens for the output's AES67
stream to confirm the tone arrives.

    python aiosendspin_server.py ws://<sender>:8928/sendspin/<id> <group> <port>

Needs Python 3.14 with aiosendspin[server] installed (as in a Music Assistant venv).
"""

from __future__ import annotations

import asyncio
import math
import socket
import struct
import sys
import threading

from aiosendspin.noise import Identity, InMemoryServerPairingStore
from aiosendspin.server import AudioFormat, SendspinServer

RATE = 48000
CHUNK = 1200  # 25 ms, as Music Assistant sends


def tone(start: int, frames: int) -> bytes:
    """Return interleaved 16-bit stereo frames of a 1 kHz tone at half scale."""
    out = bytearray()
    for n in range(start, start + frames):
        s = int(16383 * math.sin(2 * math.pi * 1000 * n / RATE))
        out += struct.pack("<hh", s, s)
    return bytes(out)


def listen(group: str, port: int, stop: threading.Event, stats: dict[str, int]) -> None:
    """Count AES67 packets and the ones that carry sound."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("", port))
    mreq = struct.pack("4s4s", socket.inet_aton(group), socket.inet_aton("0.0.0.0"))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, mreq)
    sock.settimeout(0.2)
    while not stop.is_set():
        try:
            data = sock.recv(1500)
        except TimeoutError:
            continue
        stats["packets"] += 1
        payload = data[12:]
        peak = 0
        for i in range(0, len(payload) - 2, 3):
            v = int.from_bytes(payload[i : i + 3], "big", signed=True)
            peak = max(peak, abs(v))
        stats["peak"] = max(stats["peak"], peak)
        if peak > 100_000:
            stats["loud"] += 1


async def main(url: str, group: str, port: int) -> int:
    """Run the check; 0 when the tone came out of the AES67 stream."""
    stats = {"packets": 0, "loud": 0, "peak": 0}
    stop = threading.Event()
    listener = threading.Thread(target=listen, args=(group, port, stop, stats), daemon=True)
    listener.start()

    loop = asyncio.get_running_loop()
    server = SendspinServer(
        loop, Identity.generate(), "Interop check", pairing_store=InMemoryServerPairingStore()
    )
    connected: asyncio.Future[str] = loop.create_future()

    def on_event(_server: SendspinServer, event: object) -> None:
        client_id = getattr(event, "client_id", None)
        if type(event).__name__ == "ClientConnectedEvent" and client_id and not connected.done():
            connected.set_result(client_id)

    server.add_event_listener(on_event)
    await server.connect_to_client_and_wait(url)
    client_id = await asyncio.wait_for(connected, 15)
    print(f"connected: {client_id}")
    # what Music Assistant does for an unpaired guest that only asks for the player role
    await server.trust_unpaired(client_id)
    await asyncio.sleep(3)

    client = server.get_client(client_id)
    assert client is not None
    stream = client.group.start_stream()
    fmt = AudioFormat(sample_rate=RATE, bit_depth=16, channels=2)
    for i in range(80):
        stream.prepare_audio(tone(i * CHUNK, CHUNK), fmt)
        await stream.commit_audio()
        await stream.sleep_to_limit_buffer(500_000)
    await asyncio.sleep(1.5)
    await client.group.stop()
    await asyncio.sleep(0.5)
    stop.set()
    listener.join()
    print(f"AES67 packets: {stats['packets']}, with the tone: {stats['loud']}, peak {stats['peak']}")
    ok = stats["loud"] >= 1500 and 3_000_000 < stats["peak"] < 4_300_000
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1], sys.argv[2], int(sys.argv[3]))))
