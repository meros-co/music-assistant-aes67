"""Client for the AES67 Sender add-on's control API."""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from aiohttp import ClientError, ClientTimeout

if TYPE_CHECKING:
    from aiohttp import ClientSession

_TIMEOUT = ClientTimeout(total=10)


class SenderError(Exception):
    """The sender could not be reached or refused a request."""

    def __init__(self, message: str, status: int | None = None) -> None:
        """
        Initialize the error.

        :param message: What went wrong, as the sender or the transport said it.
        :param status: The HTTP status, when the sender answered.
        """
        super().__init__(message)
        self.status = status


@dataclass
class OutputState:
    """What the sender reports for one output."""

    state: str
    volume: int
    muted: bool
    elapsed: float | None
    error: str | None
    underrun_frames: int
    group_leader: str | None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> OutputState:
        """
        Build the state from the sender's JSON.

        :param data: The body of GET /outputs/{id}.
        """
        return cls(
            state=str(data.get("state", "idle")),
            volume=int(data.get("volume", 0)),
            muted=bool(data.get("muted", False)),
            elapsed=data.get("elapsed"),
            error=data.get("error"),
            underrun_frames=int(data.get("underrun_frames", 0)),
            group_leader=data.get("group_leader"),
        )


class SenderClient:
    """Talks to one AES67 Sender over HTTP."""

    def __init__(self, session: ClientSession, host: str, port: int, token: str | None) -> None:
        """
        Initialize the client.

        :param session: The shared aiohttp session.
        :param host: The sender's host name or address.
        :param port: The sender's control API port.
        :param token: The API token, when the sender has one set.
        """
        self._session = session
        self._base = f"http://{host}:{port}"
        self._headers = {"Authorization": f"Bearer {token}"} if token else {}

    @property
    def base_url(self) -> str:
        """Return the sender's base URL."""
        return self._base

    async def status(self) -> dict[str, Any]:
        """Return the sender's status: version, interface, PTP clock and outputs."""
        return await self._request("GET", "/status")

    async def put_output(self, output_id: str, config: dict[str, Any]) -> dict[str, Any]:
        """
        Create or change an output.

        :param output_id: The output's id (the player id).
        :param config: The output's stream configuration.
        """
        return await self._request("PUT", f"/outputs/{output_id}", config)

    async def delete_output(self, output_id: str) -> None:
        """
        Remove an output.

        :param output_id: The output's id.
        """
        await self._request("DELETE", f"/outputs/{output_id}")

    async def get_output(self, output_id: str) -> OutputState:
        """
        Return an output's state.

        :param output_id: The output's id.
        """
        return OutputState.from_dict(await self._request("GET", f"/outputs/{output_id}"))

    async def play(self, output_id: str, url: str, members: list[str]) -> None:
        """
        Play a stream URL on an output and the members of its group.

        :param output_id: The output (group leader) to play on.
        :param url: Music Assistant's stream URL (WAV).
        :param members: Ids of the other outputs that play the same audio in sync.
        """
        await self._request("POST", f"/outputs/{output_id}/play", {"url": url, "members": members})

    async def stop(self, output_id: str) -> None:
        """
        Stop an output (and its group).

        :param output_id: The output's id.
        """
        await self._request("POST", f"/outputs/{output_id}/stop", {})

    async def set_volume(
        self, output_id: str, level: int | None = None, muted: bool | None = None
    ) -> None:
        """
        Set an output's volume and/or mute.

        :param output_id: The output's id.
        :param level: Volume 0..100, or None to leave it.
        :param muted: Mute state, or None to leave it.
        """
        body: dict[str, Any] = {}
        if level is not None:
            body["level"] = level
        if muted is not None:
            body["muted"] = muted
        await self._request("POST", f"/outputs/{output_id}/volume", body)

    async def _request(
        self, method: str, path: str, body: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        try:
            async with self._session.request(
                method, self._base + path, json=body, headers=self._headers, timeout=_TIMEOUT
            ) as resp:
                data = await resp.json(content_type=None)
                if resp.status >= 400:
                    message = data.get("error") if isinstance(data, dict) else None
                    raise SenderError(message or f"HTTP {resp.status}", resp.status)
                return data if isinstance(data, dict) else {}
        except (ClientError, TimeoutError, ValueError) as err:
            raise SenderError(f"AES67 Sender at {self._base}: {err}") from err
