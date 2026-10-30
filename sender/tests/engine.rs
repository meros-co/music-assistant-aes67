// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! The engine end to end on loopback: a fake Music Assistant stream server
//! serves a WAV, the engine plays it to a unicast receiver, and the packets
//! are checked for timestamps, channel mapping, gain and the SDP.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes67_sender::clock::Clock;
use aes67_sender::engine::Engine;
use aes67_sender::ffi;
use aes67_sender::output::{OutputConfig, PlayState};
use aes67_sender::stream::{media_at, NetConfig};

fn wav(rate: u32, seconds: f32, left: f32, right: f32) -> Vec<u8> {
    let frames = (rate as f32 * seconds) as usize;
    let mut data = Vec::with_capacity(frames * 6);
    for _ in 0..frames {
        for v in [left, right] {
            let s = (v * 8_388_607.0) as i32;
            data.extend_from_slice(&s.to_le_bytes()[..3]);
        }
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF\xff\xff\xff\xffWAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 6).to_le_bytes());
    v.extend_from_slice(&6u16.to_le_bytes());
    v.extend_from_slice(&24u16.to_le_bytes());
    v.extend_from_slice(b"data\xff\xff\xff\xff");
    v.extend_from_slice(&data);
    v
}

/// Serves `body` once per request at http://127.0.0.1:port/stream.wav.
fn serve(body: Vec<u8>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let _ = req.respond(tiny_http::Response::from_data(body.clone()));
        }
    });
    format!("http://127.0.0.1:{port}/stream.wav")
}

fn receiver() -> (UdpSocket, u16) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let port = s.local_addr().unwrap().port();
    (s, port)
}

fn engine() -> Arc<Engine> {
    Engine::new(
        Arc::new(Clock::new()),
        NetConfig {
            iface_ip: Ipv4Addr::UNSPECIFIED,
            dscp_media: 34,
        },
    )
}

fn config(port: u16, channels: u8, map: Vec<u8>, keep_alive: bool) -> OutputConfig {
    OutputConfig {
        name: "Patio".into(),
        session_name: String::new(),
        address: Ipv4Addr::LOCALHOST,
        port,
        channels,
        sample_rate: 48000,
        ptime_us: 1000,
        payload_type: 97,
        ttl: 15,
        channel_map: map,
        keep_alive,
    }
}

struct Packet {
    h: ffi::RtpHeader,
    samples: Vec<i32>,
}

fn recv(s: &UdpSocket, channels: usize) -> Option<Packet> {
    let mut buf = [0u8; 1500];
    let n = s.recv(&mut buf).ok()?;
    let h = ffi::parse_header(&buf[..n])?;
    let payload = &buf[ffi::RTP_HEADER_BYTES..n];
    assert_eq!(payload.len() % (3 * channels), 0);
    let samples = payload.chunks_exact(3).map(ffi::l24_to_i32).collect();
    Some(Packet { h, samples })
}

#[test]
fn plays_a_wav_into_mapped_channels_on_the_media_clock() {
    let (rx, port) = receiver();
    let e = engine();
    e.upsert_output("patio", config(port, 4, vec![3, 4], false))
        .unwrap();
    e.set_volume("patio", Some(100), None).unwrap();
    // Idle and not kept alive: nothing is sent.
    assert!(recv(&rx, 4).is_none());

    let url = serve(wav(48000, 1.0, 0.5, -0.25));
    e.play_url("patio", &url, &[]).unwrap();

    let mut last: Option<ffi::RtpHeader> = None;
    let mut loud = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && loud < 200 {
        let Some(p) = recv(&rx, 4) else { continue };
        assert_eq!(p.h.payload_type, 97);
        assert_eq!(p.samples.len(), 48 * 4);
        if let Some(l) = last {
            assert_eq!(p.h.sequence, l.sequence.wrapping_add(1));
            assert_eq!(p.h.timestamp, l.timestamp.wrapping_add(48));
            assert_eq!(p.h.ssrc, l.ssrc);
        } else {
            // The first packet is stamped close to now on the media clock.
            let now = media_at(e.clock.ptp_now(), 48000) as u32;
            let age = now.wrapping_sub(p.h.timestamp) as i32;
            assert!(
                (0..48 * 20).contains(&age),
                "first packet {age} frames from now"
            );
        }
        last = Some(p.h);
        let f = &p.samples[47 * 4..48 * 4];
        assert_eq!((f[0], f[1]), (0, 0), "unmapped channels stay silent");
        if f[2] > 4_000_000 {
            assert!((f[2] - 4_194_303).abs() < 4, "left {}", f[2]);
            assert!((f[3] + 2_097_151).abs() < 4, "right {}", f[3]);
            loud += 1;
        }
    }
    assert!(loud >= 200, "only {loud} packets carried audio");
    let st = e.status("patio").unwrap();
    assert_eq!(st.state, PlayState::Playing);
    assert!(st.elapsed.unwrap() > 0.1);

    // The stream runs out: the output goes idle and, not kept alive, falls silent.
    let deadline = Instant::now() + Duration::from_secs(3);
    while e.status("patio").unwrap().state != PlayState::Idle && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(e.status("patio").unwrap().state, PlayState::Idle);
    let elapsed = e.status("patio").unwrap().elapsed.unwrap();
    assert!((elapsed - 1.0).abs() < 0.01, "elapsed {elapsed}");
    e.stop("patio").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    while recv(&rx, 4).is_some() {}
    assert!(recv(&rx, 4).is_none());
    e.shutdown();
}

#[test]
fn volume_and_mute_ramp_and_keep_alive_sends_silence() {
    let (rx, port) = receiver();
    let e = engine();
    e.upsert_output("a", config(port, 2, vec![1, 2], true))
        .unwrap();
    // Kept alive: silence flows while idle.
    let p = recv(&rx, 2).expect("keep-alive packets");
    assert!(p.samples.iter().all(|&s| s == 0));

    e.set_volume("a", Some(50), None).unwrap();
    let url = serve(wav(48000, 2.0, 0.5, 0.5));
    e.play_url("a", &url, &[]).unwrap();
    let want = (0.5 * 8_388_607.0 * 0.031_622_8) as i32;
    let mut seen = false;
    for _ in 0..1000 {
        if let Some(p) = recv(&rx, 2) {
            if (p.samples[0] - want).abs() < 64 {
                seen = true;
                break;
            }
        }
    }
    assert!(seen, "50% is -30 dB");
    e.set_volume("a", None, Some(true)).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    while let Some(p) = recv(&rx, 2) {
        if p.samples.iter().all(|&s| s == 0) {
            e.shutdown();
            return;
        }
    }
    panic!("mute never reached silence");
}

#[test]
fn outputs_share_a_destination_and_groups_play_together() {
    let (rx, port) = receiver();
    let e = engine();
    e.upsert_output("left", config(port, 4, vec![1, 2], false))
        .unwrap();
    // A clashing format or channel is refused, with a reason.
    assert!(e
        .upsert_output("bad", config(port, 2, vec![1, 2], false))
        .unwrap_err()
        .contains("same format"));
    assert!(e
        .upsert_output("bad", config(port, 4, vec![2, 3], false))
        .unwrap_err()
        .contains("already used"));
    e.upsert_output("right", config(port, 4, vec![3, 4], false))
        .unwrap();
    for id in ["left", "right"] {
        e.set_volume(id, Some(100), None).unwrap();
    }
    let sdp = e.sdp_for_output("left").unwrap();
    let parsed = ffi::sdp_parse(&sdp).unwrap();
    assert_eq!(parsed.channels, 4);
    assert_eq!(ffi::c_str(&parsed.connection_ip), "127.0.0.1");
    assert!(
        sdp.contains("c=IN IP4 127.0.0.1\r\n"),
        "unicast c= has no TTL: {sdp}"
    );
    assert!(sdp.contains("s=Patio + Patio"));

    let url = serve(wav(48000, 1.0, 0.5, -0.5));
    e.play_url("left", &url, &["right".to_string()]).unwrap();
    assert_eq!(
        e.status("right").unwrap().group_leader.as_deref(),
        Some("left")
    );
    let mut together = 0;
    for _ in 0..2000 {
        let Some(p) = recv(&rx, 4) else { continue };
        let f = &p.samples[47 * 4..48 * 4];
        if f[0] > 4_000_000 {
            // Both outputs carry the same frame in the same packet: sample-aligned.
            assert_eq!(f[0], f[2]);
            assert_eq!(f[1], f[3]);
            together += 1;
            if together > 100 {
                break;
            }
        }
    }
    assert!(together > 100);
    // Stopping a member stops the group.
    e.stop("right").unwrap();
    assert_eq!(e.status("left").unwrap().state, PlayState::Idle);
    e.shutdown();
}

#[test]
fn a_wrong_rate_or_dead_url_is_an_error_state() {
    let (_rx, port) = receiver();
    let e = engine();
    e.upsert_output("x", config(port, 2, vec![1, 2], false))
        .unwrap();
    e.play_url("x", &serve(wav(44100, 0.1, 0.0, 0.0)), &[])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while e.status("x").unwrap().state != PlayState::Error && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let st = e.status("x").unwrap();
    assert_eq!(st.state, PlayState::Error);
    assert!(st.error.unwrap().contains("44100"));
    e.play_url("x", "http://127.0.0.1:9/none.wav", &[]).unwrap();
    assert_ne!(e.status("x").unwrap().state, PlayState::Playing);
    let deadline = Instant::now() + Duration::from_secs(10);
    while e.status("x").unwrap().state != PlayState::Error && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(e.status("x").unwrap().state, PlayState::Error);
    e.shutdown();
}
