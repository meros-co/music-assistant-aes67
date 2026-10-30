"""Tests for the AES67 output player and provider."""

from __future__ import annotations

from types import SimpleNamespace
from typing import cast
from unittest.mock import AsyncMock, MagicMock

import pytest
from music_assistant_models.enums import PlaybackState, PlayerFeature
from music_assistant_models.errors import (
    PlayerCommandFailed,
    PlayerUnavailableError,
    SetupFailedError,
)

from music_assistant.providers.aes67 import setup_flow
from music_assistant.providers.aes67.client import OutputState, SenderError
from music_assistant.providers.aes67.constants import (
    CONF_ADDRESS,
    CONF_OUTPUT_NAME,
    CONF_SENDER_HOST,
)
from music_assistant.providers.aes67.helpers import output_config
from music_assistant.providers.aes67.player import AES67Player
from music_assistant.providers.aes67.provider import AES67Provider

OUTPUT = output_config({CONF_OUTPUT_NAME: "Lobby", CONF_ADDRESS: "239.69.1.10"})


def _state(**kw: object) -> OutputState:
    base: dict[str, object] = {
        "state": "idle",
        "volume": 50,
        "muted": False,
        "elapsed": None,
        "error": None,
        "underrun_frames": 0,
        "group_leader": None,
    }
    base.update(kw)
    return OutputState(**base)  # type: ignore[arg-type]


@pytest.fixture
def provider() -> MagicMock:
    """Return a provider with a mocked sender client."""
    prov = MagicMock()
    prov.instance_id = "aes67--test"
    prov.domain = "aes67"
    prov.manifest = MagicMock()
    prov.manifest.domain = "aes67"
    prov.output = dict(OUTPUT)
    prov.client = MagicMock()
    for name in ("play", "stop", "set_volume", "put_output", "delete_output", "status"):
        setattr(prov.client, name, AsyncMock())
    prov.client.get_output = AsyncMock(return_value=_state())
    prov.mass = MagicMock()
    prov.mass.streams.resolve_stream_url = AsyncMock(return_value="http://ma/flow/1/q/i/p.wav")
    config = MagicMock()
    config.name = None
    config.default_name = "Lobby"
    config.enabled = True
    config.player_type = None
    config.get_value = MagicMock(return_value=None)
    prov.mass.config.get_base_player_config.return_value = config
    return prov


def _player(provider: MagicMock, player_id: str = "aes67_00000001") -> AES67Player:
    player = AES67Player(provider, player_id)
    player.update_state = MagicMock()  # type: ignore[misc,method-assign]
    return player


def test_features_and_device_info(provider: MagicMock) -> None:
    """The player advertises what the sender does and names its stream."""
    player = _player(provider)
    for feature in (
        PlayerFeature.PLAY_MEDIA,
        PlayerFeature.POWER,
        PlayerFeature.VOLUME_SET,
        PlayerFeature.VOLUME_MUTE,
        PlayerFeature.SET_MEMBERS,
    ):
        assert feature in player.supported_features
    assert PlayerFeature.PAUSE not in player.supported_features
    assert player.name == "Lobby"
    assert player.device_info.model == "AES67 output"
    assert player.device_info.model_id == "Lobby @ 239.69.1.10:5004"
    # no IP identifier: a multicast group must never link players across protocols
    assert not player.device_info.identifiers
    assert player._attr_supported_sample_rates == [(48000, 16), (48000, 24)]


async def test_output_codec_is_wav(provider: MagicMock) -> None:
    """The sender reads WAV, so the codec is fixed to it."""
    entries = await _player(provider).get_config_entries()
    codec = next(e for e in entries if e.key == "output_codec")
    assert codec.default_value == "wav"
    assert codec.hidden


async def test_play_media_hands_the_url_and_group_to_the_sender(provider: MagicMock) -> None:
    """Playing sends Music Assistant's stream URL with the group's members."""
    player = _player(provider)
    player._attr_group_members = ["aes67_00000001", "aes67_00000002"]
    media = MagicMock()
    await player.play_media(media)
    provider.client.play.assert_awaited_once_with(
        "aes67_00000001", "http://ma/flow/1/q/i/p.wav", ["aes67_00000002"]
    )
    assert player.playback_state == PlaybackState.PLAYING
    assert player.current_media is media


async def test_stop_volume_mute(provider: MagicMock) -> None:
    """Stop, volume and mute go to the sender and update the state."""
    player = _player(provider)
    await player.volume_set(30)
    provider.client.set_volume.assert_awaited_with("aes67_00000001", level=30)
    assert player.volume_level == 30
    await player.volume_mute(True)
    provider.client.set_volume.assert_awaited_with("aes67_00000001", muted=True)
    assert player.volume_muted
    player._attr_playback_state = PlaybackState.PLAYING
    await player.power(False)
    provider.client.stop.assert_awaited_once_with("aes67_00000001")
    assert player.playback_state == PlaybackState.IDLE
    assert not player.powered


async def test_command_errors(provider: MagicMock) -> None:
    """A refusal fails the command; an unreachable sender makes the player unavailable."""
    player = _player(provider)
    provider.client.set_volume.side_effect = SenderError("bad", 422)
    with pytest.raises(PlayerCommandFailed):
        await player.volume_set(10)
    provider.client.stop.side_effect = SenderError("down")
    with pytest.raises(PlayerUnavailableError):
        await player.stop()
    assert not player.available


async def test_poll_follows_the_sender(provider: MagicMock) -> None:
    """Polling takes elapsed time, volume and the end of the stream from the sender."""
    player = _player(provider)
    provider.client.get_output.return_value = _state(state="playing", elapsed=3.5, volume=70)
    await player.poll()
    assert player._attr_playback_state == PlaybackState.PLAYING
    assert player._attr_elapsed_time == 3.5
    assert player.volume_level == 70
    assert player.poll_interval == 1
    provider.client.get_output.return_value = _state(state="idle")
    await player.poll()
    assert player.state.playback_state == PlaybackState.IDLE
    assert player.poll_interval == 10
    provider.client.get_output.side_effect = SenderError("down")
    await player.poll()
    assert not player.available


def test_groups_with_other_aes67_outputs(provider: MagicMock) -> None:
    """Every other AES67 output is a grouping candidate; other providers are not."""
    player = _player(provider)
    other = SimpleNamespace(player_id="aes67_00000002", provider=SimpleNamespace(domain="aes67"))
    foreign = SimpleNamespace(player_id="cast_1", provider=SimpleNamespace(domain="chromecast"))
    provider.mass.players.iter_players.return_value = [player, other, foreign]
    assert player.can_group_with == {"aes67_00000002"}


async def test_set_members_restarts_a_playing_group(provider: MagicMock) -> None:
    """Adding members to a playing group restarts it so they start in sync."""
    player = _player(provider)
    player._attr_playback_state = PlaybackState.PLAYING
    provider.mass.players.get_player.return_value = None
    await player.set_members(player_ids_to_add=["aes67_00000002"])
    assert player._attr_group_members == ["aes67_00000001", "aes67_00000002"]
    provider.mass.create_task.assert_called_once()
    await player.set_members(player_ids_to_remove=["aes67_00000002"])
    assert player._attr_group_members == []


def _real_provider(values: dict[str, object]) -> AES67Provider:
    prov = AES67Provider.__new__(AES67Provider)
    prov.config = MagicMock()
    prov.config.instance_id = "aes67--test"
    prov.logger = MagicMock()
    prov.mass = MagicMock()
    prov.get_setup_value = MagicMock(side_effect=lambda key, default=None: values.get(key, default))  # type: ignore[method-assign]
    return prov


async def test_provider_creates_the_output_on_the_sender(monkeypatch: pytest.MonkeyPatch) -> None:
    """Loading the provider puts the output on the sender; a refusal fails the setup."""
    client = MagicMock()
    client.put_output = AsyncMock()
    monkeypatch.setattr(
        "music_assistant.providers.aes67.provider.SenderClient", MagicMock(return_value=client)
    )
    prov = _real_provider({CONF_OUTPUT_NAME: "Lobby", CONF_ADDRESS: "239.69.1.10"})
    await prov.handle_async_init()
    client.put_output.assert_awaited_once()
    output_id, config = client.put_output.await_args.args
    assert output_id == prov.player_id
    assert config == OUTPUT

    client.put_output.side_effect = SenderError("channel 1 is already used", 422)
    with pytest.raises(SetupFailedError, match="already used"):
        await prov.handle_async_init()
    with pytest.raises(SetupFailedError, match="Invalid"):
        await _real_provider({CONF_OUTPUT_NAME: "", CONF_ADDRESS: "x"}).handle_async_init()


async def test_removing_the_provider_removes_the_output() -> None:
    """A removed provider deletes its output; a reload leaves it streaming."""
    prov = _real_provider({})
    prov.player_id = "aes67_1"
    prov.client = MagicMock()
    prov.client.delete_output = AsyncMock()
    mass = cast("MagicMock", prov.mass)
    mass.players.iter_players.return_value = []
    mass.players.unregister = AsyncMock()
    await prov.unload(is_removed=False)
    prov.client.delete_output.assert_not_awaited()
    await prov.unload(is_removed=True)
    prov.client.delete_output.assert_awaited_once_with("aes67_1")


async def test_setup_flow_checks_the_settings_and_the_sender(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The flow refuses bad settings and an unreachable sender, then finishes."""
    client = MagicMock()
    client.status = AsyncMock(side_effect=[SenderError("down"), {"api_version": 1}])
    monkeypatch.setattr(
        "music_assistant.providers.aes67.setup_flow.SenderClient", MagicMock(return_value=client)
    )
    session = MagicMock()
    session.context.setup_data = {}
    session.form = AsyncMock(
        side_effect=[
            {CONF_OUTPUT_NAME: "Lobby", CONF_ADDRESS: "not-an-ip"},
            {CONF_ADDRESS: "239.69.1.10"},
            {CONF_SENDER_HOST: "127.0.0.1"},
        ]
    )
    session.finish = AsyncMock(return_value={"instance_id": "aes67--new"})
    await setup_flow.run_setup(session)
    errors = [call.kwargs["errors"] for call in session.form.await_args_list]
    assert errors[0] is None
    assert "base" in errors[1]
    assert errors[2] == {CONF_SENDER_HOST: "cannot_connect"}
    finished = session.finish.await_args.args[0]
    assert finished[CONF_OUTPUT_NAME] == "Lobby"
    assert finished[CONF_ADDRESS] == "239.69.1.10"
