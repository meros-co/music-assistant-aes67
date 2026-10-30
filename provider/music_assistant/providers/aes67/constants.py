"""Constants for the AES67 provider."""

from __future__ import annotations

from typing import Final

DOMAIN: Final = "aes67"

CONF_SENDER_HOST: Final = "sender_host"
CONF_SENDER_PORT: Final = "sender_port"
CONF_SENDER_TOKEN: Final = "sender_token"  # noqa: S105
CONF_OUTPUT_NAME: Final = "output_name"
CONF_ADDRESS: Final = "address"
CONF_RTP_PORT: Final = "rtp_port"
CONF_CHANNELS: Final = "channels"
CONF_CHANNEL_MAP: Final = "channel_map"
CONF_SAMPLE_RATE: Final = "sample_rate"
CONF_PTIME: Final = "ptime_us"
CONF_TTL: Final = "ttl"
CONF_SESSION_NAME: Final = "session_name"
CONF_KEEP_ALIVE: Final = "keep_alive"

DEFAULT_SENDER_HOST: Final = "127.0.0.1"
DEFAULT_SENDER_PORT: Final = 8890
DEFAULT_RTP_PORT: Final = 5004
DEFAULT_CHANNELS: Final = 2
DEFAULT_CHANNEL_MAP: Final = "1,2"
DEFAULT_SAMPLE_RATE: Final = 48000
DEFAULT_PTIME: Final = 1000
DEFAULT_TTL: Final = 15

SAMPLE_RATES: Final = (44100, 48000, 96000)
PTIMES_US: Final = (125, 250, 333, 1000, 4000)

# how often the player reads the sender's state while playing, and while idle
POLL_INTERVAL_PLAYING: Final = 1
POLL_INTERVAL_IDLE: Final = 10
