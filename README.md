# AES67 for Music Assistant

Lets a Home Assistant user create virtual speakers in Music Assistant that play to AES67
networks: each speaker sends a PTP-timed AES67 stream to a multicast group or a unicast
receiver, announced over SAP and NMOS, so Q-SYS, Dante devices in AES67 mode, RAVENNA
devices and other AES67 receivers can subscribe to it.

It runs entirely in userspace on Home Assistant OS: no kernel modules.

To install it, add `https://github.com/meros-co/music-assistant-aes67` as an add-on
repository in Home Assistant (**Settings → Add-ons → Add-on Store → ⋮ → Repositories**),
then install **AES67 Sender**. [DOCS.md](DOCS.md) describes its options.

## What is here

| Path | What |
| --- | --- |
| `config.yaml`, `Dockerfile`, `DOCS.md` | The Home Assistant add-on (this repository is an add-on repository). |
| `sender/` | The sender, in Rust: PTP-timed RTP L24 transmit, SAP, NMOS IS-04/IS-05, a Sendspin player per output, and a control API. |
| `vendor/aes67` | [meros-co/aes67](https://github.com/meros-co/aes67), the AES67 packet and SDP library (git submodule). |
| `provider/` | A Music Assistant player provider (`aes67`) for the control API, laid out as it goes into the Music Assistant server repository, with its tests. |

## How audio gets out

Music Assistant reaches the sender in either of two ways:

1. **Sendspin (works with stock Music Assistant).** The add-on presents each output as a
   Sendspin `player@v1` client. Music Assistant discovers it over mDNS, connects, and
   sends timestamped PCM; the sender maps Sendspin's clock onto PTP and writes each chunk
   at the media time its timestamp names.
2. **The `aes67` player provider.** Each provider instance is one output, set up in Music
   Assistant's own UI. The provider hands the sender Music Assistant's stream URL (WAV at
   the stream's rate); the sender fetches it and plays it from a fixed start time, and a
   group of outputs starts on the same sample.

Either way, each output has a timeline indexed by the AES67 media clock. One transmit thread
per stream wakes on PTP packet boundaries (`clock_nanosleep` at the packet's due time),
mixes the outputs mapped into the stream with their volume ramps, and sends one L24 packet
stamped with the media time of its first frame (`a=mediaclk:direct=0`).

## PTP

`ptp4l` runs with software timestamping and `free_running 1`: it measures the offset to the
grandmaster but never adjusts a clock, so the add-on needs neither `CAP_SYS_TIME` nor a PTP
hardware clock. The sender reads `TIME_STATUS_NP` and `TIME_PROPERTIES_DATA_SET` over
ptp4l's management socket four times a second, fits phase and rate over a 16-second window,
and converts host time to grandmaster time from that fit. A new grandmaster or a step in
the offset starts a new time base, and the streams restart their timestamps from it.
ptp4l (linuxptp 4.4) is built from source in the image, since Alpine doesn't package it.

## Build and test

```sh
git clone --recurse-submodules https://github.com/meros-co/music-assistant-aes67
cd music-assistant-aes67/sender
cargo test
```

The tests cover the packet and SDP arithmetic, the timeline, the PTP fit and the management
protocol, NMOS, the Sendspin handshake and time filter, and end-to-end playback over
loopback: an HTTP WAV stream, and a Sendspin session driven by a test server, both checked
sample by sample on the received RTP.

The add-on image:

```sh
docker build -t aes67-sender .
```

The provider's tests run inside a checkout of the Music Assistant server with
`provider/music_assistant` and `provider/tests` copied over it:

```sh
pytest tests/providers/aes67
```

## License

Copyright (C) 2026 Meros Inc. The add-on and sender are licensed under the GNU Affero General
Public License, version 3 only (`AGPL-3.0-only`); see [LICENSE](LICENSE). The provider in
`provider/` contains no code from the sender and is offered to Music Assistant under its
Apache-2.0 license.
