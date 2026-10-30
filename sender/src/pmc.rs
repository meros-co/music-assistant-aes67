// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! ptp4l's management protocol (what `pmc` speaks), over its UNIX socket.
//!
//! A management message is a PTP header (34 bytes), the management fields
//! (target port identity, hops, action) and one MANAGEMENT TLV. Every field is
//! in network order. GET requests carry a zeroed data field of the expected
//! size, as pmc sends them.

pub const TIME_STATUS_NP: u16 = 0xC000;
pub const TIME_PROPERTIES_DATA_SET: u16 = 0x2003;
pub const DEFAULT_DATA_SET: u16 = 0x2000;

const MSG_MANAGEMENT: u8 = 0x0D;
const TLV_MANAGEMENT: u16 = 0x0001;
const TLV_MANAGEMENT_ERROR_STATUS: u16 = 0x0002;
const ACTION_GET: u8 = 0;
const ACTION_RESPONSE: u8 = 2;
const HEADER_LEN: usize = 34;
const MGMT_LEN: usize = 14;

pub fn data_len(id: u16) -> usize {
    match id {
        TIME_STATUS_NP => 50,
        TIME_PROPERTIES_DATA_SET => 4,
        DEFAULT_DATA_SET => 20,
        _ => 0,
    }
}

/// A GET for `id`, from `source` (our port identity) in `domain`.
pub fn encode_get(id: u16, domain: u8, sequence: u16, source: &[u8; 10]) -> Vec<u8> {
    let data = data_len(id);
    let tlv_len = 2 + data;
    let total = HEADER_LEN + MGMT_LEN + 4 + tlv_len;
    let mut m = vec![0u8; total];
    m[0] = MSG_MANAGEMENT;
    m[1] = 2;
    m[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    m[4] = domain;
    m[20..30].copy_from_slice(source);
    m[30..32].copy_from_slice(&sequence.to_be_bytes());
    m[32] = 0x04;
    m[33] = 0x7f;
    // Target: every clock, every port.
    m[34..44].fill(0xff);
    m[44] = 0; // starting boundary hops
    m[45] = 0; // boundary hops
    m[46] = ACTION_GET;
    m[48..50].copy_from_slice(&TLV_MANAGEMENT.to_be_bytes());
    m[50..52].copy_from_slice(&(tlv_len as u16).to_be_bytes());
    m[52..54].copy_from_slice(&id.to_be_bytes());
    m
}

#[derive(Debug, PartialEq, Eq)]
pub struct Response<'a> {
    pub id: u16,
    pub sequence: u16,
    pub source: [u8; 10],
    pub data: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Short,
    NotManagement,
    NotResponse,
    ManagementError(u16),
}

pub fn decode(msg: &[u8]) -> Result<Response<'_>, DecodeError> {
    if msg.len() < HEADER_LEN + MGMT_LEN + 2 {
        return Err(DecodeError::Short);
    }
    if msg[0] & 0x0f != MSG_MANAGEMENT {
        return Err(DecodeError::NotManagement);
    }
    if msg[46] & 0x0f != ACTION_RESPONSE {
        return Err(DecodeError::NotResponse);
    }
    let tlv_type = u16::from_be_bytes([msg[48], msg[49]]);
    let tlv_len = u16::from_be_bytes([msg[50], msg[51]]) as usize;
    if tlv_type == TLV_MANAGEMENT_ERROR_STATUS {
        return Err(DecodeError::ManagementError(u16::from_be_bytes([
            msg[52], msg[53],
        ])));
    }
    if tlv_type != TLV_MANAGEMENT || tlv_len < 2 || msg.len() < 52 + tlv_len {
        return Err(DecodeError::Short);
    }
    let mut source = [0u8; 10];
    source.copy_from_slice(&msg[20..30]);
    Ok(Response {
        id: u16::from_be_bytes([msg[52], msg[53]]),
        sequence: u16::from_be_bytes([msg[30], msg[31]]),
        source,
        data: &msg[54..52 + tlv_len],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeStatus {
    pub master_offset_ns: i64,
    pub ingress_time_ns: i64,
    pub gm_present: bool,
    pub gm_identity: [u8; 8],
}

pub fn parse_time_status(d: &[u8]) -> Option<TimeStatus> {
    if d.len() < 50 {
        return None;
    }
    let i64_at = |o: usize| i64::from_be_bytes(d[o..o + 8].try_into().unwrap());
    // master_offset 0..8, ingress_time 8..16, cumulativeScaledRateOffset 16..20,
    // scaledLastGmPhaseChange 20..24, gmTimeBaseIndicator 24..26,
    // lastGmPhaseChange 26..38, gmPresent 38..42, gmIdentity 42..50.
    let mut gm = [0u8; 8];
    gm.copy_from_slice(&d[42..50]);
    Some(TimeStatus {
        master_offset_ns: i64_at(0),
        ingress_time_ns: i64_at(8),
        gm_present: i32::from_be_bytes(d[38..42].try_into().unwrap()) != 0,
        gm_identity: gm,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeProperties {
    pub current_utc_offset: i16,
    pub utc_offset_valid: bool,
    pub ptp_timescale: bool,
}

pub fn parse_time_properties(d: &[u8]) -> Option<TimeProperties> {
    if d.len() < 4 {
        return None;
    }
    Some(TimeProperties {
        current_utc_offset: i16::from_be_bytes([d[0], d[1]]),
        utc_offset_valid: d[2] & 0x04 != 0,
        ptp_timescale: d[2] & 0x08 != 0,
    })
}

impl TimeProperties {
    /// What ptp4l subtracted from its UTC clock, and so what we add back.
    pub fn timescale_offset_s(&self) -> i64 {
        if !self.ptp_timescale {
            0
        } else if self.utc_offset_valid {
            self.current_utc_offset as i64
        } else {
            crate::clock::DEFAULT_UTC_OFFSET_S
        }
    }
}

/// DEFAULT_DATA_SET: flags, reserved, numberPorts, priority1, clockQuality
/// and priority2 come first, so our own clock identity sits at offset 10.
pub fn parse_clock_identity(d: &[u8]) -> Option<[u8; 8]> {
    d.get(10..18).map(|s| s.try_into().unwrap())
}

#[cfg(unix)]
pub mod client {
    use super::*;
    use std::io;
    use std::os::unix::net::UnixDatagram;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// A connection to ptp4l's management socket.
    pub struct Pmc {
        sock: UnixDatagram,
        local: PathBuf,
        domain: u8,
        sequence: u16,
        source: [u8; 10],
    }

    impl Pmc {
        pub fn connect(server: &Path, domain: u8) -> io::Result<Self> {
            let local = std::env::temp_dir().join(format!("aes67-pmc.{}", std::process::id()));
            let _ = std::fs::remove_file(&local);
            let sock = UnixDatagram::bind(&local)?;
            sock.connect(server)?;
            sock.set_read_timeout(Some(Duration::from_millis(500)))?;
            let mut source = [0u8; 10];
            source[..4].copy_from_slice(&std::process::id().to_be_bytes());
            Ok(Pmc {
                sock,
                local,
                domain,
                sequence: 0,
                source,
            })
        }

        /// GETs one management id and returns its data field.
        pub fn get(&mut self, id: u16) -> io::Result<Vec<u8>> {
            self.sequence = self.sequence.wrapping_add(1);
            let msg = encode_get(id, self.domain, self.sequence, &self.source);
            self.sock.send(&msg)?;
            let mut buf = [0u8; 1500];
            loop {
                let n = self.sock.recv(&mut buf)?;
                match decode(&buf[..n]) {
                    Ok(r) if r.id == id && r.sequence == self.sequence => {
                        return Ok(r.data.to_vec())
                    }
                    Ok(_) => continue,
                    Err(DecodeError::ManagementError(code)) => {
                        return Err(io::Error::other(format!("ptp4l management error {code}")))
                    }
                    Err(_) => continue,
                }
            }
        }
    }

    impl Drop for Pmc {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.local);
        }
    }
}

#[cfg(test)]
pub fn encode_response(id: u16, sequence: u16, data: &[u8]) -> Vec<u8> {
    let mut m = encode_get(id, 0, sequence, &[1; 10]);
    m.truncate(54);
    m.extend_from_slice(data);
    let total = m.len() as u16;
    m[2..4].copy_from_slice(&total.to_be_bytes());
    m[46] = ACTION_RESPONSE;
    m[50..52].copy_from_slice(&((2 + data.len()) as u16).to_be_bytes());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_get_is_laid_out_like_pmcs() {
        let m = encode_get(TIME_STATUS_NP, 3, 0x1234, &[7; 10]);
        assert_eq!(m.len(), 54 + 50);
        assert_eq!(m[0], 0x0d);
        assert_eq!(m[1], 2);
        assert_eq!(u16::from_be_bytes([m[2], m[3]]) as usize, m.len());
        assert_eq!(m[4], 3);
        assert_eq!(&m[30..32], &[0x12, 0x34]);
        assert_eq!(m[32], 4);
        assert!(m[34..44].iter().all(|&b| b == 0xff));
        assert_eq!(&m[48..54], &[0, 1, 0, 52, 0xc0, 0]);
    }

    #[test]
    fn parses_time_status() {
        let mut d = vec![0u8; 50];
        d[0..8].copy_from_slice(&(-12345i64).to_be_bytes());
        d[8..16].copy_from_slice(&1_700_000_000_000_000_000i64.to_be_bytes());
        d[38..42].copy_from_slice(&1i32.to_be_bytes());
        d[42..50].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let msg = encode_response(TIME_STATUS_NP, 9, &d);
        let r = decode(&msg).unwrap();
        assert_eq!((r.id, r.sequence), (TIME_STATUS_NP, 9));
        let ts = parse_time_status(r.data).unwrap();
        assert_eq!(ts.master_offset_ns, -12345);
        assert_eq!(ts.ingress_time_ns, 1_700_000_000_000_000_000);
        assert!(ts.gm_present);
        assert_eq!(ts.gm_identity, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn time_properties_say_what_offset_was_removed() {
        let tp = parse_time_properties(&[0, 37, 0x0c, 0xa0]).unwrap();
        assert_eq!(tp.timescale_offset_s(), 37);
        let arb = parse_time_properties(&[0, 37, 0x04, 0xa0]).unwrap();
        assert_eq!(arb.timescale_offset_s(), 0);
        let unknown = parse_time_properties(&[0, 0, 0x08, 0xa0]).unwrap();
        assert_eq!(unknown.timescale_offset_s(), 37);
    }

    #[test]
    fn reports_management_errors() {
        let mut msg = encode_response(TIME_STATUS_NP, 1, &[0, 5, 0xc0, 0]);
        msg[48..50].copy_from_slice(&TLV_MANAGEMENT_ERROR_STATUS.to_be_bytes());
        msg[52..54].copy_from_slice(&5u16.to_be_bytes());
        assert_eq!(decode(&msg), Err(DecodeError::ManagementError(5)));
    }
}
