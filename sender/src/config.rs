// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! The add-on's options (`/data/options.json`, written by Home Assistant).

use std::net::Ipv4Addr;

use serde::Deserialize;

use crate::output::OutputConfig;
use crate::ptp::PtpRole;

fn d_follower() -> PtpRole {
    PtpRole::Follower
}
fn d_34() -> u8 {
    34
}
fn d_46() -> u8 {
    46
}
fn d_latency() -> u32 {
    3
}
fn d_api() -> u16 {
    8890
}
fn d_api_bind() -> String {
    "127.0.0.1".into()
}
fn d_nmos() -> u16 {
    8891
}
fn d_sendspin() -> u16 {
    crate::sendspin::DEFAULT_PORT
}
fn d_true() -> bool {
    true
}
fn d_port() -> u16 {
    5004
}
fn d_channels() -> u8 {
    2
}
fn d_rate() -> u32 {
    48000
}
fn d_ptime() -> u32 {
    1000
}
fn d_ttl() -> u8 {
    15
}
fn d_map() -> String {
    "1,2".into()
}

#[derive(Debug, Deserialize)]
pub struct AddonOptions {
    /// Network interface for AES67 and PTP; empty picks the first with an IPv4 address.
    #[serde(default)]
    pub interface: String,
    #[serde(default = "d_follower")]
    pub ptp_role: PtpRole,
    #[serde(default)]
    pub ptp_domain: u8,
    #[serde(default = "d_34")]
    pub dscp_media: u8,
    #[serde(default = "d_46")]
    pub dscp_ptp: u8,
    /// How long after its RTP timestamp the receivers play a sample (their link offset), ms.
    #[serde(default = "d_latency")]
    pub receiver_latency_ms: u32,
    #[serde(default = "d_true")]
    pub sendspin: bool,
    #[serde(default = "d_sendspin")]
    pub sendspin_port: u16,
    #[serde(default = "d_api")]
    pub api_port: u16,
    #[serde(default)]
    pub api_token: String,
    /// Where the control API listens: 127.0.0.1 (this host only) or 0.0.0.0 (the network).
    #[serde(default = "d_api_bind")]
    pub api_bind: String,
    #[serde(default = "d_true")]
    pub nmos: bool,
    #[serde(default = "d_nmos")]
    pub nmos_port: u16,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub outputs: Vec<AddonOutput>,
}

/// One output as the add-on options describe it (a flat form for Home Assistant's UI).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddonOutput {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
    pub address: Ipv4Addr,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_channels")]
    pub channels: u8,
    /// "1,2": Music Assistant's left to stream channel 1, right to 2. "5": a mono downmix to 5.
    #[serde(default = "d_map")]
    pub channel_map: String,
    #[serde(default = "d_rate", deserialize_with = "de_num")]
    pub sample_rate: u32,
    #[serde(default = "d_ptime", deserialize_with = "de_num")]
    pub ptime_us: u32,
    #[serde(default = "d_ttl")]
    pub ttl: u8,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default = "d_true")]
    pub keep_alive: bool,
}

/// Home Assistant's `list(...)` options arrive as strings: take either.
fn de_num<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        Int(u32),
        Text(String),
    }
    match N::deserialize(d)? {
        N::Int(n) => Ok(n),
        N::Text(t) => t.trim().parse().map_err(serde::de::Error::custom),
    }
}

pub fn parse_map(s: &str) -> Result<Vec<u8>, String> {
    s.split([',', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.trim()
                .parse::<u8>()
                .map_err(|_| format!("channel map '{s}': '{p}' is not a channel number"))
        })
        .collect()
}

/// A stable id from a name: lowercase letters, digits and '-'.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() {
        "output".into()
    } else {
        s.chars().take(48).collect()
    }
}

impl AddonOptions {
    /// The outputs as (id, config), ids unique.
    pub fn outputs(&self) -> Result<Vec<(String, OutputConfig)>, String> {
        let mut v: Vec<(String, OutputConfig)> = Vec::new();
        for o in &self.outputs {
            let base =
                o.id.clone()
                    .filter(|i| !i.is_empty())
                    .map(|i| slug(&i))
                    .unwrap_or_else(|| slug(&o.name));
            let mut id = base.clone();
            let mut n = 2;
            while v.iter().any(|(i, _)| *i == id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            let cfg = OutputConfig {
                name: o.name.clone(),
                session_name: o.session_name.clone().unwrap_or_default(),
                address: o.address,
                port: o.port,
                channels: o.channels,
                sample_rate: o.sample_rate,
                ptime_us: o.ptime_us,
                payload_type: 97,
                ttl: o.ttl,
                channel_map: parse_map(&o.channel_map)?,
                keep_alive: o.keep_alive,
            };
            cfg.validate()
                .map_err(|e| format!("output '{}': {e}", o.name))?;
            v.push((id, cfg));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_from_home_assistant() {
        let o: AddonOptions = serde_json::from_str(
            r#"{"interface": "", "ptp_role": "leader_if_none", "outputs": [
                {"name": "Lobby", "address": "239.69.1.10"},
                {"name": "Lobby", "address": "239.69.1.10", "channels": 2, "channel_map": "1", "sample_rate": "48000", "ptime_us": 1000},
                {"name": "Café Bar!", "address": "192.168.1.50", "channel_map": "2, 1", "id": "bar"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(o.ptp_role, PtpRole::LeaderIfNone);
        assert_eq!(
            (o.api_port, o.sendspin_port, o.receiver_latency_ms),
            (8890, 8928, 3)
        );
        // The second Lobby clashes on channel 1, but ids are still made unique.
        let outs = o.outputs().unwrap();
        assert_eq!(
            outs.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(),
            ["lobby", "lobby-2", "bar"]
        );
        assert_eq!(outs[2].1.channel_map, vec![2, 1]);
        assert_eq!(slug("Café Bar!"), "caf-bar");
        assert!(parse_map("1,x").is_err());
    }
}
