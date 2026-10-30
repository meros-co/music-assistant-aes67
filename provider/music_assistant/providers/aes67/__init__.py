"""
AES67 Player Provider for Music Assistant.

Each instance of this provider is one AES67 output: a player whose audio goes out as a
PTP-timed AES67 (RTP L24) stream to a multicast group or a unicast receiver. The stream
is sent by the AES67 Sender add-on, which this provider drives over a small HTTP API.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from music_assistant_models.enums import ProviderFeature

from .provider import AES67Provider

if TYPE_CHECKING:
    from music_assistant_models.config_entries import ProviderConfig
    from music_assistant_models.provider import ProviderManifest

    from music_assistant.mass import MusicAssistant
    from music_assistant.models import ProviderInstanceType

SUPPORTED_FEATURES = {
    ProviderFeature.SYNC_PLAYERS,
}


async def setup(
    mass: MusicAssistant, manifest: ProviderManifest, config: ProviderConfig
) -> ProviderInstanceType:
    """Initialize provider(instance) with given configuration."""
    return AES67Provider(mass, manifest, config, SUPPORTED_FEATURES)
