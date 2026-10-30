// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! SAP (RFC 2974) announcements of each stream's SDP on 239.255.255.255:9875,
//! every 30 s, again at once when the SDP changes, and a deletion when a
//! stream goes away. libaes67 builds the packets and hands out the message
//! hashes; this keeps the schedule and the socket.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddrV4};

use socket2::SockAddr;

use crate::ffi;
use crate::stream::{open_socket, NetConfig, StreamKey};

pub const SAP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);
pub const SAP_PORT: u16 = 9875;
pub const INTERVAL_MS: u64 = 30_000;

struct Announced {
    sdp: String,
    /// Fresh for each SDP, and kept for its deletion.
    hash: u16,
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
            self.send(&a, true);
        }
        for (k, sdp) in active {
            match self.announced.get_mut(k) {
                Some(a) if a.sdp == *sdp => {
                    if now_ms.saturating_sub(a.last_ms) >= INTERVAL_MS {
                        a.last_ms = now_ms;
                        let a = &self.announced[k];
                        self.send(a, false);
                    }
                }
                Some(_) => {
                    let new = Announced {
                        sdp: sdp.clone(),
                        hash: ffi::new_sap_hash(),
                        last_ms: now_ms,
                    };
                    let old = self.announced.insert(*k, new).unwrap();
                    self.send(&old, true);
                    self.send(&self.announced[k], false);
                }
                None => {
                    let a = Announced {
                        sdp: sdp.clone(),
                        hash: ffi::new_sap_hash(),
                        last_ms: now_ms,
                    };
                    self.send(&a, false);
                    self.announced.insert(*k, a);
                }
            }
        }
    }

    /// Deletes every announcement (at shutdown).
    pub fn withdraw_all(&mut self) {
        self.tick(0, &BTreeMap::new());
    }

    fn send(&self, a: &Announced, deletion: bool) {
        let p = ffi::sap_packet(self.origin, a.hash, &a.sdp, deletion);
        if !p.is_empty() {
            let _ = self.socket.send_to(&p, &self.dest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::time::Duration;

    /// The announcer over loopback: an announcement, a new hash when the SDP
    /// changes with a deletion of the old one under its own hash, and a
    /// deletion when the stream goes.
    #[test]
    fn announces_replaces_and_withdraws() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let net = NetConfig {
            iface_ip: Ipv4Addr::LOCALHOST,
            dscp_media: 0,
        };
        let mut a = Announcer::new(&net).unwrap();
        a.dest = SockAddr::from(rx.local_addr().unwrap());
        let next = || {
            let mut b = [0u8; 2048];
            let n = rx.recv(&mut b).unwrap();
            ffi::sap_parse(&b[..n]).unwrap()
        };
        let key = StreamKey {
            address: Ipv4Addr::new(239, 69, 1, 1),
            port: 5004,
        };
        let mut active = BTreeMap::new();
        active.insert(key, "v=0\r\ns=A\r\n".to_string());
        a.tick(0, &active);
        let (del, h1, origin, sdp) = next();
        assert!(!del && h1 != 0 && origin == Ipv4Addr::LOCALHOST && sdp.contains("s=A"));

        active.insert(key, "v=0\r\ns=B\r\n".to_string());
        a.tick(1000, &active);
        let (del, h, _, sdp) = next();
        assert!(del && h == h1 && sdp.contains("s=A"));
        let (del, h2, _, sdp) = next();
        assert!(!del && h2 != h1 && sdp.contains("s=B"));

        a.withdraw_all();
        let (del, h, _, sdp) = next();
        assert!(del && h == h2 && sdp.contains("s=B"));
    }
}
