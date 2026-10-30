// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Sendspin's security layer: identity, PSK selection and the Noise KKpsk2
//! session in which the client is always the responder.

use std::path::{Path, PathBuf};

use data_encoding::{BASE32_NOPAD, BASE64URL_NOPAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION: u32 = 1;
pub const SUITE: &str = "25519_ChaChaPoly_SHA256";
const MAX_RECORDS: usize = 8;

pub fn b64url(b: &[u8]) -> String {
    BASE64URL_NOPAD.encode(b)
}

pub fn from_b64url(s: &str) -> Option<Vec<u8>> {
    BASE64URL_NOPAD.decode(s.as_bytes()).ok()
}

fn key32(v: &[u8]) -> Option<[u8; 32]> {
    v.try_into().ok()
}

pub fn psk_id(psk: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"sendspin-psk-id-v1");
    h.update(psk);
    b64url(&h.finalize())
}

pub fn sentinel_psk() -> [u8; 32] {
    Sha256::digest(b"sendspin-sentinel-psk-v1").into()
}

pub fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("system random source");
    b
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingRecord {
    pub server_id: String,
    pub psk: String,
    #[serde(default)]
    pub last_used: u64,
}

/// What a client keeps across restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub private_key: String,
    pub public_key: String,
    pub pairing_psk: String,
    #[serde(default)]
    pub records: Vec<PairingRecord>,
    #[serde(default)]
    pub output_delay_ms: u32,
    #[serde(default)]
    pub last_playback_server: Option<String>,
    #[serde(default = "default_volume")]
    pub volume: u8,
    #[serde(default)]
    pub muted: bool,
    #[serde(skip)]
    path: PathBuf,
}

fn default_volume() -> u8 {
    50
}

impl Identity {
    /// Loads the identity at `path`, or makes and saves a new one.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(mut id) = serde_json::from_str::<Identity>(&text) {
                id.path = path.to_path_buf();
                return Ok(id);
            }
        }
        let kp = snow::Builder::new(noise_params()).generate_keypair().map_err(std::io::Error::other)?;
        let id = Identity {
            private_key: b64url(&kp.private),
            public_key: b64url(&kp.public),
            pairing_psk: b64url(&random32()),
            records: Vec::new(),
            output_delay_ms: 0,
            last_playback_server: None,
            volume: default_volume(),
            muted: false,
            path: path.to_path_buf(),
        };
        id.save()?;
        Ok(id)
    }

    pub fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, &self.path)
    }

    pub fn client_id(&self) -> &str {
        &self.public_key
    }

    pub fn private(&self) -> [u8; 32] {
        from_b64url(&self.private_key).and_then(|v| key32(&v)).expect("stored private key")
    }

    pub fn pairing(&self) -> [u8; 32] {
        from_b64url(&self.pairing_psk).and_then(|v| key32(&v)).expect("stored pairing PSK")
    }

    /// The operator's pairing token: "SP:0" and base32 of public key and pairing PSK.
    pub fn pairing_token(&self) -> String {
        let mut raw = from_b64url(&self.public_key).unwrap_or_default();
        raw.extend_from_slice(&self.pairing());
        format!("SP:0{}", BASE32_NOPAD.encode(&raw).replace('2', "9"))
    }

    pub fn add_record(&mut self, server_id: &str, psk: [u8; 32], now: u64) {
        self.records.retain(|r| r.server_id != server_id);
        self.records.push(PairingRecord { server_id: server_id.into(), psk: b64url(&psk), last_used: now });
        while self.records.len() > MAX_RECORDS {
            let oldest = self.records.iter().enumerate().min_by_key(|(_, r)| r.last_used).map(|(i, _)| i).unwrap();
            self.records.remove(oldest);
        }
    }

    pub fn remove_record(&mut self, server_id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.server_id != server_id);
        before != self.records.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PskKind {
    LongTerm,
    Pairing,
    Sentinel,
}

/// Picks the PSK the server's first handshake message names. None only for
/// a long-term match recorded for a different server (a failure).
pub fn choose_psk(id: &Identity, server_id: &str, psk_id_wanted: &str, category: &str) -> Option<([u8; 32], PskKind)> {
    match category {
        "lt" => {
            for r in &id.records {
                let Some(psk) = from_b64url(&r.psk).and_then(|v| key32(&v)) else { continue };
                if psk_id(&psk) == psk_id_wanted {
                    return (r.server_id == server_id).then_some((psk, PskKind::LongTerm));
                }
            }
        }
        "pr" => {
            let p = id.pairing();
            if psk_id(&p) == psk_id_wanted {
                return Some((p, PskKind::Pairing));
            }
        }
        _ => {}
    }
    Some((sentinel_psk(), PskKind::Sentinel))
}

pub fn noise_params() -> snow::params::NoiseParams {
    format!("Noise_KKpsk2_{SUITE}").parse().unwrap()
}

/// A responder handshake waiting for the server's first message.
pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    pub fn new(id: &Identity, server_static: &[u8; 32], prologue: &[u8]) -> Result<Self, snow::Error> {
        let private = id.private();
        let placeholder = [0u8; 32];
        let state = snow::Builder::new(noise_params())
            .local_private_key(&private)?
            .remote_public_key(server_static)?
            .prologue(prologue)?
            .psk(2, &placeholder)?
            .build_responder()?;
        Ok(Handshake { state })
    }

    /// Reads message 1 and returns its payload: {"psk_id", "psk_category"}.
    pub fn read_first(&mut self, msg: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut buf = vec![0u8; msg.len()];
        let n = self.state.read_message(msg, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Mixes in the chosen PSK and writes message 2 (payload `{}`).
    pub fn write_second(mut self, psk: &[u8; 32]) -> Result<(Vec<u8>, Session), snow::Error> {
        self.state.set_psk(2, psk)?;
        let mut buf = vec![0u8; 256];
        let n = self.state.write_message(b"{}", &mut buf)?;
        buf.truncate(n);
        let hash = self.state.get_handshake_hash().to_vec();
        let transport = self.state.into_transport_mode()?;
        Ok((buf, Session { transport, hash }))
    }
}

pub struct Session {
    transport: snow::TransportState,
    /// The handshake hash, the prologue of any in-band re-handshake.
    pub hash: Vec<u8>,
}

pub const MAX_MESSAGE: usize = 65535;

impl Session {
    pub fn encrypt(&mut self, plain: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut out = vec![0u8; plain.len() + 16];
        let n = self.transport.write_message(plain, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    pub fn decrypt(&mut self, cipher: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut out = vec![0u8; cipher.len()];
        let n = self.transport.read_message(cipher, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

/// Reassembles fragmented messages (type 1): `[1][flags][orig][data]` then `[1][flags][data]`.
#[derive(Default)]
pub struct Reassembly {
    in_flight: Option<(u8, Vec<u8>)>,
}

impl Reassembly {
    /// Feeds one decrypted message; returns a whole message (type byte first)
    /// when one is complete. Err means the peer broke the framing rules.
    pub fn feed(&mut self, plain: Vec<u8>) -> Result<Option<Vec<u8>>, &'static str> {
        let Some(&kind) = plain.first() else { return Err("empty message") };
        if kind != 1 {
            if self.in_flight.is_some() {
                return Err("message inside a fragmented one");
            }
            return Ok(Some(plain));
        }
        let flags = *plain.get(1).ok_or("short fragment")?;
        if flags & !0x03 != 0 {
            return Err("reserved fragment flags");
        }
        let (first, last) = (flags & 0x02 != 0, flags & 0x01 != 0);
        if first {
            if self.in_flight.is_some() {
                return Err("nested fragment");
            }
            let orig = *plain.get(2).ok_or("short fragment")?;
            if orig == 1 {
                return Err("fragment of a fragment");
            }
            self.in_flight = Some((orig, plain[3..].to_vec()));
        } else {
            let (_, data) = self.in_flight.as_mut().ok_or("stray continuation")?;
            data.extend_from_slice(&plain[2..]);
        }
        if last {
            let (orig, data) = self.in_flight.take().unwrap();
            let mut whole = Vec::with_capacity(data.len() + 1);
            whole.push(orig);
            whole.extend_from_slice(&data);
            return Ok(Some(whole));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_matches_the_spec() {
        assert_eq!(
            data_encoding::HEXLOWER.encode(&sentinel_psk()),
            "1b5e24dbc1aed95fc2a5a338a90c05df44bd10f5ec1f4cd66cbf86272767b9d3"
        );
        assert_eq!(psk_id(&sentinel_psk()), "GFsV9tLaSQm9HcFWpKsgYQOr7wFTvNUtkmFwuVz3zoo");
    }

    #[test]
    fn pairing_token_matches_the_reference_vector() {
        let mut id = test_identity();
        id.public_key = b64url(&(0u8..32).collect::<Vec<_>>());
        id.pairing_psk = b64url(&(0xe0u8..=0xff).collect::<Vec<_>>());
        assert_eq!(
            id.pairing_token(),
            "SP:0AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQTCQKRMFYYDENBWHA5DYP6BYPC4PSOLZXH5DU6V97M5XXO74HR6LZ7J5PW674PT6X37T6757Y"
        );
    }

    pub fn test_identity() -> Identity {
        let dir = std::env::temp_dir().join(format!("sendspin-test-{}", random32()[0]));
        let path = dir.join(format!("{}.json", b64url(&random32()[..6])));
        let id = Identity::load_or_create(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        id
    }

    #[test]
    fn chooses_psks_by_category() {
        let mut id = test_identity();
        let lt = random32();
        id.add_record("server-a", lt, 1);
        assert_eq!(choose_psk(&id, "server-a", &psk_id(&lt), "lt"), Some((lt, PskKind::LongTerm)));
        assert_eq!(choose_psk(&id, "server-b", &psk_id(&lt), "lt"), None);
        let pr = id.pairing();
        assert_eq!(choose_psk(&id, "x", &psk_id(&pr), "pr"), Some((pr, PskKind::Pairing)));
        // A long-term id offered under the pairing category does not match.
        assert_eq!(choose_psk(&id, "server-a", &psk_id(&lt), "pr").unwrap().1, PskKind::Sentinel);
        assert_eq!(choose_psk(&id, "x", "nope", "sn").unwrap().1, PskKind::Sentinel);
    }

    #[test]
    fn handshake_against_an_initiator() {
        let id = test_identity();
        let server = snow::Builder::new(noise_params()).generate_keypair().unwrap();
        let client_pub: [u8; 32] = from_b64url(&id.public_key).unwrap().try_into().unwrap();
        let server_pub: [u8; 32] = server.public.clone().try_into().unwrap();
        let prologue = b"client-initserver-init";
        let psk = sentinel_psk();
        let mut init = snow::Builder::new(noise_params())
            .local_private_key(&server.private)
            .unwrap()
            .remote_public_key(&client_pub)
            .unwrap()
            .prologue(prologue)
            .unwrap()
            .psk(2, &psk)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut m1 = vec![0u8; 256];
        let payload = br#"{"psk_id":"GFsV9tLaSQm9HcFWpKsgYQOr7wFTvNUtkmFwuVz3zoo","psk_category":"sn"}"#;
        let n = init.write_message(payload, &mut m1).unwrap();

        let mut hs = Handshake::new(&id, &server_pub, prologue).unwrap();
        assert_eq!(hs.read_first(&m1[..n]).unwrap(), payload.to_vec());
        let (m2, mut session) = hs.write_second(&psk).unwrap();
        let mut got = vec![0u8; 64];
        let k = init.read_message(&m2, &mut got).unwrap();
        assert_eq!(&got[..k], b"{}");
        let mut server_t = init.into_transport_mode().unwrap();
        let mut c = vec![0u8; 64];
        let k = server_t.write_message(b"\x00hello", &mut c).unwrap();
        assert_eq!(session.decrypt(&c[..k]).unwrap(), b"\x00hello");
        let back = session.encrypt(b"\x00hi").unwrap();
        let mut p = vec![0u8; 64];
        let k = server_t.read_message(&back, &mut p).unwrap();
        assert_eq!(&p[..k], b"\x00hi");
    }

    #[test]
    fn reassembles_fragments_and_rejects_bad_framing() {
        let mut r = Reassembly::default();
        assert_eq!(r.feed(vec![0, b'{']).unwrap(), Some(vec![0, b'{']));
        assert_eq!(r.feed(vec![1, 0x02, 4, 1, 2]).unwrap(), None);
        assert!(r.feed(vec![0]).is_err());
        let mut r = Reassembly::default();
        r.feed(vec![1, 0x02, 4, 1, 2]).unwrap();
        assert_eq!(r.feed(vec![1, 0x01, 3]).unwrap(), Some(vec![4, 1, 2, 3]));
        assert!(r.feed(vec![1, 0x01, 3]).is_err());
        assert!(Reassembly::default().feed(vec![1, 0x06, 4]).is_err());
        assert!(Reassembly::default().feed(vec![]).is_err());
    }
}
