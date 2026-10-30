// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! The outputs and the streams they make up.
//!
//! Outputs naming the same destination share one stream; each takes the
//! stream channels its map names. Playback sources (an HTTP stream from Music
//! Assistant, or a Sendspin connection) write into outputs' timelines.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::clock::{Clock, ClockStatus};
use crate::ffi;
use crate::log;
use crate::output::{adapt_channels, Output, OutputConfig, PlayState, Session};
use crate::sap::Announcer;
use crate::stream::{media_at, NetConfig, Stream, StreamKey};
use crate::timeline::{MediaTime, WriteResult};
use crate::wav::WavReader;

/// How far ahead of now an HTTP stream starts: time to fill the buffer.
pub const HTTP_PREROLL_MS: u64 = 300;

#[derive(Debug, Serialize)]
pub struct OutputStatus {
    pub id: String,
    pub config: OutputConfig,
    pub state: PlayState,
    pub volume: u8,
    pub muted: bool,
    pub elapsed: Option<f64>,
    pub session: Option<u64>,
    pub url: Option<String>,
    pub group_leader: Option<String>,
    pub error: Option<String>,
    pub underrun_frames: u64,
    pub late_frames: u64,
    pub stream: String,
    pub stream_sending: bool,
    pub packets: u64,
}

struct Inner {
    outputs: BTreeMap<String, Arc<Output>>,
    streams: HashMap<StreamKey, Stream>,
}

pub struct Engine {
    pub clock: Arc<Clock>,
    pub net: NetConfig,
    inner: Mutex<Inner>,
    next_session: AtomicU64,
    /// Bumped on every change to outputs or streams (NMOS watches it).
    pub version: AtomicU64,
    stop: Arc<AtomicBool>,
}

impl Engine {
    pub fn new(clock: Arc<Clock>, net: NetConfig) -> Arc<Self> {
        Arc::new(Engine {
            clock,
            net,
            inner: Mutex::new(Inner {
                outputs: BTreeMap::new(),
                streams: HashMap::new(),
            }),
            next_session: AtomicU64::new(1),
            version: AtomicU64::new(1),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Starts the SAP announcer.
    pub fn start_sap(self: &Arc<Self>) -> std::io::Result<()> {
        let mut sap = Announcer::new(&self.net)?;
        let engine = Arc::downgrade(self);
        let stop = self.stop.clone();
        std::thread::Builder::new()
            .name("sap".into())
            .spawn(move || {
                let start = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let Some(e) = engine.upgrade() else { break };
                    let active = e.active_sdps();
                    drop(e);
                    sap.tick(start.elapsed().as_millis() as u64, &active);
                    std::thread::sleep(Duration::from_millis(1000));
                }
                sap.withdraw_all();
            })?;
        Ok(())
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        for o in g.outputs.values() {
            o.stop();
        }
        g.streams.clear();
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn output(&self, id: &str) -> Option<Arc<Output>> {
        self.inner.lock().unwrap().outputs.get(id).cloned()
    }

    pub fn output_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().outputs.keys().cloned().collect()
    }

    /// Adds or changes an output. Refused when it would share a destination
    /// with an output of another format or take a channel already taken.
    pub fn upsert_output(&self, id: &str, cfg: OutputConfig) -> Result<(), String> {
        cfg.validate()?;
        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err("output ids are 1 to 64 letters, digits, '-', '_' or '.'".into());
        }
        let mut g = self.inner.lock().unwrap();
        for (oid, o) in &g.outputs {
            if oid == id || o.cfg.address != cfg.address || o.cfg.port != cfg.port {
                continue;
            }
            if !o.cfg.same_stream_format(&cfg) {
                return Err(format!(
                    "{}:{} is already a {}-channel {} Hz {} us stream (output '{}'); outputs sharing a destination need the same format",
                    cfg.address, cfg.port, o.cfg.channels, o.cfg.sample_rate, o.cfg.ptime_us, o.cfg.name
                ));
            }
            if let Some(c) = cfg
                .channel_map
                .iter()
                .find(|c| o.cfg.channel_map.contains(c))
            {
                return Err(format!(
                    "channel {c} of {}:{} is already used by '{}'",
                    cfg.address, cfg.port, o.cfg.name
                ));
            }
        }
        let (volume, muted) = match g.outputs.get(id) {
            Some(o) if o.cfg == cfg => return Ok(()),
            Some(o) => {
                o.stop();
                (o.volume(), o.muted())
            }
            None => (50, false),
        };
        g.outputs
            .insert(id.to_string(), Output::new(id, cfg, volume, muted));
        self.rebuild(&mut g)?;
        drop(g);
        self.bump();
        Ok(())
    }

    pub fn remove_output(&self, id: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(o) = g.outputs.remove(id) else {
            return false;
        };
        o.stop();
        let _ = self.rebuild(&mut g);
        drop(g);
        self.bump();
        true
    }

    fn rebuild(&self, g: &mut Inner) -> Result<(), String> {
        let mut by_key: BTreeMap<StreamKey, Vec<Arc<Output>>> = BTreeMap::new();
        for o in g.outputs.values() {
            by_key
                .entry(StreamKey {
                    address: o.cfg.address,
                    port: o.cfg.port,
                })
                .or_default()
                .push(o.clone());
        }
        g.streams.retain(|k, s| {
            by_key
                .get(k)
                .is_some_and(|outs| outs[0].cfg.same_stream_format(&s.format))
        });
        for (key, outs) in by_key {
            if let Some(s) = g.streams.get(&key) {
                s.set_outputs(outs);
                continue;
            }
            let format = outs[0].cfg.clone();
            let s = Stream::start(key, format, outs, self.clock.clone(), self.net)
                .map_err(|e| format!("cannot open a socket for {key}: {e}"))?;
            log::info(&format!("stream {key} up"));
            g.streams.insert(key, s);
        }
        Ok(())
    }

    /// The SDP of the stream an output is in.
    pub fn sdp_for_output(&self, id: &str) -> Option<String> {
        let g = self.inner.lock().unwrap();
        let o = g.outputs.get(id)?;
        let s = g.streams.get(&StreamKey {
            address: o.cfg.address,
            port: o.cfg.port,
        })?;
        Some(self.sdp(s))
    }

    fn sdp(&self, s: &Stream) -> String {
        let (gm, domain) = self.clock.grandmaster();
        // A stream shared by several outputs is named after them all.
        let outs = s.outputs.read().unwrap();
        let name = if outs.len() == 1 {
            outs[0].cfg.session().to_string()
        } else {
            outs.iter()
                .map(|o| o.cfg.session())
                .collect::<Vec<_>>()
                .join(" + ")
        };
        ffi::sdp_write(&ffi::SdpInput {
            session_name: &name,
            origin_ip: &self.net.iface_ip.to_string(),
            group: &s.key.address.to_string(),
            port: s.key.port,
            payload_type: s.format.payload_type,
            channels: s.format.channels,
            sample_rate: s.format.sample_rate,
            ptime_us: s.format.ptime_us,
            ssrc: s.ssrc,
            ttl: s.format.ttl,
            gm_identity: gm,
            ptp_domain: domain as u16,
            encoding: s.format.encoding,
            channel_order: if s.format.channels == 2 {
                "SMPTE2110.(ST)"
            } else {
                ""
            },
        })
    }

    /// Streams being sent and their SDP, for SAP.
    pub fn active_sdps(&self) -> BTreeMap<StreamKey, String> {
        let g = self.inner.lock().unwrap();
        g.streams
            .values()
            .filter(|s| s.is_sending())
            .map(|s| (s.key, self.sdp(s)))
            .collect()
    }

    /// Every stream: key, format, outputs, SDP (for NMOS).
    pub fn streams(&self) -> Vec<(StreamKey, OutputConfig, Vec<String>, String, bool)> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<_> = g
            .streams
            .values()
            .map(|s| {
                let ids = s
                    .outputs
                    .read()
                    .unwrap()
                    .iter()
                    .map(|o| o.id.clone())
                    .collect();
                (s.key, s.format.clone(), ids, self.sdp(s), s.is_sending())
            })
            .collect();
        v.sort_by_key(|x| x.0);
        v
    }

    pub fn status(&self, id: &str) -> Option<OutputStatus> {
        let g = self.inner.lock().unwrap();
        let o = g.outputs.get(id)?;
        let key = StreamKey {
            address: o.cfg.address,
            port: o.cfg.port,
        };
        let stream = g.streams.get(&key);
        let s = o.session.lock().unwrap().clone();
        let (_, late) = o.timeline.stats();
        Some(OutputStatus {
            id: id.to_string(),
            config: o.cfg.clone(),
            state: o.state(),
            volume: o.volume(),
            muted: o.muted(),
            elapsed: o.elapsed(),
            session: s.as_ref().map(|s| s.id),
            url: s.as_ref().and_then(|s| s.url.clone()),
            group_leader: s.as_ref().map(|s| s.leader.clone()).filter(|l| l != id),
            error: s.as_ref().and_then(|s| s.error.clone()),
            underrun_frames: o.underrun_frames.load(Ordering::Relaxed),
            late_frames: late,
            stream: key.to_string(),
            stream_sending: stream.is_some_and(|s| s.is_sending()),
            packets: stream
                .map(|s| s.stats.packets.load(Ordering::Relaxed))
                .unwrap_or(0),
        })
    }

    pub fn clock_status(&self) -> ClockStatus {
        self.clock.status()
    }

    pub fn set_volume(
        &self,
        id: &str,
        level: Option<u8>,
        muted: Option<bool>,
    ) -> Result<(), String> {
        let o = self.output(id).ok_or_else(|| format!("no output '{id}'"))?;
        if let Some(l) = level {
            o.set_volume(l);
        }
        if let Some(m) = muted {
            o.set_muted(m);
        }
        Ok(())
    }

    /// Stops an output, and the whole group if it leads or belongs to one.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        let o = self.output(id).ok_or_else(|| format!("no output '{id}'"))?;
        let leader = o.session.lock().unwrap().as_ref().map(|s| s.leader.clone());
        let g = self.inner.lock().unwrap();
        for other in g.outputs.values() {
            let in_group = other.id == id
                || leader.as_ref().is_some_and(|l| {
                    other
                        .session
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|s| &s.leader == l)
                });
            if in_group {
                other.stop();
            }
        }
        Ok(())
    }

    /// Starts a session on `leader` and `members` (stopping what they played)
    /// and returns the outputs and their generations for the source to check.
    pub fn begin_session(
        &self,
        leader: &str,
        members: &[String],
        start: MediaTime,
        url: Option<String>,
    ) -> Result<(u64, Vec<(Arc<Output>, u64)>), String> {
        let mut ids = vec![leader.to_string()];
        for m in members {
            if !ids.contains(m) {
                ids.push(m.clone());
            }
        }
        let outs: Vec<Arc<Output>> = ids
            .iter()
            .map(|i| self.output(i).ok_or_else(|| format!("no output '{i}'")))
            .collect::<Result<_, _>>()?;
        let rate = outs[0].cfg.sample_rate;
        if let Some(o) = outs.iter().find(|o| o.cfg.sample_rate != rate) {
            return Err(format!(
                "'{}' runs at {} Hz and '{}' at {rate} Hz: a group needs one rate",
                o.cfg.name, o.cfg.sample_rate, outs[0].cfg.name
            ));
        }
        for o in &outs {
            self.stop(&o.id)?;
        }
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut v = Vec::new();
        for o in outs {
            *o.session.lock().unwrap() = Some(Session {
                id,
                start,
                written_until: start,
                ended: false,
                error: None,
                url: url.clone(),
                leader: leader.to_string(),
            });
            let gen = o.generation.load(Ordering::SeqCst);
            v.push((o, gen));
        }
        Ok((id, v))
    }

    /// Plays Music Assistant's stream at `url` (WAV) on an output and its group members.
    pub fn play_url(
        self: &Arc<Self>,
        leader: &str,
        url: &str,
        members: &[String],
    ) -> Result<u64, String> {
        let rate = self
            .output(leader)
            .ok_or_else(|| format!("no output '{leader}'"))?
            .cfg
            .sample_rate;
        let start = media_at(self.clock.ptp_now(), rate) + rate as u64 * HTTP_PREROLL_MS / 1000;
        let (session, outs) = self.begin_session(leader, members, start, Some(url.to_string()))?;
        let url = url.to_string();
        let clock = self.clock.clone();
        std::thread::Builder::new()
            .name(format!("http {leader}"))
            .spawn(move || http_source(&url, outs, clock))
            .map_err(|e| e.to_string())?;
        Ok(session)
    }
}

fn still_current(outs: &[(Arc<Output>, u64)]) -> bool {
    outs.iter()
        .any(|(o, g)| o.generation.load(Ordering::SeqCst) == *g)
}

fn set_error(outs: &[(Arc<Output>, u64)], msg: &str) {
    log::error(msg);
    for (o, g) in outs {
        if o.generation.load(Ordering::SeqCst) == *g {
            if let Some(s) = o.session.lock().unwrap().as_mut() {
                s.error = Some(msg.to_string());
            }
        }
    }
}

/// Fetches a WAV stream and lays it down back to back on every output of the session.
fn http_source(url: &str, outs: Vec<(Arc<Output>, u64)>, clock: Arc<Clock>) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build();
    let resp = match agent.get(url).call() {
        Ok(r) => r,
        Err(e) => return set_error(&outs, &format!("cannot fetch {url}: {e}")),
    };
    let mut wav = match WavReader::new(resp.into_reader()) {
        Ok(w) => w,
        Err(e) => return set_error(&outs, &format!("{url}: {e}")),
    };
    let rate = outs[0].0.cfg.sample_rate;
    if wav.format.sample_rate != rate {
        return set_error(
            &outs,
            &format!(
                "{url} is {} Hz; the output runs at {rate} Hz",
                wav.format.sample_rate
            ),
        );
    }
    let channels = wav.format.channels;
    let mut pos: MediaTime = outs[0]
        .0
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.start)
        .unwrap_or(0);
    // If the first data came late, start later rather than lose the beginning.
    let earliest = media_at(clock.ptp_now(), rate) + rate as u64 / 20;
    if pos < earliest {
        pos = earliest;
        for (o, _) in &outs {
            if let Some(s) = o.session.lock().unwrap().as_mut() {
                s.start = pos;
                s.written_until = pos;
            }
        }
    }
    let mut frames = Vec::new();
    let mut adapted = Vec::new();
    loop {
        frames.clear();
        let n = match wav.read_frames(1024, &mut frames) {
            Ok(n) => n,
            Err(e) => return set_error(&outs, &format!("{url}: {e}")),
        };
        if !still_current(&outs) {
            return;
        }
        if n == 0 {
            break;
        }
        for (o, g) in &outs {
            if o.generation.load(Ordering::SeqCst) != *g {
                continue;
            }
            adapt_channels(&frames, channels, o.cfg.source_channels(), &mut adapted);
            loop {
                match o.timeline.write(pos, &adapted, Duration::from_millis(500)) {
                    WriteResult::TimedOut if o.generation.load(Ordering::SeqCst) == *g => continue,
                    _ => break,
                }
            }
            if let Some(s) = o.session.lock().unwrap().as_mut() {
                s.written_until = pos + n as u64;
            }
        }
        pos += n as u64;
    }
    for (o, g) in &outs {
        if o.generation.load(Ordering::SeqCst) == *g {
            if let Some(s) = o.session.lock().unwrap().as_mut() {
                s.ended = true;
            }
        }
    }
}
