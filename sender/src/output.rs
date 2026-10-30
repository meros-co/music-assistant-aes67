// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! One virtual output: its stream settings, its audio timeline, its volume,
//! and what it is playing.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ffi;
use crate::timeline::{MediaTime, Timeline};

fn default_port() -> u16 {
    5004
}
fn default_channels() -> u8 {
    2
}
fn default_rate() -> u32 {
    48000
}
fn default_ptime() -> u32 {
    1000
}
fn default_pt() -> u8 {
    97
}
fn default_ttl() -> u8 {
    15
}
fn default_map() -> Vec<u8> {
    vec![1, 2]
}
fn default_true() -> bool {
    true
}

/// How an output is set up, from the add-on options or the control API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// What the user calls it: the player's name in Music Assistant.
    pub name: String,
    /// The SAP/SDP session name; the output's name when empty.
    #[serde(default)]
    pub session_name: String,
    /// Destination: a multicast group (239.x is AES67's usual range) or a unicast receiver.
    pub address: Ipv4Addr,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Channels in the AES67 stream.
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    /// Packet time in microseconds.
    #[serde(default = "default_ptime")]
    pub ptime_us: u32,
    #[serde(default = "default_pt")]
    pub payload_type: u8,
    #[serde(default = "default_ttl")]
    pub ttl: u8,
    /// For each channel the output plays (left, right), the stream channel
    /// (1-based) it goes to. One entry plays a mono downmix.
    #[serde(default = "default_map")]
    pub channel_map: Vec<u8>,
    /// Send silence while idle, so receivers stay subscribed.
    #[serde(default = "default_true")]
    pub keep_alive: bool,
}

pub const PTIMES_US: [u32; 5] = [125, 250, 333, 1000, 4000];
pub const SAMPLE_RATES: [u32; 3] = [44100, 48000, 96000];

impl OutputConfig {
    pub fn frames_per_packet(&self) -> usize {
        ((self.sample_rate as u64 * self.ptime_us as u64 + 500_000) / 1_000_000) as usize
    }

    pub fn session(&self) -> &str {
        if self.session_name.is_empty() {
            &self.name
        } else {
            &self.session_name
        }
    }

    /// The channels this output plays: as many as the map has entries.
    pub fn source_channels(&self) -> usize {
        self.channel_map.len()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name is empty".into());
        }
        let a = self.address;
        if a.is_unspecified() || a.is_broadcast() {
            return Err(format!("{a} is not a destination"));
        }
        if self.port == 0 {
            return Err("port 0".into());
        }
        if !SAMPLE_RATES.contains(&self.sample_rate) {
            return Err(format!("sample rate {} (use 44100, 48000 or 96000)", self.sample_rate));
        }
        if !PTIMES_US.contains(&self.ptime_us) {
            return Err(format!("packet time {} us (use 125, 250, 333, 1000 or 4000)", self.ptime_us));
        }
        let fits = ffi::max_channels(self.frames_per_packet());
        if self.channels == 0 || self.channels as usize > fits {
            return Err(format!(
                "{} channels do not fit a {} us packet (up to {fits})",
                self.channels, self.ptime_us
            ));
        }
        if !(96..=127).contains(&self.payload_type) {
            return Err(format!("payload type {} (use 96 to 127)", self.payload_type));
        }
        if self.channel_map.is_empty() || self.channel_map.len() > 2 {
            return Err("channel map needs one (mono) or two (left, right) entries".into());
        }
        for (i, &c) in self.channel_map.iter().enumerate() {
            if c == 0 || c > self.channels {
                return Err(format!("channel map entry {c} is outside the stream's 1..{}", self.channels));
            }
            if self.channel_map[..i].contains(&c) {
                return Err(format!("channel map uses channel {c} twice"));
            }
        }
        Ok(())
    }

    /// Whether two outputs can share a stream (same destination): the same format.
    pub fn same_stream_format(&self, o: &OutputConfig) -> bool {
        self.channels == o.channels
            && self.sample_rate == o.sample_rate
            && self.ptime_us == o.ptime_us
            && self.payload_type == o.payload_type
            && self.ttl == o.ttl
    }
}

/// Converts `frames` of `from` channels into `to` channels: mono is copied
/// to every channel, stereo to mono is the average, extra channels are dropped.
pub fn adapt_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    out.clear();
    if from == to {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(from) {
        if from == 1 {
            out.extend(std::iter::repeat(frame[0]).take(to));
        } else if to == 1 {
            out.push(frame.iter().sum::<f32>() / from as f32);
        } else {
            out.extend((0..to).map(|c| frame.get(c).copied().unwrap_or(0.0)));
        }
    }
}

/// Music Assistant's 0..100 volume as a gain: a 60 dB taper, 0 is silence.
pub fn volume_to_gain(level: u8) -> f32 {
    if level == 0 {
        return 0.0;
    }
    let level = level.min(100) as f32;
    10f32.powf(-60.0 * (1.0 - level / 100.0) / 20.0)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayState {
    Idle,
    Buffering,
    Playing,
    Error,
}

/// What an output is playing: one playback session, possibly shared by a group.
#[derive(Clone, Debug)]
pub struct Session {
    pub id: u64,
    /// Media time of the first frame.
    pub start: MediaTime,
    /// Frames laid down so far (an HTTP source), or the furthest frame written (Sendspin).
    pub written_until: MediaTime,
    /// The source ended: playback is over once playout reaches written_until.
    pub ended: bool,
    pub error: Option<String>,
    pub url: Option<String>,
    pub leader: String,
}

pub struct Output {
    pub id: String,
    pub cfg: OutputConfig,
    pub timeline: Timeline,
    volume: AtomicU32,
    muted: AtomicBool,
    pub session: Mutex<Option<Session>>,
    pub underrun_frames: AtomicU64,
    /// Bumped on every stop, so a source of an earlier session stops writing.
    pub generation: AtomicU64,
    /// Where the gain ramp is, owned by the transmit thread.
    pub gain_now: Mutex<f32>,
    /// Turned off through NMOS IS-05 (master_enable false).
    pub nmos_disabled: AtomicBool,
}

/// The playout buffer: about 2.7 s at 48 kHz, more than a Sendspin server
/// sends ahead and enough to ride out an HTTP stall.
pub fn timeline_frames(sample_rate: u32) -> usize {
    sample_rate as usize * 11 / 4
}

impl Output {
    pub fn new(id: &str, cfg: OutputConfig, volume: u8, muted: bool) -> Arc<Self> {
        let frames = timeline_frames(cfg.sample_rate);
        Arc::new(Output {
            id: id.to_string(),
            timeline: Timeline::new(cfg.source_channels(), frames),
            cfg,
            volume: AtomicU32::new(volume as u32),
            muted: AtomicBool::new(muted),
            session: Mutex::new(None),
            underrun_frames: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            gain_now: Mutex::new(0.0),
            nmos_disabled: AtomicBool::new(false),
        })
    }

    pub fn volume(&self) -> u8 {
        self.volume.load(Ordering::Relaxed) as u8
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, level: u8) {
        self.volume.store(level.min(100) as u32, Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn target_gain(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            volume_to_gain(self.volume())
        }
    }

    /// Ends whatever is playing and drops its audio.
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.session.lock().unwrap() = None;
        self.timeline.clear();
    }

    pub fn state(&self) -> PlayState {
        let s = self.session.lock().unwrap();
        let Some(s) = s.as_ref() else {
            return PlayState::Idle;
        };
        if s.error.is_some() {
            return PlayState::Error;
        }
        match self.timeline.read_pos() {
            Some(pos) if pos >= s.start && (s.written_until > s.start || s.ended) => {
                if s.ended && pos >= s.written_until {
                    PlayState::Idle
                } else {
                    PlayState::Playing
                }
            }
            _ => PlayState::Buffering,
        }
    }

    /// Seconds played of the current session.
    pub fn elapsed(&self) -> Option<f64> {
        let s = self.session.lock().unwrap();
        let s = s.as_ref()?;
        let pos = self.timeline.read_pos()?;
        let played = pos.min(s.written_until).saturating_sub(s.start);
        Some(played as f64 / self.cfg.sample_rate as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OutputConfig {
        serde_json::from_str(r#"{"name": "Lobby", "address": "239.69.1.10"}"#).unwrap()
    }

    #[test]
    fn defaults_are_aes67s() {
        let c = cfg();
        assert_eq!((c.port, c.channels, c.sample_rate, c.ptime_us), (5004, 2, 48000, 1000));
        assert_eq!(c.channel_map, vec![1, 2]);
        assert_eq!(c.frames_per_packet(), 48);
        assert_eq!(c.session(), "Lobby");
        c.validate().unwrap();
    }

    #[test]
    fn validation_explains_itself() {
        let bad = |f: &dyn Fn(&mut OutputConfig)| {
            let mut c = cfg();
            f(&mut c);
            c.validate().unwrap_err()
        };
        assert!(bad(&|c| c.channel_map = vec![3]).contains("outside"));
        assert!(bad(&|c| c.channel_map = vec![1, 1]).contains("twice"));
        assert!(bad(&|c| c.sample_rate = 22050).contains("sample rate"));
        assert!(bad(&|c| c.ptime_us = 2000).contains("packet time"));
        assert!(bad(&|c| c.channels = 100).contains("do not fit"));
        assert!(bad(&|c| c.address = Ipv4Addr::UNSPECIFIED).contains("not a destination"));
        assert!(serde_json::from_str::<OutputConfig>(r#"{"name":"x","address":"239.1.1.1","chanels":2}"#).is_err());
        let mut c = cfg();
        c.channels = 8;
        c.channel_map = vec![7, 8];
        c.validate().unwrap();
    }

    #[test]
    fn channels_adapt() {
        let mut out = Vec::new();
        adapt_channels(&[0.5, 0.25], 1, 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5, 0.25, 0.25]);
        adapt_channels(&[1.0, 0.0, 0.5, 0.5], 2, 1, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        adapt_channels(&[1.0, 2.0], 2, 2, &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn volume_taper() {
        assert_eq!(volume_to_gain(0), 0.0);
        assert_eq!(volume_to_gain(100), 1.0);
        assert!((volume_to_gain(50) - 0.031_622_8).abs() < 1e-5);
    }
}
