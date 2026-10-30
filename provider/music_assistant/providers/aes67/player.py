"""AES67 output player."""

from __future__ import annotations

import time
from typing import TYPE_CHECKING, cast

from music_assistant_models.config_entries import ConfigEntry
from music_assistant_models.enums import PlaybackState, PlayerFeature
from music_assistant_models.errors import PlayerCommandFailed, PlayerUnavailableError
from music_assistant_models.player import DeviceInfo

from music_assistant.constants import CONF_ENTRY_OUTPUT_CODEC
from music_assistant.models.player import Player, PlayerMedia

from .client import SenderError
from .constants import DOMAIN, POLL_INTERVAL_IDLE, POLL_INTERVAL_PLAYING

if TYPE_CHECKING:
    from collections.abc import Awaitable

    from .client import SenderClient
    from .provider import AES67Provider

# the sender reads WAV: raw PCM with the format in its header, at the stream's rate
CONF_ENTRY_OUTPUT_CODEC_WAV = ConfigEntry.from_dict(
    {**CONF_ENTRY_OUTPUT_CODEC.to_dict(), "default_value": "wav", "hidden": True}
)


class AES67Player(Player):
    """A player whose audio leaves as an AES67 stream."""

    def __init__(self, provider: AES67Provider, player_id: str) -> None:
        """
        Initialize the player.

        :param provider: The provider instance this output belongs to.
        :param player_id: The player (and sender output) id.
        """
        super().__init__(provider, player_id)
        output = provider.output
        self._attr_name = output["name"]
        self._attr_supported_features = {
            PlayerFeature.PLAY_MEDIA,
            PlayerFeature.POWER,
            PlayerFeature.VOLUME_SET,
            PlayerFeature.VOLUME_MUTE,
            PlayerFeature.SET_MEMBERS,
            PlayerFeature.GAPLESS_PLAYBACK,
        }
        rate = int(output["sample_rate"])
        self._attr_supported_sample_rates = [(rate, 16), (rate, 24)]
        session = output["session_name"] or output["name"]
        self._attr_device_info = DeviceInfo(
            model="AES67 output",
            manufacturer="Meros",
            # "<session> @ <address>:<port>": what a receiver subscribes to
            model_id=f"{session} @ {output['address']}:{output['port']}",
        )
        self._attr_needs_poll = True
        self._attr_powered = True
        self._attr_volume_level = 50
        self._attr_volume_muted = False
        self._attr_available = True

    @property
    def _client(self) -> SenderClient:
        return cast("AES67Provider", self.provider).client

    @property
    def can_group_with(self) -> set[str]:
        """Return the other AES67 outputs: one sender plays them all in sync."""
        return {
            player.player_id
            for player in self.mass.players.iter_players()
            if player.provider.domain == DOMAIN and player.player_id != self.player_id
        }

    @property
    def poll_interval(self) -> int:
        """Return how often to read the sender's state."""
        if self._attr_playback_state == PlaybackState.PLAYING:
            return POLL_INTERVAL_PLAYING
        return POLL_INTERVAL_IDLE

    async def get_config_entries(self) -> list[ConfigEntry]:
        """Return the player's config entries."""
        return [CONF_ENTRY_OUTPUT_CODEC_WAV]

    async def play_media(self, media: PlayerMedia) -> None:
        """
        Play media on the output and on the members of its group.

        :param media: The media to play.
        """
        url = await self.mass.streams.resolve_stream_url(self.player_id, media)
        members = [pid for pid in self.group_members if pid != self.player_id]
        await self._command(self._client.play(self.player_id, url, members))
        self._attr_current_media = media
        self._attr_playback_state = PlaybackState.PLAYING
        self._attr_elapsed_time = 0
        self._attr_elapsed_time_last_updated = time.time()
        self.update_state()

    async def play(self) -> None:
        """Play: start the current media again (the queue resumes from its position)."""
        if self._attr_current_media:
            await self.play_media(self._attr_current_media)

    async def stop(self) -> None:
        """Stop the output and its group."""
        await self._command(self._client.stop(self.player_id))
        self._attr_playback_state = PlaybackState.IDLE
        self._attr_current_media = None
        self._attr_active_source = None
        self.update_state()

    async def power(self, powered: bool) -> None:
        """
        Power the output on or off; off stops it.

        :param powered: The new power state.
        """
        if not powered and self._attr_playback_state != PlaybackState.IDLE:
            await self.stop()
        self._attr_powered = powered
        self.update_state()

    async def volume_set(self, volume_level: int) -> None:
        """
        Set the output's volume (a digital gain in the sender).

        :param volume_level: The volume, 0 to 100.
        """
        await self._command(self._client.set_volume(self.player_id, level=volume_level))
        self._attr_volume_level = volume_level
        self.update_state()

    async def volume_mute(self, muted: bool) -> None:
        """
        Mute or unmute the output.

        :param muted: The new mute state.
        """
        await self._command(self._client.set_volume(self.player_id, muted=muted))
        self._attr_volume_muted = muted
        self.update_state()

    async def set_members(
        self,
        player_ids_to_add: list[str] | None = None,
        player_ids_to_remove: list[str] | None = None,
    ) -> None:
        """
        Group other AES67 outputs with this one.

        :param player_ids_to_add: Outputs to add to the group.
        :param player_ids_to_remove: Outputs to remove from the group.
        """
        members = dict.fromkeys(self._attr_group_members)
        for member_id in player_ids_to_add or []:
            members[member_id] = None
        for member_id in player_ids_to_remove or []:
            members.pop(member_id, None)
            if member := self.mass.players.get_player(member_id):
                await member.stop()
        others = [pid for pid in members if pid != self.player_id]
        self._attr_group_members = [self.player_id, *others] if others else []
        self.update_state()
        for member_id in [*(player_ids_to_add or []), *(player_ids_to_remove or [])]:
            if (member := self.mass.players.get_player(member_id)) is not None:
                member.update_state()
        if player_ids_to_add and self._attr_playback_state == PlaybackState.PLAYING:
            # the sender starts a group together; restart so the new members join in sync
            self.mass.create_task(
                self.mass.players.cmd_resume(self.player_id),
                task_id=f"aes67_regroup_resume_{self.player_id}",
                abort_existing=True,
            )

    async def poll(self) -> None:
        """Read the output's state from the sender."""
        try:
            state = await self._client.get_output(self.player_id)
        except SenderError as err:
            if self._attr_available:
                self.logger.warning("AES67 Sender unreachable: %s", err)
            self._attr_available = False
            self.update_state()
            return
        self._attr_available = True
        self._attr_volume_level = state.volume
        self._attr_volume_muted = state.muted
        if state.error:
            self.logger.warning("Playback failed: %s", state.error)
        if state.state in ("playing", "buffering"):
            if state.group_leader is None:
                self._attr_playback_state = PlaybackState.PLAYING
            if state.elapsed is not None:
                self._attr_elapsed_time = state.elapsed
                self._attr_elapsed_time_last_updated = time.time()
        elif self._attr_playback_state == PlaybackState.PLAYING:
            # the stream ended (the queue ran out) or failed
            self._attr_playback_state = PlaybackState.IDLE
        self.update_state()

    async def _command(self, request: Awaitable[None]) -> None:
        try:
            await request
        except SenderError as err:
            if err.status is None:
                self._attr_available = False
                self.update_state()
                raise PlayerUnavailableError(str(err)) from err
            raise PlayerCommandFailed(str(err)) from err
