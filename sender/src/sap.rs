// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! SAP (RFC 2974) announcements of each stream's SDP on 239.255.255.255:9875,
//! every 30 s, again at once when the SDP changes, and a deletion when a
//! stream goes away.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddrV4};

use socket2::SockAddr;

use crate::stream::{open_socket, NetConfig, StreamKey};

pub const SAP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);
pub const SAP_PORT: u16 = 9875;
pub const INTERVAL_MS: u64 = 30_000;

/// The 16-bit message id hash: it changes whenever the SDP does, as RFC 2974 asks.
pub fn msg_hash(sdp: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for b in sdp.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    let h = (h ^ (h >> 16)) as u16;
    if h == 0 {
        1
    } else {
        h
    }
}

pub fn packet(origin: Ipv4Addr, sdp: &str, deletion: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + sdp.len());
    // V=1, IPv4, announcement or deletion, no encryption or compression.
    p.push(if deletion { 0x24 } else { 0x20 });
    p.push(0);
    p.extend_from_slice(&msg_hash(sdp).to_be_bytes());
    p.extend_from_slice(&origin.octets());
    p.extend_from_slice(b"application/sdp\0");
    p.extend_from_slice(sdp.as_bytes());
    p
}

/// Parses a SAP packet: (deletion, origin, sdp).
pub fn parse(p: &[u8]) -> Option<(bool, Ipv4Addr, String)> {
    if p.len() < 8 || p[0] >> 5 != 1 || p[0] & 0x10 != 0 {
        return None;
    }
    let auth = p[1] as usize * 4;
    let origin = Ipv4Addr::new(p[4], p[5], p[6], p[7]);
    let mut rest = p.get(8 + auth..)?;
    if rest.starts_with(b"application/sdp\0") {
        rest = &rest[16..];
    }
    Some((
        p[0] & 0x04 != 0,
        origin,
        String::from_utf8_lossy(rest).into_owned(),
    ))
}

struct Announced {
    sdp: String,
    last_ms: u64,
}

pub struct Announcer {
    socket: socket2::Socket,
    origin: Ipv4Addr,
    dest: SockAddr,
    announced: BTreeMap<StreamKey, Announced>,
}

impl Announcer {
    pub fn new(net: &NetConfig) -> std::io::Result<Self> {
        Ok(Announcer {
            socket: open_socket(net, 255, 0)?,
            origin: net.iface_ip,
            dest: SockAddr::from(SocketAddrV4::new(SAP_GROUP, SAP_PORT)),
            announced: BTreeMap::new(),
        })
    }

    /// Brings the announcements in line with `active` (the streams being sent
    /// and their SDP). Call about once a second.
    pub fn tick(&mut self, now_ms: u64, active: &BTreeMap<StreamKey, String>) {
        let gone: Vec<StreamKey> = self
            .announced
            .keys()
            .filter(|k| !active.contains_key(k))
            .copied()
            .collect();
        for k in gone {
            let a = self.announced.remove(&k).unwrap();
            self.send(&a.sdp, true);
        }
        for (k, sdp) in active {
            match self.announced.get_mut(k) {
                Some(a) if a.sdp == *sdp => {
                    if now_ms.saturating_sub(a.last_ms) >= INTERVAL_MS {
                        a.last_ms = now_ms;
                        let sdp = a.sdp.clone();
                        self.send(&sdp, false);
                    }
                }
                Some(a) => {
                    let old = std::mem::replace(&mut a.sdp, sdp.clone());
                    a.last_ms = now_ms;
                    self.send(&old, true);
                    self.send(sdp, false);
                }
                None => {
                    self.announced.insert(
                        *k,
                        Announced {
                            sdp: sdp.clone(),
                            last_ms: now_ms,
                        },
                    );
                    self.send(sdp, false);
                }
            }
        }
    }

    /// Deletes every announcement (at shutdown).
    pub fn withdraw_all(&mut self) {
        self.tick(0, &BTreeMap::new());
    }

    fn send(&self, sdp: &str, deletion: bool) {
        let _ = self
            .socket
            .send_to(&packet(self.origin, sdp, deletion), &self.dest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let p = packet(Ipv4Addr::new(10, 1, 2, 3), "v=0\r\n", false);
        assert_eq!(p[0], 0x20);
        assert_eq!(
            parse(&p),
            Some((false, Ipv4Addr::new(10, 1, 2, 3), "v=0\r\n".into()))
        );
        let d = packet(Ipv4Addr::new(10, 1, 2, 3), "v=0\r\n", true);
        assert!(parse(&d).unwrap().0);
        assert_eq!(&p[2..4], &d[2..4]);
        assert_ne!(msg_hash("a"), msg_hash("b"));
    }
}
