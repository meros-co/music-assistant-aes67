"""AES67 player provider: one instance is one output on an AES67 Sender."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, cast

from music_assistant_models.errors import SetupFailedError

from music_assistant.models.player_provider import PlayerProvider

from .client import SenderClient, SenderError
from .constants import (
    CONF_SENDER_HOST,
    CONF_SENDER_PORT,
    CONF_SENDER_TOKEN,
    DEFAULT_SENDER_HOST,
    DEFAULT_SENDER_PORT,
)
from .helpers import OUTPUT_KEYS, output_config, player_id_for
from .player import AES67Player

if TYPE_CHECKING:
    from music_assistant_models.config_entries import ConfigEntry


class AES67Provider(PlayerProvider):
    """An AES67 output, sent by the AES67 Sender add-on."""

    client: SenderClient
    output: dict[str, Any]
    player_id: str

    async def get_config_entries(self) -> tuple[ConfigEntry, ...]:
        """Return the (options) config entries; the stream is set up in the setup flow."""
        return ()

    async def handle_async_init(self) -> None:
        """Connect to the sender and create (or update) the output on it."""
        token = cast("str | None", self.get_setup_value(CONF_SENDER_TOKEN)) or None
        self.client = SenderClient(
            self.mass.http_session,
            str(self.get_setup_value(CONF_SENDER_HOST) or DEFAULT_SENDER_HOST),
            int(cast("int", self.get_setup_value(CONF_SENDER_PORT) or DEFAULT_SENDER_PORT)),
            token,
        )
        values = {key: self.get_setup_value(key) for key in OUTPUT_KEYS}
        try:
            self.output = output_config(values)
        except ValueError as err:
            raise SetupFailedError(f"Invalid AES67 output: {err}") from err
        self.player_id = player_id_for(self.instance_id)
        try:
            await self.client.put_output(self.player_id, self.output)
        except SenderError as err:
            raise SetupFailedError(f"The AES67 Sender refused the output: {err}") from err

    async def loaded_in_mass(self) -> None:
        """Register the output's player."""
        await super().loaded_in_mass()
        await self.discover_players()

    async def discover_players(self) -> None:
        """Register the one player this instance is."""
        if self.mass.players.get_player(self.player_id):
            return
        await self.mass.players.register(AES67Player(self, self.player_id))

    async def unload(self, is_removed: bool = False) -> None:
        """
        Handle unload/close of the provider.

        :param is_removed: True when the provider was removed: the output is then
            deleted from the sender, which stops its stream.
        """
        for player in self.players:
            await self.mass.players.unregister(player.player_id)
        if is_removed:
            try:
                await self.client.delete_output(self.player_id)
            except SenderError as err:
                self.logger.warning("Could not remove the output from the sender: %s", err)
