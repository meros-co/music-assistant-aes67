// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! A Sendspin server in miniature drives an output: the Noise handshake,
//! activation, clock sync, PCM chunks, commands and the end of the stream.
//! The AES67 packets must carry the audio at the media time the chunk
//! timestamps name.

use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes67_sender::clock::Clock;
use aes67_sender::engine::Engine;
use aes67_sender::ffi;
use aes67_sender::output::{OutputConfig, PlayState};
use aes67_sender::sendspin::secure::{b64url, from_b64url, noise_params, psk_id, sentinel_psk};
use aes67_sender::sendspin::{Sendspin, SendspinConfig};
use aes67_sender::stream::{media_at, NetConfig};
use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

/// The fake server's clock: PTP microseconds plus a large offset.
const SERVER_OFFSET_US: i64 = -1_234_567_890_123;

fn server_now(clock: &Clock) -> i64 {
    clock.ptp_now() / 1000 + SERVER_OFFSET_US
}

struct FakeServer {
    ws: WebSocket<TcpStream>,
    t: snow::TransportState,
}

impl FakeServer {
    fn send(&mut self, v: Value) {
        let mut plain = vec![0u8];
        plain.extend_from_slice(v.to_string().as_bytes());
        self.send_raw(&plain);
    }

    fn send_raw(&mut self, plain: &[u8]) {
        let mut c = vec![0u8; plain.len() + 16];
        let n = self.t.write_message(plain, &mut c).unwrap();
        self.ws.send(Message::binary(c[..n].to_vec())).unwrap();
    }

    fn recv(&mut self, timeout: Duration) -> Option<Value> {
        self.ws.get_mut().set_read_timeout(Some(timeout)).unwrap();
        loop {
            match self.ws.read() {
                Ok(Message::Binary(b)) => {
                    let mut p = vec![0u8; b.len()];
                    let n = self.t.read_message(&b, &mut p).unwrap();
                    assert_eq!(p[0], 0);
                    return Some(serde_json::from_slice(&p[1..n]).unwrap());
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Reads until a message of `kind`, answering clock requests on the way.
    fn until(&mut self, kind: &str, clock: &Clock) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let Some(m) = self.recv(Duration::from_millis(500)) else {
                continue;
            };
            if m["type"] == "client/time" {
                let now = server_now(clock);
                self.send(json!({"type": "server/time", "payload": {
                    "client_transmitted": m["payload"]["client_transmitted"],
                    "server_received": now, "server_transmitted": now + 5}}));
                continue;
            }
            if m["type"] == kind {
                return m;
            }
        }
        panic!("no {kind}");
    }
}

fn connect(port: u16, id: &str) -> FakeServer {
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let (mut ws, _) =
        tungstenite::client::client(format!("ws://127.0.0.1:{port}/sendspin/{id}"), tcp).unwrap();
    let init = match ws.read().unwrap() {
        Message::Text(t) => t.to_string(),
        m => panic!("{m:?}"),
    };
    let ci: Value = serde_json::from_str(&init).unwrap();
    assert_eq!(ci["type"], "client/init");
    assert_eq!(ci["payload"]["suite"], "25519_ChaChaPoly_SHA256");
    let client_pub = from_b64url(ci["payload"]["client_id"].as_str().unwrap()).unwrap();
    let kp = snow::Builder::new(noise_params())
        .generate_keypair()
        .unwrap();
    let si =
        json!({"type": "server/init", "payload": {"server_id": b64url(&kp.public), "version": 1}})
            .to_string();
    ws.send(Message::text(si.clone())).unwrap();
    let mut prologue = init.into_bytes();
    prologue.extend_from_slice(si.as_bytes());
    let psk = sentinel_psk();
    let mut hs = snow::Builder::new(noise_params())
        .local_private_key(&kp.private)
        .unwrap()
        .remote_public_key(&client_pub)
        .unwrap()
        .prologue(&prologue)
        .unwrap()
        .psk(2, &psk)
        .unwrap()
        .build_initiator()
        .unwrap();
    let payload = json!({"psk_id": psk_id(&psk), "psk_category": "sn"}).to_string();
    let mut m1 = vec![0u8; 512];
    let n = hs.write_message(payload.as_bytes(), &mut m1).unwrap();
    ws.send(Message::text(
        json!({"type": "noise/handshake", "payload": {"data": b64url(&m1[..n])}}).to_string(),
    ))
    .unwrap();
    let m2: Value = match ws.read().unwrap() {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        m => panic!("{m:?}"),
    };
    let data = from_b64url(m2["payload"]["data"].as_str().unwrap()).unwrap();
    let mut out = vec![0u8; 64];
    let k = hs.read_message(&data, &mut out).unwrap();
    assert_eq!(&out[..k], b"{}");
    FakeServer {
        ws,
        t: hs.into_transport_mode().unwrap(),
    }
}

#[test]
fn music_assistant_plays_through_sendspin() {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let rtp_port = rx.local_addr().unwrap().port();
    let clock = Arc::new(Clock::new());
    let engine = Engine::new(
        clock.clone(),
        NetConfig {
            iface_ip: Ipv4Addr::UNSPECIFIED,
            dscp_media: 34,
        },
    );
    let cfg = OutputConfig {
        name: "Lobby".into(),
        session_name: String::new(),
        address: Ipv4Addr::LOCALHOST,
        port: rtp_port,
        channels: 2,
        sample_rate: 48000,
        ptime_us: 1000,
        payload_type: 97,
        ttl: 15,
        channel_map: vec![2, 1],
        keep_alive: false,
    };
    engine.upsert_output("lobby", cfg).unwrap();
    let data_dir = std::env::temp_dir().join(format!("sendspin-it-{}", std::process::id()));
    let latency_us = 3000;
    let ss = Sendspin::new(
        SendspinConfig {
            port: 0,
            data_dir: data_dir.clone(),
            hostname: "test".into(),
            ip: Ipv4Addr::LOCALHOST,
            mac: None,
            receiver_latency_us: latency_us,
        },
        engine.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let ctx = ss.add_output("lobby").unwrap();
    // The stored volume came back (50 by default); run at full scale for the check.
    let listener = ss.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn({
        let ss = ss.clone();
        move || ss.serve(listener)
    });

    // An unknown path, or a browser's Origin, is refused.
    assert!(tungstenite::client::connect(format!("ws://127.0.0.1:{port}/sendspin/nope")).is_err());

    let mut s = connect(port, "lobby");
    s.send(json!({"type": "server/hello", "payload": {"name": "Music Assistant"}}));
    let hello = s.recv(Duration::from_secs(5)).unwrap();
    assert_eq!(hello["type"], "client/hello");
    let p = &hello["payload"];
    assert_eq!(p["name"], "Lobby");
    assert_eq!(
        p["device_info"]["product_name"],
        format!("Lobby @ 127.0.0.1:{rtp_port}")
    );
    assert_eq!(p["supported_roles"], json!(["player@v1"]));
    assert_eq!(p["unpaired_access"]["enabled"], true);
    assert_eq!(
        p["player@v1_support"]["supported_formats"][0],
        json!({"codec": "pcm", "channels": 2, "sample_rate": 48000, "bit_depth": 24})
    );
    s.send(json!({"type": "server/activate", "payload": {"activities": [], "active_roles": ["player@v1"]}}));
    let state = s.until("client/state", &clock);
    assert_eq!(state["payload"]["available"], true);
    assert_eq!(state["payload"]["player"]["volume"], 50);

    s.send(json!({"type": "server/command", "payload": {"player": {"command": "volume", "volume": 100}}}));
    let state = s.until("client/state", &clock);
    assert_eq!(state["payload"]["player"]["volume"], 100);
    assert_eq!(engine.output("lobby").unwrap().volume(), 100);

    s.send(json!({"type": "server/activate", "payload": {"activities": ["playback"]}}));
    s.until("client/state", &clock);
    s.send(json!({"type": "stream/start", "payload": {"server_transmitted": server_now(&clock), "player": {"codec": "pcm", "sample_rate": 48000, "channels": 2, "bit_depth": 24}}}));

    // 1 s of left 0.5 / right -0.25, in 25 ms chunks, starting 300 ms out.
    let first_ts = server_now(&clock) + 300_000;
    let mut chunk = Vec::new();
    for _ in 0..1200 {
        chunk.extend_from_slice(&((0.5 * 8_388_607.0) as i32).to_le_bytes()[..3]);
        chunk.extend_from_slice(&((-0.25 * 8_388_607.0) as i32).to_le_bytes()[..3]);
    }
    for i in 0..40 {
        let mut m = vec![4u8];
        m.extend_from_slice(&(first_ts + i * 25_000).to_be_bytes());
        m.extend_from_slice(&0u32.to_be_bytes());
        m.extend_from_slice(&chunk);
        s.send_raw(&m);
    }

    // The first sample is due at first_ts on the server clock, which is
    // first_ts - offset on PTP, and the RTP timestamp leads it by the receiver latency.
    let want = media_at((first_ts - SERVER_OFFSET_US - latency_us) * 1000, 48000) as u32;
    let mut first_loud: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 1500];
    while Instant::now() < deadline && first_loud.is_none() {
        let Ok(n) = rx.recv(&mut buf) else { continue };
        let h = ffi::parse_header(&buf[..n]).unwrap();
        for f in 0..48 {
            let off = ffi::RTP_HEADER_BYTES + f * 6;
            let ch1 = ffi::l24_to_i32(&buf[off..]);
            let ch2 = ffi::l24_to_i32(&buf[off + 3..]);
            if ch2 != 0 {
                // The map put left on channel 2 and right on channel 1; the ramp starts at 0.
                assert!(ch2 > 0 && ch1 < 0, "ch1 {ch1} ch2 {ch2}");
                first_loud = Some(h.timestamp.wrapping_add(f as u32));
                break;
            }
        }
    }
    let got = first_loud.expect("audio arrived");
    let err = got.wrapping_sub(want) as i32;
    assert!(
        err.abs() <= 5,
        "first sample at {got}, wanted {want} ({err} frames)"
    );
    assert_eq!(engine.status("lobby").unwrap().state, PlayState::Playing);

    s.send(json!({"type": "stream/end", "payload": {"roles": ["player"]}}));
    let deadline = Instant::now() + Duration::from_secs(2);
    while engine.status("lobby").unwrap().state != PlayState::Idle && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(engine.status("lobby").unwrap().state, PlayState::Idle);

    // A second server with nothing to play does not displace the first.
    let mut other = connect(port, "lobby");
    other.send(json!({"type": "server/hello", "payload": {"name": "Other"}}));
    other.recv(Duration::from_secs(5)).unwrap();
    other.send(json!({"type": "server/activate", "payload": {"activities": [], "active_roles": ["player@v1"]}}));
    let bye = other.until("client/goodbye", &clock);
    assert_eq!(bye["payload"]["reason"], "concurrent_attempt");

    // The identity survives: the same client id after a reload.
    let id1 = ctx.identity.lock().unwrap().client_id().to_string();
    let reloaded =
        aes67_sender::sendspin::secure::Identity::load_or_create(&data_dir.join("lobby.json"))
            .unwrap();
    assert_eq!(reloaded.client_id(), id1);
    assert_eq!(reloaded.volume, 100);
    engine.shutdown();
    let _ = std::fs::remove_dir_all(&data_dir);
}
