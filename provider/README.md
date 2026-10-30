# The `aes67` Music Assistant player provider

This folder is laid out like the Music Assistant server repository
(`music-assistant/server`): copy `music_assistant/providers/aes67` and
`tests/providers/aes67` into a checkout of it.

- Each instance of the provider is one AES67 output, set up in its setup flow: the
  destination (multicast group or unicast receiver), port, stream channels, channel map,
  sample rate, packet time, TTL, session name, and where the AES67 Sender add-on runs.
- The provider talks to the sender only over its HTTP control API (`client.py`); it carries
  no sender code. The provider is offered to Music Assistant under its Apache-2.0 license.
- Playback uses Music Assistant's stream URL (WAV at the stream's rate), so Music
  Assistant's DSP, volume normalization, crossfade and resampling apply as for any player.

Run the tests from the server checkout:

```sh
pytest tests/providers/aes67
```
