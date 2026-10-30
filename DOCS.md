# AES67 Sender

Plays Music Assistant to AES67 networks. Each output you set up here is a speaker in Music
Assistant, and so a `media_player` in Home Assistant. Its audio leaves as a PTP-timed AES67
stream that Q-SYS, Dante devices in AES67 mode, RAVENNA devices and other AES67 receivers
can subscribe to.

## How it works

- **Music Assistant finds the outputs by itself.** Each output is offered to Music Assistant
  as a [Sendspin](https://sendspin-audio.com) player, the protocol Music Assistant uses for
  its own players. Nothing has to be installed in Music Assistant. The outputs appear under
  Settings → Players a few seconds after the add-on starts.
- **The streams are timed by PTP.** The add-on runs `ptp4l` (linuxptp) to follow the
  network's PTP grandmaster, which is usually the Q-SYS Core or a Dante device. ptp4l only
  measures; it never changes the Home Assistant clock. The RTP timestamps follow the
  grandmaster's media clock, so receivers play the stream without drift.
- **Receivers find the streams.** Every stream is announced over SAP (the way most AES67
  devices discover streams) and registered with an NMOS registry when the network has one
  (IS-04, with IS-05 connection management).

## Outputs

Add one entry under **Outputs** for each speaker:

| Option | Meaning | Default |
| --- | --- | --- |
| `name` | The speaker's name in Music Assistant. | required |
| `address` | Where the stream goes: a multicast group (239.x.x.x is usual) or one receiver's IPv4 address. | required |
| `port` | The UDP port. | 5004 |
| `channels` | Channels in the AES67 stream. | 2 |
| `channel_map` | Which stream channels this speaker plays on. `1,2` sends left to 1 and right to 2; one number, such as `5`, sends a mono downmix there. | `1,2` |
| `sample_rate` | 44100, 48000 or 96000. AES67 receivers expect 48000. | 48000 |
| `ptime_us` | Packet time in microseconds. 1000 (1 ms) is the AES67 default that every receiver accepts. | 1000 |
| `ttl` | Multicast TTL. | 15 |
| `session_name` | The stream's name in SAP and NMOS. | the name |
| `keep_alive` | Send silence while nothing plays, so receivers stay subscribed. | true |
| `id` | A fixed id for the speaker. Without it, the id comes from the name, and renaming the speaker makes a new player in Music Assistant. | from the name |

Several speakers can share one stream: give them the same `address` and `port`, the same
`channels`, `sample_rate` and `ptime_us`, and different `channel_map` entries. For example,
an 8-channel stream to `239.69.1.10` can carry four stereo zones on channels 1–2, 3–4, 5–6
and 7–8. A receiver subscribes to the stream once and picks the channels it needs.

Example:

```yaml
outputs:
  - name: Lobby
    address: 239.69.1.10
    channels: 4
    channel_map: "1,2"
  - name: Patio
    address: 239.69.1.10
    channels: 4
    channel_map: "3,4"
  - name: Office
    address: 192.168.1.60
```

## Network options

| Option | Meaning |
| --- | --- |
| `interface` | The network interface for AES67 and PTP, such as `eth0`. Empty picks the first interface with an IPv4 address. |
| `ptp_role` | `follower` follows the network's grandmaster and never becomes one. `leader_if_none` also becomes the grandmaster when the network has none. `none` uses no PTP at all (the host clock); use it only when the receivers don't need PTP. |
| `ptp_domain` | The PTP domain. AES67 uses 0 unless the network is set up otherwise. |
| `receiver_latency_ms` | How long after its timestamp your receivers play a sample (their link offset or receive latency). It lines AES67 speakers up with other Music Assistant players in a group. |
| `dscp_media`, `dscp_ptp` | DSCP marks for audio (AES67 recommends 34) and PTP (46). |
| `sendspin`, `sendspin_port` | Offer the outputs to Music Assistant as Sendspin players (port 8928). |
| `api_port`, `api_token`, `api_bind` | The control API (port 8890), for the Music Assistant AES67 player provider. It listens on 127.0.0.1 (this host) unless `api_bind` is 0.0.0.0; set a token before opening it to the network. |
| `nmos`, `nmos_port` | NMOS IS-04/IS-05 (port 8891). |

## On the receiving side

- **Q-SYS:** add an AES67 receiver (or use the Core's stream discovery) and pick the stream
  by its session name. The Core is normally the PTP grandmaster; keep `ptp_role` at
  `follower`.
- **Dante (AES67 mode):** turn AES67 on in Dante Controller for the device and the
  multicast range. Dante Controller lists SAP-announced AES67 streams for subscription.
- **Streams in different domains or on different grandmasters drift.** The SDP of each
  stream names the grandmaster it is timed against (`a=ts-refclk`). The add-on's log and its
  `/status` page show which grandmaster it follows.

## Status

`http://<home-assistant>:8890/status` shows the PTP state (locked, grandmaster, jitter) and
every output's state, buffer underruns and packet count. `/outputs/<id>/sdp` returns a
stream's SDP, for receivers that need it pasted in.

## Pairing

Music Assistant connects to the outputs without pairing. Sendspin also supports pairing,
which authenticates the connection; each output's pairing token is in the add-on log
(`pairing token SP:…`).
