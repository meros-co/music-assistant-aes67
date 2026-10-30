// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! One Sendspin connection: an output, seen by a Sendspin server (Music
//! Assistant) as a `player@v1` client.
//!
//! The server dials us. After the cleartext init exchange and the Noise
//! handshake, every message is one encrypted binary frame: JSON (type 0),
//! a fragment (1) or an audio chunk (4). Chunks carry the server-clock time
//! their first sample must leave the output; the time filter maps that onto
//! PTP time, and so onto the AES67 media clock.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use super::secure::{
    self, choose_psk, from_b64url, Handshake, Identity, PskKind, Reassembly, Session,
};
use super::timefilter::{measure, TimeFilter};
use crate::engine::Engine;
use crate::log;
use crate::output::{adapt_channels, Output};
use crate::stream::media_at;
use crate::timeline::{MediaTime, WriteResult};
use crate::wav::SampleFormat;

/// What a client needs that outlives a connection.
pub struct ClientCtx {
    pub engine: Arc<Engine>,
    pub output_id: String,
    pub identity: Mutex<Identity>,
    pub manufacturer: String,
    pub product: String,
    pub mac: Option<String>,
    /// How long after its RTP timestamp a sample leaves the AES67 receiver (µs).
    pub receiver_latency_us: i64,
    pub admission: Admission,
    pub shutdown: Arc<AtomicBool>,
}

impl ClientCtx {
    pub fn output(&self) -> Option<Arc<Output>> {
        self.engine.output(&self.output_id)
    }

    fn name(&self) -> String {
        let name = self
            .output()
            .map(|o| o.cfg.name.clone())
            .unwrap_or_else(|| self.output_id.clone());
        truncate(&name, 64)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// One admitted connection per client; others wait their turn by rank.
#[derive(Default)]
pub struct Admission {
    current: Mutex<Option<(u64, u8, Arc<AtomicBool>)>>,
    next_id: AtomicU64,
}

impl Admission {
    fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Admits connection `id` at `rank` (playback 2, pairing 1, none 0).
    /// `preferred` says the server is the last one that held playback.
    fn admit(&self, id: u64, rank: u8, preferred: bool, kick: &Arc<AtomicBool>) -> bool {
        let mut g = self.current.lock().unwrap();
        match g.as_ref() {
            Some((cur, _, _)) if *cur == id => {
                *g = Some((id, rank, kick.clone()));
                true
            }
            Some((_, cur_rank, cur_kick)) => {
                let wins = match (rank, *cur_rank) {
                    (_, 1) => false, // a pairing attempt is never displaced
                    (0, 0) => preferred,
                    (r, c) => r >= c,
                };
                if wins {
                    cur_kick.store(true, Ordering::Relaxed);
                    *g = Some((id, rank, kick.clone()));
                }
                wins
            }
            None => {
                *g = Some((id, rank, kick.clone()));
                true
            }
        }
    }

    fn release(&self, id: u64) {
        let mut g = self.current.lock().unwrap();
        if g.as_ref().is_some_and(|c| c.0 == id) {
            *g = None;
        }
    }
}

/// The two ends a connection talks through, so tests can drive one in memory.
pub trait Wire {
    fn send_text(&mut self, s: &str) -> std::io::Result<()>;
    fn send_binary(&mut self, b: Vec<u8>) -> std::io::Result<()>;
    /// The next message, None after `timeout` without one. Err: closed.
    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<Message>>;
    fn close(&mut self);
}

impl Wire for WebSocket<TcpStream> {
    fn send_text(&mut self, s: &str) -> std::io::Result<()> {
        self.send(Message::text(s)).map_err(std::io::Error::other)
    }

    fn send_binary(&mut self, b: Vec<u8>) -> std::io::Result<()> {
        self.send(Message::binary(b)).map_err(std::io::Error::other)
    }

    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<Message>> {
        self.get_mut()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match self.read() {
            Ok(m) => Ok(Some(m)),
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                let _ = self.flush();
                Ok(None)
            }
            Err(e) => Err(std::io::Error::other(e)),
        }
    }

    fn close(&mut self) {
        let _ = WebSocket::close(self, None);
        let _ = self.flush();
    }
}

#[derive(Debug)]
struct Fail(String);

impl<E: std::fmt::Display> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PcmFormat {
    rate: u32,
    channels: usize,
    format: SampleFormat,
}

pub fn supported_formats(rate: u32) -> Value {
    json!([
        {"codec": "pcm", "channels": 2, "sample_rate": rate, "bit_depth": 24},
        {"codec": "pcm", "channels": 2, "sample_rate": rate, "bit_depth": 16},
        {"codec": "pcm", "channels": 1, "sample_rate": rate, "bit_depth": 24},
        {"codec": "pcm", "channels": 1, "sample_rate": rate, "bit_depth": 16},
    ])
}

/// About two seconds of the largest format we accept, counted the way the server counts.
pub fn buffer_capacity(rate: u32) -> u64 {
    let per_second = rate as u64 * 2 * 3 + 40 * 13;
    per_second * 2
}

const REQUIRED_LEAD_MS: u32 = 150;
const MIN_BUFFER_MS: u32 = 200;
/// How far a chunk may land from where the timeline expects it before it is nudged.
const DEAD_BAND_US: i64 = 100;
/// Beyond this the timeline restarts at the chunk's own time.
const RESYNC_US: i64 = 20_000;

struct Conn<'a, W: Wire> {
    ctx: &'a ClientCtx,
    wire: W,
    session: Session,
    psk_kind: PskKind,
    server_id: String,
    conn_id: u64,
    kick: Arc<AtomicBool>,
    reassembly: Reassembly,
    activated: bool,
    player_active: bool,
    playback: bool,
    hello_sent: bool,
    state_sent: bool,
    filter: TimeFilter,
    clock_epoch: u64,
    burst_left: u32,
    burst_best: Option<(i64, i64, i64)>,
    awaiting_time: Option<i64>,
    next_burst: Instant,
    format: Option<PcmFormat>,
    expected: Option<MediaTime>,
    generation: Option<u64>,
    pairing_index: u32,
    pending_lt: Option<[u8; 32]>,
}

/// Runs a connection until it closes. The server's init and handshake must
/// arrive within 30 s each, and its first activation within 30 s after that.
pub fn run<W: Wire>(ctx: &ClientCtx, mut wire: W) {
    match handshake(ctx, &mut wire) {
        Ok((session, psk_kind, server_id)) => {
            let conn_id = ctx.admission.new_id();
            let mut c = Conn {
                ctx,
                wire,
                session,
                psk_kind,
                server_id,
                conn_id,
                kick: Arc::new(AtomicBool::new(false)),
                reassembly: Reassembly::default(),
                activated: false,
                player_active: false,
                playback: false,
                hello_sent: false,
                state_sent: false,
                filter: TimeFilter::default(),
                clock_epoch: ctx.engine.clock.epoch(),
                burst_left: 0,
                burst_best: None,
                awaiting_time: None,
                next_burst: Instant::now(),
                format: None,
                expected: None,
                generation: None,
                pairing_index: 0,
                pending_lt: None,
            };
            let result = c.serve();
            if let Err(Fail(e)) = &result {
                log::info(&format!(
                    "sendspin {}: connection ended: {e}",
                    ctx.output_id
                ));
            }
            c.end_stream();
            ctx.admission.release(conn_id);
            c.wire.close();
        }
        Err(Fail(e)) => {
            log::debug(&format!(
                "sendspin {}: handshake failed: {e}",
                ctx.output_id
            ));
            wire.close();
        }
    }
}

fn recv_text<W: Wire>(wire: &mut W, timeout: Duration) -> Result<String, Fail> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Fail("timed out".into()));
        }
        match wire.recv(left)? {
            Some(Message::Text(t)) => return Ok(t.to_string()),
            Some(Message::Close(_)) => return Err(Fail("closed".into())),
            Some(Message::Binary(_)) => {
                return Err(Fail("binary frame during the handshake".into()))
            }
            _ => continue,
        }
    }
}

fn handshake<W: Wire>(ctx: &ClientCtx, wire: &mut W) -> Result<(Session, PskKind, String), Fail> {
    let client_id = ctx.identity.lock().unwrap().client_id().to_string();
    let init = json!({"type": "client/init", "payload": {"client_id": client_id, "version": secure::VERSION, "suite": secure::SUITE}}).to_string();
    wire.send_text(&init)?;
    let server_init = recv_text(wire, Duration::from_secs(30))?;
    let si: Value = serde_json::from_str(&server_init)?;
    if si["type"] == "server/error" {
        return Err(Fail(format!("server refused: {}", si["payload"]["reason"])));
    }
    if si["type"] != "server/init" || si["payload"]["version"] != secure::VERSION {
        return Err(Fail("not a version 1 server/init".into()));
    }
    let server_id = si["payload"]["server_id"]
        .as_str()
        .ok_or("no server_id")?
        .to_string();
    let server_static: [u8; 32] = from_b64url(&server_id)
        .and_then(|v| v.try_into().ok())
        .ok_or("server_id is not a key")?;
    let mut prologue = init.into_bytes();
    prologue.extend_from_slice(server_init.as_bytes());
    let msg1 = recv_text(wire, Duration::from_secs(30))?;
    let (session, kind) = {
        let id = ctx.identity.lock().unwrap();
        respond(&id, &server_id, &server_static, &prologue, &msg1, |m2| {
            wire.send_text(
                &json!({"type": "noise/handshake", "payload": {"data": secure::b64url(m2)}})
                    .to_string(),
            )
            .map_err(Fail::from)
        })?
    };
    Ok((session, kind, server_id))
}

/// Answers a `noise/handshake` message 1 (as JSON text) and hands message 2 to `send`.
fn respond(
    id: &Identity,
    server_id: &str,
    server_static: &[u8; 32],
    prologue: &[u8],
    msg1_json: &str,
    send: impl FnOnce(&[u8]) -> Result<(), Fail>,
) -> Result<(Session, PskKind), Fail> {
    let m: Value = serde_json::from_str(msg1_json)?;
    if m["type"] != "noise/handshake" {
        return Err(Fail("expected noise/handshake".into()));
    }
    let data = m["payload"]["data"]
        .as_str()
        .and_then(from_b64url)
        .ok_or("bad handshake data")?;
    let mut hs = Handshake::new(id, server_static, prologue)?;
    let payload: Value = serde_json::from_slice(&hs.read_first(&data)?)?;
    let category = payload["psk_category"].as_str().ok_or("no psk_category")?;
    if !["lt", "pr", "sn"].contains(&category) {
        return Err(Fail("bad psk_category".into()));
    }
    let wanted = payload["psk_id"].as_str().ok_or("no psk_id")?;
    let (psk, kind) =
        choose_psk(id, server_id, wanted, category).ok_or("long-term PSK of another server")?;
    let (m2, session) = hs.write_second(&psk)?;
    send(&m2)?;
    Ok((session, kind))
}

fn client_now_us(engine: &Engine) -> i64 {
    engine.clock.ptp_now() / 1000
}

impl<W: Wire> Conn<'_, W> {
    fn send_json(&mut self, v: Value) -> Result<(), Fail> {
        let mut plain = vec![0u8];
        plain.extend_from_slice(v.to_string().as_bytes());
        let c = self.session.encrypt(&plain)?;
        self.wire.send_binary(c)?;
        Ok(())
    }

    fn goodbye(&mut self, reason: &str) -> Result<(), Fail> {
        let _ = self.send_json(json!({"type": "client/goodbye", "payload": {"reason": reason}}));
        Err(Fail(format!("goodbye: {reason}")))
    }

    fn serve(&mut self) -> Result<(), Fail> {
        let started = Instant::now();
        loop {
            if self.ctx.shutdown.load(Ordering::Relaxed) {
                return self.goodbye("shutdown");
            }
            if self.kick.load(Ordering::Relaxed) {
                return self.goodbye("another_server");
            }
            if !self.activated && started.elapsed() > Duration::from_secs(30) {
                return Err(Fail("no server/activate within 30 s".into()));
            }
            self.tick_clock()?;
            match self.wire.recv(Duration::from_millis(20))? {
                None
                | Some(Message::Ping(_))
                | Some(Message::Pong(_))
                | Some(Message::Frame(_)) => {}
                Some(Message::Close(_)) => return Err(Fail("closed by the server".into())),
                Some(Message::Text(_)) => {
                    return Err(Fail("text frame after the handshake".into()))
                }
                Some(Message::Binary(b)) => {
                    let plain = self.session.decrypt(&b)?;
                    if let Some(msg) = self.reassembly.feed(plain).map_err(|e| Fail(e.into()))? {
                        self.dispatch(msg)?;
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, msg: Vec<u8>) -> Result<(), Fail> {
        match msg[0] {
            0 => {
                let v: Value = serde_json::from_slice(&msg[1..])?;
                let (Some(t), Some(_)) = (v["type"].as_str(), v.get("payload")) else {
                    return Err(Fail("JSON message without type and payload".into()));
                };
                let t = t.to_string();
                self.on_json(&t, &v["payload"])
            }
            4 => {
                if msg.len() < 13 {
                    return Err(Fail("short audio chunk".into()));
                }
                if self.player_active && self.state_sent {
                    let ts = i64::from_be_bytes(msg[1..9].try_into().unwrap());
                    self.on_audio(ts, &msg[13..]);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_json(&mut self, t: &str, p: &Value) -> Result<(), Fail> {
        if !self.activated && !matches!(t, "server/hello" | "server/activate" | "noise/handshake") {
            // Before the first activation the server sends nothing else.
            return Ok(());
        }
        match t {
            "server/hello" => {
                if !self.hello_sent {
                    self.hello_sent = true;
                    self.send_hello()?;
                }
            }
            "server/activate" => self.on_activate(p)?,
            "server/time" => self.on_time(p),
            "stream/start" => {
                if let Some(pl) = p.get("player") {
                    let codec = pl["codec"].as_str().unwrap_or("");
                    let rate = pl["sample_rate"].as_u64().unwrap_or(0) as u32;
                    let channels = pl["channels"].as_u64().unwrap_or(0) as usize;
                    let format = match pl["bit_depth"].as_u64() {
                        Some(16) => Some(SampleFormat::S16),
                        Some(24) => Some(SampleFormat::S24),
                        Some(32) => Some(SampleFormat::S32),
                        _ => None,
                    };
                    let want_rate = self.ctx.output().map(|o| o.cfg.sample_rate).unwrap_or(0);
                    self.format = match (codec, format) {
                        ("pcm", Some(f)) if rate == want_rate && (1..=8).contains(&channels) => {
                            Some(PcmFormat {
                                rate,
                                channels,
                                format: f,
                            })
                        }
                        _ => {
                            log::warn(&format!(
                                "sendspin {}: unexpected stream format {pl}",
                                self.ctx.output_id
                            ));
                            None
                        }
                    };
                }
            }
            "stream/clear" => {
                if roles_include_player(p) {
                    if let Some(o) = self.ctx.output() {
                        o.timeline.clear();
                    }
                    self.expected = None;
                }
            }
            "stream/end" => {
                if roles_include_player(p) {
                    self.end_stream();
                }
            }
            "server/command" => self.on_command(&p["player"])?,
            "server/unpair" => {
                if self.psk_kind == PskKind::LongTerm {
                    let mut id = self.ctx.identity.lock().unwrap();
                    id.remove_record(&self.server_id);
                    let _ = id.save();
                    drop(id);
                    return self.goodbye("unpaired");
                }
            }
            "noise/handshake" => self.rehandshake(p)?,
            "server/pair-finalize" => {
                if let Some(psk) = self.pending_lt.take() {
                    let mut id = self.ctx.identity.lock().unwrap();
                    id.add_record(&self.server_id, psk, unix_now());
                    let _ = id.save();
                    log::info(&format!(
                        "sendspin {}: paired with {}",
                        self.ctx.output_id, self.server_id
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn send_hello(&mut self) -> Result<(), Fail> {
        let rate = self
            .ctx
            .output()
            .map(|o| o.cfg.sample_rate)
            .unwrap_or(48000);
        // Music Assistant shows product_name as the player's model: name the stream there,
        // "<session> @ <address>:<port>", the form controllers read it in.
        let product = self
            .ctx
            .output()
            .map(|o| format!("{} @ {}:{}", o.cfg.session(), o.cfg.address, o.cfg.port))
            .unwrap_or_else(|| self.ctx.product.clone());
        let mut device = json!({
            "product_name": truncate(&product, 64),
            "manufacturer": truncate(&self.ctx.manufacturer, 64),
            "software_version": env!("CARGO_PKG_VERSION"),
        });
        if let Some(mac) = &self.ctx.mac {
            device["mac_address"] = json!(mac.to_lowercase().replace('-', ":"));
        }
        let name = self.ctx.name();
        self.send_json(json!({"type": "client/hello", "payload": {
            "name": name,
            "device_info": device,
            "supported_roles": ["player@v1"],
            "player@v1_support": {"supported_formats": supported_formats(rate), "buffer_capacity": buffer_capacity(rate)},
            "supported_pair_methods": {"pairing_psk": {"locations": ["operator"]}},
            "unpaired_access": {"enabled": true},
        }}))
    }

    fn on_activate(&mut self, p: &Value) -> Result<(), Fail> {
        let activities: Vec<&str> = p["activities"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let playback = activities.contains(&"playback");
        let pairing = activities.contains(&"pairing");
        let allowed = match self.psk_kind {
            PskKind::LongTerm => !pairing,
            // Unpaired access is on, so playback is allowed here too.
            PskKind::Pairing | PskKind::Sentinel => true,
        };
        if !allowed
            || activities
                .iter()
                .any(|a| !["playback", "pairing"].contains(a))
        {
            return self.goodbye("unauthorized");
        }
        let roles: Option<Vec<String>> =
            p.get("active_roles").and_then(|r| r.as_array()).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
        let roles = if self.activated {
            roles
        } else {
            Some(roles.unwrap_or_default())
        };
        let rank = if playback {
            2
        } else if pairing {
            1
        } else {
            0
        };
        let preferred = self
            .ctx
            .identity
            .lock()
            .unwrap()
            .last_playback_server
            .as_deref()
            == Some(self.server_id.as_str());
        if !self
            .ctx
            .admission
            .admit(self.conn_id, rank, preferred, &self.kick)
        {
            if pairing {
                let _ = self.send_json(
                    json!({"type": "pair/abort", "payload": {"reason": "concurrent_attempt"}}),
                );
            }
            return self.goodbye("concurrent_attempt");
        }
        let first = !self.activated;
        self.activated = true;
        if playback && !self.playback {
            let mut id = self.ctx.identity.lock().unwrap();
            if id.last_playback_server.as_deref() != Some(&self.server_id) {
                id.last_playback_server = Some(self.server_id.clone());
                let _ = id.save();
            }
        }
        self.playback = playback;
        if let Some(roles) = roles {
            let active = roles.iter().any(|r| r == "player@v1");
            if active && (!self.player_active || first) {
                self.state_sent = false;
                self.start_burst();
            }
            if !active && self.player_active {
                self.end_stream();
            }
            self.player_active = active;
        }
        if self.player_active && self.state_sent {
            // A re-activation: report again.
            self.send_state()?;
        }
        if pairing {
            self.pairing_index += 1;
            let method = p["pairing"]["method"].as_str().unwrap_or("");
            if method != "pairing_psk" || self.psk_kind != PskKind::Pairing {
                return self.send_json(
                    json!({"type": "pair/abort", "payload": {"reason": "method_not_supported"}}),
                );
            }
            let lt = secure::random32();
            self.pending_lt = Some(lt);
            self.send_json(json!({"type": "client/pair-init", "payload": {"pairing_index": self.pairing_index}}))?;
            self.send_json(json!({"type": "client/pair-finalize", "payload": {"long_term_psk": secure::b64url(&lt)}}))?;
        } else {
            self.pending_lt = None;
        }
        Ok(())
    }

    /// An in-band re-handshake (to the pairing PSK, or to a fresh long-term one).
    fn rehandshake(&mut self, p: &Value) -> Result<(), Fail> {
        let server_static: [u8; 32] = from_b64url(&self.server_id)
            .and_then(|v| v.try_into().ok())
            .ok_or("server_id is not a key")?;
        let msg1 = json!({"type": "noise/handshake", "payload": p}).to_string();
        let prologue = self.session.hash.clone();
        let mut m2_out = None;
        let (session, kind) = {
            let id = self.ctx.identity.lock().unwrap();
            respond(
                &id,
                &self.server_id,
                &server_static,
                &prologue,
                &msg1,
                |m2| {
                    m2_out = Some(m2.to_vec());
                    Ok(())
                },
            )?
        };
        let m2 = m2_out.unwrap();
        // Message 2 still travels under the old keys.
        self.send_json(
            json!({"type": "noise/handshake", "payload": {"data": secure::b64url(&m2)}}),
        )?;
        self.session = session;
        self.psk_kind = kind;
        self.pairing_index = 0;
        self.state_sent = self.state_sent && self.player_active;
        Ok(())
    }

    fn on_command(&mut self, cmd: &Value) -> Result<(), Fail> {
        let Some(o) = self.ctx.output() else {
            return Ok(());
        };
        match cmd["command"].as_str() {
            Some("volume") => {
                if let Some(v) = cmd["volume"].as_u64() {
                    o.set_volume(v.min(100) as u8);
                }
            }
            Some("mute") => {
                if let Some(m) = cmd["mute"].as_bool() {
                    o.set_muted(m);
                }
            }
            Some("set_output_delay") => {
                if let Some(d) = cmd["output_delay_ms"].as_u64() {
                    self.ctx.identity.lock().unwrap().output_delay_ms = d.min(5000) as u32;
                    self.expected = None;
                }
            }
            _ => return Ok(()),
        }
        {
            let mut id = self.ctx.identity.lock().unwrap();
            id.volume = o.volume();
            id.muted = o.muted();
            let _ = id.save();
        }
        if self.state_sent {
            self.send_state()?;
        }
        Ok(())
    }

    fn send_state(&mut self) -> Result<(), Fail> {
        let (volume, muted) = self
            .ctx
            .output()
            .map(|o| (o.volume(), o.muted()))
            .unwrap_or((0, false));
        let delay = self.ctx.identity.lock().unwrap().output_delay_ms;
        self.send_json(json!({"type": "client/state", "payload": {
            "available": true,
            "player": {
                "volume": volume,
                "muted": muted,
                "output_delay_ms": delay,
                "required_lead_time_ms": REQUIRED_LEAD_MS,
                "min_buffer_ms": MIN_BUFFER_MS,
                "supported_commands": ["volume", "mute", "set_output_delay"],
            },
        }}))?;
        self.state_sent = true;
        Ok(())
    }

    fn start_burst(&mut self) {
        self.burst_left = 8;
        self.burst_best = None;
        self.awaiting_time = None;
        self.next_burst = Instant::now();
    }

    fn tick_clock(&mut self) -> Result<(), Fail> {
        if !self.player_active {
            return Ok(());
        }
        let epoch = self.ctx.engine.clock.epoch();
        if epoch != self.clock_epoch {
            // Our time base stepped: what the filter learned no longer holds.
            self.clock_epoch = epoch;
            self.filter.reset();
            self.expected = None;
            self.start_burst();
        }
        if self
            .awaiting_time
            .is_some_and(|t| client_now_us(&self.ctx.engine) - t > 2_000_000)
        {
            self.awaiting_time = None;
        }
        if self.awaiting_time.is_none() && self.burst_left == 0 && Instant::now() >= self.next_burst
        {
            self.burst_left = 8;
            self.burst_best = None;
        }
        if self.awaiting_time.is_none() && self.burst_left > 0 {
            let t1 = client_now_us(&self.ctx.engine);
            self.awaiting_time = Some(t1);
            self.send_json(json!({"type": "client/time", "payload": {"client_transmitted": t1}}))?;
        }
        if !self.state_sent && self.filter.count() >= 2 && self.filter.error_us() < 1000.0 {
            self.send_state()?;
        }
        Ok(())
    }

    fn on_time(&mut self, p: &Value) {
        let t4 = client_now_us(&self.ctx.engine);
        let (Some(t1), Some(t2), Some(t3)) = (
            p["client_transmitted"].as_i64(),
            p["server_received"].as_i64(),
            p["server_transmitted"].as_i64(),
        ) else {
            return;
        };
        if self.awaiting_time != Some(t1) {
            return;
        }
        self.awaiting_time = None;
        let (m, e) = measure(t1, t2, t3, t4);
        if self.burst_best.is_none_or(|b| e < b.1) {
            self.burst_best = Some((m, e, t4));
        }
        self.burst_left = self.burst_left.saturating_sub(1);
        if self.burst_left == 0 {
            if let Some((m, e, t)) = self.burst_best.take() {
                self.filter.update(m, e, t);
            }
            let wait = if self.filter.count() < 4 || self.filter.error_us() > 1000.0 {
                200
            } else {
                3000
            };
            self.next_burst = Instant::now() + Duration::from_millis(wait);
        }
    }

    fn end_stream(&mut self) {
        if let (Some(o), Some(g)) = (self.ctx.output(), self.generation.take()) {
            if o.generation.load(Ordering::SeqCst) == g {
                o.stop();
            }
        }
        self.expected = None;
    }

    fn on_audio(&mut self, server_ts: i64, payload: &[u8]) {
        let (Some(o), Some(f)) = (self.ctx.output(), self.format) else {
            return;
        };
        let delay_us = self.ctx.identity.lock().unwrap().output_delay_ms as i64 * 1000;
        let play_us = self.filter.client_time(server_ts) - delay_us - self.ctx.receiver_latency_us;
        let target = media_at(play_us * 1000, f.rate);
        let mut samples = Vec::with_capacity(payload.len() / f.format.bytes());
        let frame_bytes = f.channels * f.format.bytes();
        f.format.decode(
            &payload[..payload.len() / frame_bytes * frame_bytes],
            &mut samples,
        );
        let mut adapted = Vec::new();
        adapt_channels(&samples, f.channels, o.cfg.source_channels(), &mut adapted);
        let ch = o.cfg.source_channels();

        // Keep one continuous timeline; nudge it a frame per chunk toward
        // where the server wants it, and jump only when far off.
        let at = match self.expected {
            Some(e) => {
                let diff_us = (target as i64 - e as i64) * 1_000_000 / f.rate as i64;
                if diff_us.abs() > RESYNC_US {
                    target
                } else if diff_us > DEAD_BAND_US && !adapted.is_empty() {
                    // We run early: repeat the first frame.
                    let first: Vec<f32> = adapted[..ch].to_vec();
                    adapted.splice(0..0, first);
                    e
                } else if diff_us < -DEAD_BAND_US && adapted.len() > ch {
                    // We run late: drop the first frame.
                    adapted.drain(..ch);
                    e
                } else {
                    e
                }
            }
            None => target,
        };
        let frames = (adapted.len() / ch) as u64;
        if self
            .generation
            .is_none_or(|g| o.generation.load(Ordering::SeqCst) != g)
        {
            match self
                .ctx
                .engine
                .begin_session(&self.ctx.output_id, &[], at, None)
            {
                Ok((_, outs)) => self.generation = outs.first().map(|(_, g)| *g),
                Err(_) => return,
            }
        }
        if o.timeline.write(at, &adapted, Duration::ZERO) == WriteResult::TimedOut {
            log::debug(&format!(
                "sendspin {}: buffer full, chunk dropped",
                self.ctx.output_id
            ));
        }
        if let Some(s) = o.session.lock().unwrap().as_mut() {
            s.written_until = s.written_until.max(at + frames);
        }
        self.expected = Some(at + frames);
    }
}

fn roles_include_player(p: &Value) -> bool {
    match p.get("roles").and_then(|r| r.as_array()) {
        None => true,
        Some(r) => r.iter().any(|v| v.as_str() == Some("player")),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
