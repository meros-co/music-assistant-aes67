"""Helpers for the AES67 provider: ids and the output configuration."""

from __future__ import annotations

import hashlib
import ipaddress
from typing import Any

from .constants import (
    CONF_ADDRESS,
    CONF_CHANNEL_MAP,
    CONF_CHANNELS,
    CONF_ENCODING,
    CONF_KEEP_ALIVE,
    CONF_OUTPUT_NAME,
    CONF_PTIME,
    CONF_RTP_PORT,
    CONF_SAMPLE_RATE,
    CONF_SESSION_NAME,
    CONF_TTL,
    DEFAULT_CHANNEL_MAP,
    DEFAULT_CHANNELS,
    DEFAULT_ENCODING,
    DEFAULT_PTIME,
    DEFAULT_RTP_PORT,
    DEFAULT_SAMPLE_RATE,
    DEFAULT_TTL,
    ENCODINGS,
    PTIMES_US,
    SAMPLE_RATES,
)

# the setup values that make up an output's stream configuration
OUTPUT_KEYS = (
    CONF_OUTPUT_NAME,
    CONF_ADDRESS,
    CONF_RTP_PORT,
    CONF_CHANNELS,
    CONF_CHANNEL_MAP,
    CONF_SAMPLE_RATE,
    CONF_PTIME,
    CONF_ENCODING,
    CONF_TTL,
    CONF_SESSION_NAME,
    CONF_KEEP_ALIVE,
)


def player_id_for(instance_id: str) -> str:
    """
    Return the stable player id of the output a provider instance is.

    :param instance_id: The provider instance id.
    """
    return "aes67_" + hashlib.sha1(instance_id.encode()).hexdigest()[:8]


def parse_channel_map(value: str, channels: int) -> list[int]:
    """
    Parse a channel map such as "1,2" (left to 1, right to 2) or "5" (mono to 5).

    :param value: The map as the user typed it.
    :param channels: The stream's channel count.
    :raises ValueError: When the map is not one or two distinct channels of the stream.
    """
    parts = [p for p in value.replace(" ", ",").split(",") if p]
    try:
        mapped = [int(p) for p in parts]
    except ValueError as err:
        raise ValueError(f"'{value}' is not a list of channel numbers") from err
    if not 1 <= len(mapped) <= 2:
        raise ValueError("use one channel (mono) or two (left, right)")
    if len(set(mapped)) != len(mapped):
        raise ValueError("a channel is used twice")
    if any(not 1 <= c <= channels for c in mapped):
        raise ValueError(f"channels are numbered 1 to {channels}")
    return mapped


def output_config(values: dict[str, Any]) -> dict[str, Any]:
    """
    Build the sender's output configuration from the setup values.

    :param values: The values collected by the setup flow.
    :raises ValueError: When a value is out of range.
    """
    name = str(values.get(CONF_OUTPUT_NAME) or "").strip()
    if not name:
        raise ValueError("the output needs a name")
    try:
        address = ipaddress.IPv4Address(str(values.get(CONF_ADDRESS, "")).strip())
    except ValueError as err:
        raise ValueError("the destination must be an IPv4 address") from err
    if address.is_unspecified or address == ipaddress.IPv4Address("255.255.255.255"):
        raise ValueError(f"{address} is not a destination")
    channels = int(values.get(CONF_CHANNELS) or DEFAULT_CHANNELS)
    if not 1 <= channels <= 64:
        raise ValueError("a stream has 1 to 64 channels")
    sample_rate = int(values.get(CONF_SAMPLE_RATE) or DEFAULT_SAMPLE_RATE)
    if sample_rate not in SAMPLE_RATES:
        raise ValueError(f"sample rate {sample_rate} is not supported")
    ptime = int(values.get(CONF_PTIME) or DEFAULT_PTIME)
    if ptime not in PTIMES_US:
        raise ValueError(f"packet time {ptime} is not supported")
    encoding = str(values.get(CONF_ENCODING) or DEFAULT_ENCODING)
    if encoding not in ENCODINGS:
        raise ValueError(f"encoding {encoding} is not supported")
    port = int(values.get(CONF_RTP_PORT) or DEFAULT_RTP_PORT)
    if not 1 <= port <= 65535:
        raise ValueError("the port is 1 to 65535")
    return {
        "name": name,
        "session_name": str(values.get(CONF_SESSION_NAME) or ""),
        "address": str(address),
        "port": port,
        "channels": channels,
        "sample_rate": sample_rate,
        "ptime_us": ptime,
        "encoding": encoding,
        "ttl": int(values.get(CONF_TTL) or DEFAULT_TTL),
        "channel_map": parse_channel_map(
            str(values.get(CONF_CHANNEL_MAP) or DEFAULT_CHANNEL_MAP), channels
        ),
        "keep_alive": values.get(CONF_KEEP_ALIVE) is not False,
    }
