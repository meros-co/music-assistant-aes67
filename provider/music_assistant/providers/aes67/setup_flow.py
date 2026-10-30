"""Setup flow for the AES67 provider: one form with the sender and the stream."""

from __future__ import annotations

from dataclasses import replace
from typing import TYPE_CHECKING

from music_assistant_models.config_entries import ConfigEntry, ConfigValueOption
from music_assistant_models.enums import ConfigEntryType

from music_assistant.models.setup_flow import SetupFlowError

from .client import SenderClient, SenderError
from .constants import (
    CONF_ADDRESS,
    CONF_CHANNEL_MAP,
    CONF_CHANNELS,
    CONF_KEEP_ALIVE,
    CONF_OUTPUT_NAME,
    CONF_PTIME,
    CONF_RTP_PORT,
    CONF_SAMPLE_RATE,
    CONF_SENDER_HOST,
    CONF_SENDER_PORT,
    CONF_SENDER_TOKEN,
    CONF_SESSION_NAME,
    CONF_TTL,
    DEFAULT_CHANNEL_MAP,
    DEFAULT_CHANNELS,
    DEFAULT_PTIME,
    DEFAULT_RTP_PORT,
    DEFAULT_SAMPLE_RATE,
    DEFAULT_SENDER_HOST,
    DEFAULT_SENDER_PORT,
    DEFAULT_TTL,
    PTIMES_US,
    SAMPLE_RATES,
)
from .helpers import output_config

if TYPE_CHECKING:
    from music_assistant.models.setup_flow import SetupSession

ENTRIES = (
    ConfigEntry(key=CONF_OUTPUT_NAME, type=ConfigEntryType.STRING, required=True),
    ConfigEntry(key=CONF_ADDRESS, type=ConfigEntryType.STRING, required=True),
    ConfigEntry(
        key=CONF_RTP_PORT,
        type=ConfigEntryType.INTEGER,
        default_value=DEFAULT_RTP_PORT,
        range=(1, 65535),
    ),
    ConfigEntry(
        key=CONF_CHANNELS,
        type=ConfigEntryType.INTEGER,
        default_value=DEFAULT_CHANNELS,
        range=(1, 64),
    ),
    ConfigEntry(
        key=CONF_CHANNEL_MAP, type=ConfigEntryType.STRING, default_value=DEFAULT_CHANNEL_MAP
    ),
    ConfigEntry(
        key=CONF_SAMPLE_RATE,
        type=ConfigEntryType.INTEGER,
        default_value=DEFAULT_SAMPLE_RATE,
        options=[ConfigValueOption(rate, f"{rate / 1000:g} kHz") for rate in SAMPLE_RATES],
    ),
    ConfigEntry(
        key=CONF_PTIME,
        type=ConfigEntryType.INTEGER,
        default_value=DEFAULT_PTIME,
        options=[ConfigValueOption(ptime, f"{ptime} µs") for ptime in PTIMES_US],
        advanced=True,
    ),
    ConfigEntry(
        key=CONF_TTL,
        type=ConfigEntryType.INTEGER,
        default_value=DEFAULT_TTL,
        range=(1, 255),
        advanced=True,
    ),
    ConfigEntry(key=CONF_SESSION_NAME, type=ConfigEntryType.STRING, required=False, advanced=True),
    ConfigEntry(
        key=CONF_KEEP_ALIVE, type=ConfigEntryType.BOOLEAN, default_value=True, advanced=True
    ),
    ConfigEntry(
        key=CONF_SENDER_HOST, type=ConfigEntryType.STRING, default_value=DEFAULT_SENDER_HOST
    ),
    ConfigEntry(
        key=CONF_SENDER_PORT,
        type=ConfigEntryType.INTEGER,
        default_value=DEFAULT_SENDER_PORT,
        range=(1, 65535),
    ),
    ConfigEntry(
        key=CONF_SENDER_TOKEN, type=ConfigEntryType.SECURE_STRING, required=False, advanced=True
    ),
)


async def run_setup(session: SetupSession) -> None:
    """Run the setup flow: collect the output's stream and its sender, then create it."""
    errors: dict[str, str | SetupFlowError] | None = None
    setup_data = dict(session.context.setup_data)
    while True:
        entries = [
            replace(entry, value=setup_data.get(entry.key, entry.default_value))
            for entry in ENTRIES
        ]
        submitted = await session.form(entries, step_id="user", errors=errors, last_step=True)
        setup_data.update(submitted)
        try:
            output_config(setup_data)
        except ValueError as err:
            errors = {"base": SetupFlowError(str(err), translation_key="invalid_output")}
            continue
        client = SenderClient(
            session.mass.http_session,
            str(setup_data.get(CONF_SENDER_HOST) or DEFAULT_SENDER_HOST),
            int(setup_data.get(CONF_SENDER_PORT) or DEFAULT_SENDER_PORT),
            str(setup_data.get(CONF_SENDER_TOKEN) or "") or None,
        )
        try:
            await client.status()
        except SenderError as err:
            key = "invalid_token" if err.status == 401 else "cannot_connect"
            errors = {CONF_SENDER_HOST: key}
            continue
        try:
            await session.finish(setup_data)
            return
        except SetupFlowError as err:
            errors = {"base": err}
