// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Bindings to the parts of libaes67 the sender uses: RTP framing, L24
//! conversion, the PTP media clock, SDP writing and SDP parsing.

use std::ffi::{c_char, c_int, CStr, CString};

pub const RTP_HEADER_BYTES: usize = 12;
pub const MAX_PAYLOAD_BYTES: usize = 1440;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

#[repr(C)]
struct SdpParams {
    session_name: *const c_char,
    origin_ip: *const c_char,
    group: *const c_char,
    port: u16,
    payload_type: u8,
    channels: u8,
    sample_rate: u32,
    ptime_us: u32,
    ssrc: u32,
    ttl: u8,
    gm_identity: [u8; 8],
    ptp_domain: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ParsedSdp {
    pub session_name: [c_char; 128],
    pub origin_ip: [c_char; 64],
    pub connection_ip: [c_char; 64],
    pub connection_ttl: c_int,
    pub source_ip: [c_char; 64],
    pub port: u16,
    pub payload_type: u8,
    pub encoding: [c_char; 16],
    pub sample_rate: u32,
    pub channels: u8,
    pub ptime_us: u32,
    pub maxptime_us: u32,
    pub framecount: u32,
    pub mediaclk_direct: bool,
    pub mediaclk_offset: i64,
    pub refclk_ptp: bool,
    pub ptp_version: [c_char; 24],
    pub gm_traceable: bool,
    pub gm_known: bool,
    pub gm_identity: [u8; 8],
    pub ptp_domain: c_int,
    pub refclk_localmac: bool,
    pub channel_order: [c_char; 64],
    pub direction: c_int,
    pub is_st2110_31: bool,
    pub essence: [c_char; 48],
    pub other_port: u16,
}

extern "C" {
    fn aes67_max_channels(bytes_per_sample: c_int, frames_per_packet: c_int) -> c_int;
    fn aes67_rtp_parse_header(packet: *const u8, len: usize, out: *mut RtpHeader) -> bool;
    fn aes67_l24_to_i32(p: *const u8) -> i32;
    fn aes67_media_clock_ts(ptp_ns: u64, rate: u32) -> u32;
    fn aes67_rtp_pack_l24(
        packet: *mut u8,
        cap: usize,
        h: *const RtpHeader,
        interleaved: *const i32,
        frames: c_int,
        channels: c_int,
    ) -> usize;
    fn aes67_sdp_write(buf: *mut c_char, len: usize, p: *const SdpParams) -> c_int;
    fn aes67_sdp_parse(
        text: *const c_char,
        len: usize,
        out: *mut ParsedSdp,
        errbuf: *mut c_char,
        errlen: usize,
    ) -> bool;
}

/// The most channels one L24 packet of `frames` frames can carry.
pub fn max_channels(frames_per_packet: usize) -> usize {
    unsafe { aes67_max_channels(3, frames_per_packet as c_int).max(0) as usize }
}

/// The RTP timestamp AES67 means by `a=mediaclk:direct=0` for PTP time `ptp_ns`.
pub fn media_clock_ts(ptp_ns: u64, rate: u32) -> u32 {
    unsafe { aes67_media_clock_ts(ptp_ns, rate) }
}

/// Packs a header and right-justified 24-bit samples into `packet`; the length, or 0.
pub fn pack_l24(packet: &mut [u8], h: &RtpHeader, interleaved: &[i32], channels: usize) -> usize {
    if channels == 0 || interleaved.len() % channels != 0 {
        return 0;
    }
    let frames = interleaved.len() / channels;
    unsafe {
        aes67_rtp_pack_l24(
            packet.as_mut_ptr(),
            packet.len(),
            h,
            interleaved.as_ptr(),
            frames as c_int,
            channels as c_int,
        )
    }
}

pub fn parse_header(packet: &[u8]) -> Option<RtpHeader> {
    let mut h = RtpHeader::default();
    unsafe { aes67_rtp_parse_header(packet.as_ptr(), packet.len(), &mut h) }.then_some(h)
}

pub fn l24_to_i32(p: &[u8]) -> i32 {
    assert!(p.len() >= 3);
    unsafe { aes67_l24_to_i32(p.as_ptr()) }
}

pub struct SdpInput<'a> {
    pub session_name: &'a str,
    pub origin_ip: &'a str,
    pub group: &'a str,
    pub port: u16,
    pub payload_type: u8,
    pub channels: u8,
    pub sample_rate: u32,
    pub ptime_us: u32,
    pub ssrc: u32,
    pub ttl: u8,
    pub gm_identity: [u8; 8],
    pub ptp_domain: u16,
}

/// libaes67's AES67 SDP for one stream.
pub fn sdp_write(p: &SdpInput) -> String {
    let clean = |s: &str| CString::new(s.replace(['\r', '\n', '\0'], " ")).unwrap();
    let (name, origin, group) = (clean(p.session_name), clean(p.origin_ip), clean(p.group));
    let params = SdpParams {
        session_name: name.as_ptr(),
        origin_ip: origin.as_ptr(),
        group: group.as_ptr(),
        port: p.port,
        payload_type: p.payload_type,
        channels: p.channels,
        sample_rate: p.sample_rate,
        ptime_us: p.ptime_us,
        ssrc: p.ssrc,
        ttl: p.ttl,
        gm_identity: p.gm_identity,
        ptp_domain: p.ptp_domain,
    };
    let mut buf = vec![0 as c_char; 2048];
    let n = unsafe { aes67_sdp_write(buf.as_mut_ptr(), buf.len(), &params) };
    if n <= 0 {
        return String::new();
    }
    let n = (n as usize).min(buf.len() - 1);
    let bytes: Vec<u8> = buf[..n].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// libaes67's SDP parser, for tests and for checking what we announce.
pub fn sdp_parse(text: &str) -> Result<ParsedSdp, String> {
    let mut out: ParsedSdp = unsafe { std::mem::zeroed() };
    let mut err = [0 as c_char; 256];
    let ok = unsafe {
        aes67_sdp_parse(text.as_ptr() as *const c_char, text.len(), &mut out, err.as_mut_ptr(), err.len())
    };
    if ok {
        Ok(out)
    } else {
        Err(unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned())
    }
}

pub fn c_str(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_clock_matches_the_definition() {
        assert_eq!(media_clock_ts(0, 48000), 0);
        assert_eq!(media_clock_ts(1_000_000_000, 48000), 48000);
        assert_eq!(media_clock_ts(1_500_000_000, 48000), 72000);
        let big = 1_800_000_000u64 * 1_000_000_000;
        assert_eq!(media_clock_ts(big, 48000), (1_800_000_000u64 * 48000) as u32);
    }

    #[test]
    fn packs_and_parses_a_packet() {
        let h = RtpHeader { payload_type: 97, marker: false, sequence: 7, timestamp: 1234, ssrc: 0xdead };
        let samples = [0x123456, -0x123456, 0x7fffff, -0x800000];
        let mut pkt = [0u8; 1500];
        let n = pack_l24(&mut pkt, &h, &samples, 2);
        assert_eq!(n, RTP_HEADER_BYTES + 12);
        assert_eq!(parse_header(&pkt[..n]), Some(h));
        for (i, s) in samples.iter().enumerate() {
            assert_eq!(l24_to_i32(&pkt[RTP_HEADER_BYTES + i * 3..]), *s);
        }
    }

    #[test]
    fn eight_channels_fit_a_millisecond() {
        assert!(max_channels(48) >= 8);
        assert!(max_channels(6) >= 64);
    }

    #[test]
    fn sdp_round_trips_through_the_parser() {
        let sdp = sdp_write(&SdpInput {
            session_name: "Lobby",
            origin_ip: "192.168.1.5",
            group: "239.69.1.10",
            port: 5004,
            payload_type: 97,
            channels: 2,
            sample_rate: 48000,
            ptime_us: 1000,
            ssrc: 1,
            ttl: 15,
            gm_identity: [0, 0x1d, 0xc1, 0xff, 0xfe, 1, 2, 3],
            ptp_domain: 0,
        });
        let p = sdp_parse(&sdp).unwrap();
        assert_eq!(c_str(&p.session_name), "Lobby");
        assert_eq!(c_str(&p.connection_ip), "239.69.1.10");
        assert_eq!(p.port, 5004);
        assert_eq!(p.channels, 2);
        assert_eq!(p.sample_rate, 48000);
        assert_eq!(p.ptime_us, 1000);
        assert!(p.gm_known && p.mediaclk_direct);
        assert_eq!(p.gm_identity, [0, 0x1d, 0xc1, 0xff, 0xfe, 1, 2, 3]);
    }
}
