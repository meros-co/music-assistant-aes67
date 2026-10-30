// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! One AES67 stream: a destination, the outputs mixed into it, and the thread
//! that sends a packet every packet time, stamped with the PTP media clock.
//!
//! A packet's RTP timestamp is the media time of its first frame, and it goes
//! out once its last frame is due (the sample "has been taken"), so a receiver
//! sees the same timing a hardware sender would give it.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::clock::{realtime_ns, Clock};
use crate::ffi::{self, RtpHeader};
use crate::log;
use crate::output::{Output, OutputConfig};
use crate::timeline::MediaTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamKey {
    pub address: Ipv4Addr,
    pub port: u16,
}

impl std::fmt::Display for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.address, self.port)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NetConfig {
    pub iface_ip: Ipv4Addr,
    pub dscp_media: u8,
}

/// Media time (frames) at PTP time `ptp_ns`.
pub fn media_at(ptp_ns: i64, rate: u32) -> MediaTime {
    (ptp_ns.max(0) as u128 * rate as u128 / 1_000_000_000) as u64
}

/// PTP time (ns) of media time `t`.
pub fn ptp_of(t: MediaTime, rate: u32) -> i64 {
    (t as u128 * 1_000_000_000 / rate as u128) as i64
}

#[derive(Default)]
pub struct StreamStats {
    pub packets: AtomicU64,
    pub send_errors: AtomicU64,
    pub resyncs: AtomicU64,
    pub sending: AtomicBool,
}

pub struct Stream {
    pub key: StreamKey,
    pub format: OutputConfig,
    pub ssrc: u32,
    pub outputs: Arc<RwLock<Vec<Arc<Output>>>>,
    pub stats: Arc<StreamStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub fn open_socket(net: &NetConfig, ttl: u8, dscp: u8) -> std::io::Result<Socket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if !net.iface_ip.is_unspecified() {
        s.set_multicast_if_v4(&net.iface_ip)?;
        s.bind(&SockAddr::from(SocketAddrV4::new(net.iface_ip, 0)))?;
    }
    s.set_multicast_ttl_v4(ttl as u32)?;
    s.set_multicast_loop_v4(true)?;
    #[cfg(not(windows))]
    let _ = s.set_tos((dscp as u32) << 2);
    #[cfg(windows)]
    let _ = dscp;
    Ok(s)
}

impl Stream {
    pub fn start(
        key: StreamKey,
        format: OutputConfig,
        outputs: Vec<Arc<Output>>,
        clock: Arc<Clock>,
        net: NetConfig,
    ) -> std::io::Result<Self> {
        let socket = open_socket(&net, format.ttl, net.dscp_media)?;
        let ssrc = stream_ssrc(&key, &net);
        let outputs = Arc::new(RwLock::new(outputs));
        let stats = Arc::new(StreamStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let tx = Tx {
            key,
            format: format.clone(),
            ssrc,
            socket,
            outputs: outputs.clone(),
            clock,
            stats: stats.clone(),
            stop: stop.clone(),
        };
        let thread = std::thread::Builder::new().name(format!("tx {key}")).spawn(move || tx.run())?;
        Ok(Stream { key, format, ssrc, outputs, stats, stop, thread: Some(thread) })
    }

    pub fn set_outputs(&self, outputs: Vec<Arc<Output>>) {
        *self.outputs.write().unwrap() = outputs;
    }

    pub fn is_sending(&self) -> bool {
        self.stats.sending.load(Ordering::Relaxed)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A stable SSRC per destination and sender, so a restart looks like the same source.
pub fn stream_ssrc(key: &StreamKey, net: &NetConfig) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in key.address.octets().iter().chain(&key.port.to_be_bytes()).chain(&net.iface_ip.octets()) {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

struct Tx {
    key: StreamKey,
    format: OutputConfig,
    ssrc: u32,
    socket: Socket,
    outputs: Arc<RwLock<Vec<Arc<Output>>>>,
    clock: Arc<Clock>,
    stats: Arc<StreamStats>,
    stop: Arc<AtomicBool>,
}

/// How late a wakeup may be before the stream gives up on catching up and
/// restarts its timestamps from now.
const MAX_LATE_NS: i64 = 50_000_000;

impl Tx {
    fn run(self) {
        raise_priority();
        let rate = self.format.sample_rate;
        let fpp = self.format.frames_per_packet();
        let channels = self.format.channels as usize;
        let dest = SockAddr::from(SocketAddrV4::new(self.key.address, self.key.port));
        let mut seq: u16 = (self.ssrc >> 16) as u16;
        let mut next: Option<MediaTime> = None;
        let mut epoch = self.clock.epoch();
        let mut mix = vec![0f32; fpp * channels];
        let mut src = Vec::new();
        let mut pcm = vec![0i32; fpp * channels];
        let mut packet = [0u8; 1500];

        while !self.stop.load(Ordering::Relaxed) {
            if self.clock.epoch() != epoch {
                epoch = self.clock.epoch();
                next = None;
                self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
            }
            let ts = match next {
                Some(t) => t,
                None => {
                    let now = media_at(self.clock.ptp_now(), rate);
                    (now / fpp as u64 + 1) * fpp as u64
                }
            };
            let due_ptp = ptp_of(ts + fpp as u64, rate);
            let due = self.clock.to_realtime(due_ptp);
            sleep_until(due, &self.stop);
            let late = realtime_ns() - due;
            if late > MAX_LATE_NS {
                log::warn(&format!("{}: woke {} ms late; restarting timestamps", self.key, late / 1_000_000));
                next = None;
                self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            mix.fill(0.0);
            let mut active = false;
            for out in self.outputs.read().unwrap().iter() {
                active |= self.mix_output(out, ts, fpp, &mut src, &mut mix);
            }
            self.stats.sending.store(active, Ordering::Relaxed);
            if active {
                for (d, s) in pcm.iter_mut().zip(&mix) {
                    *d = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                }
                let h = RtpHeader {
                    payload_type: self.format.payload_type,
                    marker: false,
                    sequence: seq,
                    timestamp: ts as u32,
                    ssrc: self.ssrc,
                };
                let n = ffi::pack_l24(&mut packet, &h, &pcm, channels);
                seq = seq.wrapping_add(1);
                if n > 0 && self.socket.send_to(&packet[..n], &dest).is_ok() {
                    self.stats.packets.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            next = Some(ts + fpp as u64);
        }
        self.stats.sending.store(false, Ordering::Relaxed);
    }

    /// Mixes one output's frames at `ts` into `mix`; whether it wants the stream sent.
    fn mix_output(&self, out: &Output, ts: MediaTime, fpp: usize, src: &mut Vec<f32>, mix: &mut [f32]) -> bool {
        let src_ch = out.cfg.source_channels();
        src.resize(fpp * src_ch, 0.0);
        // Read even when disabled, so playout (and its writers) keep moving.
        let present = out.timeline.read(ts, src);
        if out.nmos_disabled.load(Ordering::Relaxed) {
            return false;
        }
        let playing = {
            let s = out.session.lock().unwrap();
            match s.as_ref() {
                Some(s) => {
                    let end = ts + fpp as u64;
                    let in_range = end > s.start && (!s.ended || ts < s.written_until);
                    if in_range && present < fpp && s.error.is_none() {
                        let missing = (end.min(if s.ended { s.written_until } else { end }))
                            .saturating_sub(ts.max(s.start)) as usize;
                        out.underrun_frames.fetch_add(missing.saturating_sub(present) as u64, Ordering::Relaxed);
                    }
                    true
                }
                None => false,
            }
        };
        // Ramp the gain across the packet, at most full scale per 20 ms.
        let target = out.target_gain();
        let mut g = out.gain_now.lock().unwrap();
        let max_step = fpp as f32 / (self.format.sample_rate as f32 * 0.02);
        let g_end = *g + (target - *g).clamp(-max_step, max_step);
        let channels = self.format.channels as usize;
        for f in 0..fpp {
            let gain = *g + (g_end - *g) * (f + 1) as f32 / fpp as f32;
            for (c, &dst) in out.cfg.channel_map.iter().enumerate() {
                mix[f * channels + dst as usize - 1] += src[f * src_ch + c] * gain;
            }
        }
        *g = g_end;
        playing || out.cfg.keep_alive
    }
}

#[cfg(target_os = "linux")]
fn sleep_until(realtime: i64, _stop: &AtomicBool) {
    let ts = libc::timespec {
        tv_sec: (realtime / 1_000_000_000) as libc::time_t,
        tv_nsec: (realtime % 1_000_000_000) as libc::c_long,
    };
    unsafe {
        while libc::clock_nanosleep(libc::CLOCK_REALTIME, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) == libc::EINTR {}
    }
}

#[cfg(not(target_os = "linux"))]
fn sleep_until(realtime: i64, _stop: &AtomicBool) {
    let wait = realtime - realtime_ns();
    if wait > 0 {
        std::thread::sleep(std::time::Duration::from_nanos(wait as u64));
    }
}

#[cfg(target_os = "linux")]
fn raise_priority() {
    let param = libc::sched_param { sched_priority: 70 };
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
    if rc != 0 {
        log::debug("no real-time priority for the transmit thread (needs SYS_NICE)");
    }
}

#[cfg(not(target_os = "linux"))]
fn raise_priority() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_time_matches_libaes67() {
        for ptp in [0i64, 999_999_999, 1_000_000_000, 1_234_567_890_123_456_789] {
            for rate in [44100, 48000, 96000] {
                assert_eq!(media_at(ptp, rate) as u32, ffi::media_clock_ts(ptp as u64, rate));
            }
        }
        assert_eq!(media_at(ptp_of(123_456, 48000), 48000), 123_456);
    }

    #[test]
    fn ssrc_is_stable_and_distinct() {
        let net = NetConfig { iface_ip: Ipv4Addr::new(10, 0, 0, 2), dscp_media: 34 };
        let a = StreamKey { address: Ipv4Addr::new(239, 69, 1, 1), port: 5004 };
        let b = StreamKey { address: Ipv4Addr::new(239, 69, 1, 2), port: 5004 };
        assert_eq!(stream_ssrc(&a, &net), stream_ssrc(&a, &net));
        assert_ne!(stream_ssrc(&a, &net), stream_ssrc(&b, &net));
    }
}
