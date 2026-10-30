"""Tests for the AES67 provider's helpers."""

from __future__ import annotations

import pytest

from music_assistant.providers.aes67.constants import (
    CONF_ADDRESS,
    CONF_CHANNEL_MAP,
    CONF_CHANNELS,
    CONF_ENCODING,
    CONF_KEEP_ALIVE,
    CONF_OUTPUT_NAME,
    CONF_PTIME,
    CONF_SAMPLE_RATE,
)
from music_assistant.providers.aes67.helpers import (
    output_config,
    parse_channel_map,
    player_id_for,
)


def test_player_id_is_stable_and_distinct() -> None:
    """The same instance always gets the same player id, another instance another."""
    assert player_id_for("aes67--abc") == player_id_for("aes67--abc")
    assert player_id_for("aes67--abc") != player_id_for("aes67--abd")
    assert player_id_for("aes67--abc").startswith("aes67_")
    assert len(player_id_for("aes67--abc")) == len("aes67_") + 8


def test_channel_map() -> None:
    """Channel maps are one or two distinct stream channels."""
    assert parse_channel_map("1,2", 2) == [1, 2]
    assert parse_channel_map(" 7, 8 ", 8) == [7, 8]
    assert parse_channel_map("5", 8) == [5]
    for bad, channels in (("1,1", 2), ("3", 2), ("0", 2), ("1,2,3", 4), ("x", 2), ("", 2)):
        with pytest.raises(ValueError, match=r"channel|twice|one channel"):
            parse_channel_map(bad, channels)


def test_output_config_defaults() -> None:
    """Only a name and a destination are needed."""
    config = output_config({CONF_OUTPUT_NAME: "Lobby", CONF_ADDRESS: "239.69.1.10"})
    assert config == {
        "name": "Lobby",
        "session_name": "",
        "address": "239.69.1.10",
        "port": 5004,
        "channels": 2,
        "sample_rate": 48000,
        "ptime_us": 1000,
        "encoding": "L24",
        "ttl": 15,
        "channel_map": [1, 2],
        "keep_alive": True,
    }


def test_output_config_values() -> None:
    """Every value is carried over and checked."""
    config = output_config(
        {
            CONF_OUTPUT_NAME: "Bar",
            CONF_ADDRESS: "192.168.1.50",
            CONF_CHANNELS: 8,
            CONF_CHANNEL_MAP: "7,8",
            CONF_SAMPLE_RATE: 96000,
            CONF_PTIME: 125,
            CONF_ENCODING: "L16",
            CONF_KEEP_ALIVE: False,
        }
    )
    assert config["encoding"] == "L16"
    assert config["channel_map"] == [7, 8]
    assert config["sample_rate"] == 96000
    assert config["ptime_us"] == 125
    assert config["keep_alive"] is False


@pytest.mark.parametrize(
    ("values", "message"),
    [
        ({CONF_OUTPUT_NAME: "", CONF_ADDRESS: "239.1.1.1"}, "name"),
        ({CONF_OUTPUT_NAME: "x", CONF_ADDRESS: "lobby.local"}, "IPv4"),
        ({CONF_OUTPUT_NAME: "x", CONF_ADDRESS: "0.0.0.0"}, "not a destination"),
        (
            {CONF_OUTPUT_NAME: "x", CONF_ADDRESS: "239.1.1.1", CONF_SAMPLE_RATE: 22050},
            "sample rate",
        ),
        ({CONF_OUTPUT_NAME: "x", CONF_ADDRESS: "239.1.1.1", CONF_PTIME: 2000}, "packet time"),
        ({CONF_OUTPUT_NAME: "x", CONF_ADDRESS: "239.1.1.1", CONF_CHANNEL_MAP: "3"}, "channels"),
        ({CONF_OUTPUT_NAME: "x", CONF_ADDRESS: "239.1.1.1", CONF_ENCODING: "L20"}, "encoding"),
    ],
)
def test_output_config_refuses(values: dict[str, object], message: str) -> None:
    """Bad settings are refused with a reason."""
    with pytest.raises(ValueError, match=message):
        output_config(values)
