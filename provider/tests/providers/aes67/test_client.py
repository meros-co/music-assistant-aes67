"""Tests for the AES67 Sender client, against a fake sender."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

import pytest
from aiohttp import ClientSession, web
from aiohttp.test_utils import TestServer

from music_assistant.providers.aes67.client import SenderClient, SenderError

if TYPE_CHECKING:
    from collections.abc import AsyncIterator


class FakeSender:
    """Records what it is sent and answers like the add-on."""

    def __init__(self, token: str | None = None) -> None:
        """Initialize with an optional required token."""
        self.token = token
        self.calls: list[tuple[str, str, Any]] = []
        self.outputs: dict[str, dict[str, Any]] = {}

    def app(self) -> web.Application:
        """Build the web application."""
        app = web.Application(middlewares=[self._auth])
        app.router.add_get("/status", self._status)
        app.router.add_put("/outputs/{id}", self._put)
        app.router.add_delete("/outputs/{id}", self._delete)
        app.router.add_get("/outputs/{id}", self._get)
        app.router.add_post("/outputs/{id}/{action}", self._action)
        return app

    @web.middleware
    async def _auth(self, request: web.Request, handler: Any) -> web.StreamResponse:
        if self.token and request.headers.get("Authorization") != f"Bearer {self.token}":
            return web.json_response({"error": "missing or wrong token"}, status=401)
        return await handler(request)  # type: ignore[no-any-return]

    async def _status(self, request: web.Request) -> web.Response:
        return web.json_response({"api_version": 1, "outputs": list(self.outputs)})

    async def _put(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.calls.append(("PUT", request.match_info["id"], body))
        if body["channel_map"] == [9]:
            return web.json_response({"error": "channel 9 is already used"}, status=422)
        self.outputs[request.match_info["id"]] = body
        return web.json_response({"id": request.match_info["id"], "state": "idle"})

    async def _delete(self, request: web.Request) -> web.Response:
        self.calls.append(("DELETE", request.match_info["id"], None))
        if self.outputs.pop(request.match_info["id"], None) is None:
            return web.json_response({"error": "no output"}, status=404)
        return web.json_response({})

    async def _get(self, request: web.Request) -> web.Response:
        return web.json_response(
            {
                "id": request.match_info["id"],
                "state": "playing",
                "volume": 40,
                "muted": True,
                "elapsed": 12.5,
                "error": None,
                "underrun_frames": 3,
                "group_leader": "aes67_leader",
            }
        )

    async def _action(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.calls.append((request.match_info["action"], request.match_info["id"], body))
        return web.json_response({"session": 1} if request.match_info["action"] == "play" else {})


@pytest.fixture
async def sender() -> AsyncIterator[tuple[FakeSender, TestServer]]:
    """Run a fake sender."""
    fake = FakeSender()
    server = TestServer(fake.app())
    await server.start_server()
    yield fake, server
    await server.close()


async def test_round_trip(sender: tuple[FakeSender, TestServer]) -> None:
    """Every call reaches the sender in the shape it expects."""
    fake, server = sender
    async with ClientSession() as session:
        client = SenderClient(session, "127.0.0.1", server.port or 0, None)
        assert (await client.status())["api_version"] == 1
        await client.put_output("aes67_1", {"name": "Lobby", "channel_map": [1, 2]})
        await client.play("aes67_1", "http://ma/flow.wav", ["aes67_2"])
        await client.set_volume("aes67_1", level=30)
        await client.set_volume("aes67_1", muted=True)
        await client.stop("aes67_1")
        state = await client.get_output("aes67_1")
        await client.delete_output("aes67_1")
    assert fake.calls == [
        ("PUT", "aes67_1", {"name": "Lobby", "channel_map": [1, 2]}),
        ("play", "aes67_1", {"url": "http://ma/flow.wav", "members": ["aes67_2"]}),
        ("volume", "aes67_1", {"level": 30}),
        ("volume", "aes67_1", {"muted": True}),
        ("stop", "aes67_1", {}),
        ("DELETE", "aes67_1", None),
    ]
    assert state.state == "playing"
    assert (state.volume, state.muted, state.elapsed) == (40, True, 12.5)
    assert state.group_leader == "aes67_leader"


async def test_errors_carry_the_senders_reason(sender: tuple[FakeSender, TestServer]) -> None:
    """A refusal says why; an unreachable sender has no status."""
    _, server = sender
    async with ClientSession() as session:
        client = SenderClient(session, "127.0.0.1", server.port or 0, None)
        with pytest.raises(SenderError, match="already used") as refused:
            await client.put_output("x", {"channel_map": [9]})
        assert refused.value.status == 422
        with pytest.raises(SenderError) as missing:
            await client.delete_output("nope")
        assert missing.value.status == 404
        dead = SenderClient(session, "127.0.0.1", 9, None)
        with pytest.raises(SenderError) as unreachable:
            await dead.status()
        assert unreachable.value.status is None


async def test_token() -> None:
    """The token is sent as a bearer token."""
    fake = FakeSender(token="s3cret")
    server = TestServer(fake.app())
    await server.start_server()
    try:
        async with ClientSession() as session:
            with pytest.raises(SenderError) as err:
                await SenderClient(session, "127.0.0.1", server.port or 0, None).status()
            assert err.value.status == 401
            await SenderClient(session, "127.0.0.1", server.port or 0, "s3cret").status()
    finally:
        await server.close()
