// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! PTP time without touching the host clock.
//!
//! ptp4l runs free (it measures but never adjusts), so the host's own clock
//! stays as Home Assistant OS keeps it. ptp4l reports `master_offset`, the
//! local clock minus the grandmaster, at each Sync's ingress time. This module
//! fits those offsets against local time (phase and rate, least squares over a
//! sliding window) and turns local time into the grandmaster's time and back.
//!
//! With software timestamping ptp4l measures against CLOCK_REALTIME, which is
//! UTC, and corrects for the grandmaster's UTC offset when the grandmaster
//! runs the PTP (TAI) timescale; that correction is added back here.

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Local realtime in ns since the Unix epoch.
pub fn realtime_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// TAI - UTC since 2017, what a clock with no better information assumes.
pub const DEFAULT_UTC_OFFSET_S: i64 = 37;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    /// Following a grandmaster on the network.
    Ptp,
    /// This add-on's ptp4l is the grandmaster.
    Leader,
    /// No PTP: the host clock plus the TAI offset.
    System,
}

#[derive(Clone, Copy, Debug)]
pub struct PtpSample {
    /// Local realtime (ns) at which the offset was measured.
    pub ingress_ns: i64,
    /// Local clock minus grandmaster (ns), as ptp4l reports it.
    pub master_offset_ns: i64,
    pub gm_present: bool,
    pub gm_identity: [u8; 8],
    /// Seconds to add to UTC to reach the grandmaster's timescale.
    pub timescale_offset_s: i64,
    pub we_are_gm: bool,
    pub domain: u8,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ClockStatus {
    pub source: ClockSource,
    pub locked: bool,
    pub grandmaster: Option<String>,
    pub domain: u8,
    /// RMS of the fit's residuals, ns.
    pub jitter_ns: f64,
    /// Local clock rate relative to the grandmaster, in parts per million.
    pub rate_ppm: f64,
    pub samples: usize,
    /// Bumped whenever the time base steps (a new grandmaster, a jump):
    /// streams restart their timestamps from it.
    pub epoch: u64,
}

pub fn format_clock_identity(id: &[u8; 8]) -> String {
    id.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Clone, Copy, Debug)]
struct Fit {
    /// Offset (ns) at t_ref, and its slope (ns per ns).
    offset: f64,
    rate: f64,
    t_ref: i64,
}

impl Fit {
    fn offset_at(&self, t: i64) -> f64 {
        self.offset + self.rate * (t - self.t_ref) as f64
    }
}

/// The sliding-window fit, separate from locking so it can be tested alone.
pub struct OffsetFilter {
    window: Vec<(i64, f64)>,
    max_samples: usize,
    max_age_ns: i64,
    step_threshold_ns: f64,
    pending_steps: usize,
    fit: Option<Fit>,
    rms: f64,
}

impl Default for OffsetFilter {
    fn default() -> Self {
        OffsetFilter {
            window: Vec::new(),
            max_samples: 64,
            max_age_ns: 16_000_000_000,
            step_threshold_ns: 1_000_000.0,
            pending_steps: 0,
            fit: None,
            rms: 0.0,
        }
    }
}

pub enum FilterEvent {
    Accepted,
    Duplicate,
    Outlier,
    /// The offset jumped and stayed: the window restarted from this sample.
    Stepped,
}

impl OffsetFilter {
    pub fn reset(&mut self) {
        self.window.clear();
        self.fit = None;
        self.pending_steps = 0;
        self.rms = 0.0;
    }

    pub fn len(&self) -> usize {
        self.window.len()
    }

    pub fn add(&mut self, t: i64, offset: f64) -> FilterEvent {
        if self.window.last().is_some_and(|&(lt, _)| t <= lt) {
            return FilterEvent::Duplicate;
        }
        if let Some(fit) = self.fit {
            if self.window.len() >= 4 && (offset - fit.offset_at(t)).abs() > self.step_threshold_ns
            {
                self.pending_steps += 1;
                if self.pending_steps < 3 {
                    return FilterEvent::Outlier;
                }
                self.reset();
                self.window.push((t, offset));
                self.refit();
                return FilterEvent::Stepped;
            }
        }
        self.pending_steps = 0;
        self.window.push((t, offset));
        let oldest = t - self.max_age_ns;
        self.window.retain(|&(st, _)| st >= oldest);
        if self.window.len() > self.max_samples {
            let excess = self.window.len() - self.max_samples;
            self.window.drain(..excess);
        }
        self.refit();
        FilterEvent::Accepted
    }

    fn refit(&mut self) {
        let n = self.window.len();
        if n == 0 {
            self.fit = None;
            return;
        }
        let t_ref = self.window[n - 1].0;
        let xs: Vec<f64> = self
            .window
            .iter()
            .map(|&(t, _)| (t - t_ref) as f64)
            .collect();
        let ys: Vec<f64> = self.window.iter().map(|&(_, o)| o).collect();
        let mx = xs.iter().sum::<f64>() / n as f64;
        let my = ys.iter().sum::<f64>() / n as f64;
        let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
        let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
        // Under two seconds of history the slope is mostly noise.
        let span = xs.first().map(|x| -x).unwrap_or(0.0);
        let rate = if n >= 3 && sxx > 0.0 && span >= 2e9 {
            sxy / sxx
        } else {
            0.0
        };
        let offset = my - rate * mx;
        let fit = Fit {
            offset,
            rate,
            t_ref,
        };
        let sq: f64 = self
            .window
            .iter()
            .map(|&(t, o)| (o - fit.offset_at(t)).powi(2))
            .sum();
        self.rms = (sq / n as f64).sqrt();
        self.fit = Some(fit);
    }

    fn offset_at(&self, t: i64) -> Option<f64> {
        self.fit.map(|f| f.offset_at(t))
    }
}

struct State {
    source: ClockSource,
    filter: OffsetFilter,
    gm_identity: Option<[u8; 8]>,
    domain: u8,
    timescale_offset_s: i64,
    epoch: u64,
    locked: bool,
}

/// The PTP media clock every stream reads.
pub struct Clock {
    state: RwLock<State>,
    lock_samples: usize,
    lock_jitter_ns: f64,
}

impl Clock {
    /// A clock that has not heard from PTP: the host clock plus the TAI offset.
    pub fn new() -> Self {
        Clock {
            state: RwLock::new(State {
                source: ClockSource::System,
                filter: OffsetFilter::default(),
                gm_identity: None,
                domain: 0,
                timescale_offset_s: DEFAULT_UTC_OFFSET_S,
                epoch: 0,
                locked: false,
            }),
            lock_samples: 8,
            lock_jitter_ns: 250_000.0,
        }
    }

    /// Feeds one ptp4l reading.
    pub fn update(&self, s: &PtpSample) {
        let mut st = self.state.write().unwrap();
        st.domain = s.domain;
        if s.we_are_gm {
            if st.source != ClockSource::Leader || st.gm_identity != Some(s.gm_identity) {
                st.epoch += 1;
                st.filter.reset();
            }
            st.source = ClockSource::Leader;
            st.gm_identity = Some(s.gm_identity);
            st.timescale_offset_s = s.timescale_offset_s;
            st.locked = true;
            return;
        }
        if !s.gm_present {
            self.lose_locked(&mut st);
            return;
        }
        if st.gm_identity != Some(s.gm_identity) || st.source != ClockSource::Ptp {
            st.filter.reset();
            st.epoch += 1;
            st.gm_identity = Some(s.gm_identity);
            st.source = ClockSource::Ptp;
            st.locked = false;
        }
        if st.timescale_offset_s != s.timescale_offset_s {
            st.timescale_offset_s = s.timescale_offset_s;
            st.epoch += 1;
        }
        if let FilterEvent::Stepped = st.filter.add(s.ingress_ns, s.master_offset_ns as f64) {
            st.epoch += 1;
            st.locked = false;
        }
        st.locked = st.filter.len() >= self.lock_samples && st.filter.rms <= self.lock_jitter_ns;
    }

    /// ptp4l went away or lost every grandmaster: keep running on the last
    /// estimate, unlocked.
    pub fn lose(&self) {
        let mut st = self.state.write().unwrap();
        self.lose_locked(&mut st);
    }

    fn lose_locked(&self, st: &mut State) {
        if st.source == ClockSource::Leader {
            st.source = ClockSource::System;
            st.gm_identity = None;
            st.epoch += 1;
        }
        st.locked = false;
    }

    /// Local realtime (ns) to the grandmaster's time (ns since the PTP epoch).
    pub fn to_ptp(&self, realtime: i64) -> i64 {
        let st = self.state.read().unwrap();
        let offset = match st.source {
            ClockSource::Ptp => st.filter.offset_at(realtime).unwrap_or(0.0),
            _ => 0.0,
        };
        realtime + st.timescale_offset_s * 1_000_000_000 - offset.round() as i64
    }

    /// The grandmaster's time (ns) back to local realtime (ns).
    pub fn to_realtime(&self, ptp: i64) -> i64 {
        // offset(r) is nearly constant: a few fixed-point steps are exact to under a ns.
        let mut r = ptp - self.timescale_ns();
        for _ in 0..3 {
            r += ptp - self.to_ptp(r);
        }
        r
    }

    fn timescale_ns(&self) -> i64 {
        self.state.read().unwrap().timescale_offset_s * 1_000_000_000
    }

    pub fn ptp_now(&self) -> i64 {
        self.to_ptp(realtime_ns())
    }

    pub fn epoch(&self) -> u64 {
        self.state.read().unwrap().epoch
    }

    pub fn grandmaster(&self) -> ([u8; 8], u8) {
        let st = self.state.read().unwrap();
        (st.gm_identity.unwrap_or([0; 8]), st.domain)
    }

    pub fn status(&self) -> ClockStatus {
        let st = self.state.read().unwrap();
        ClockStatus {
            source: st.source,
            locked: st.locked,
            grandmaster: st.gm_identity.map(|g| format_clock_identity(&g)),
            domain: st.domain,
            jitter_ns: st.filter.rms,
            rate_ppm: -st.filter.fit.map(|f| f.rate).unwrap_or(0.0) * 1e6,
            samples: st.filter.len(),
            epoch: st.epoch,
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GM: [u8; 8] = [0x00, 0x60, 0x74, 0xff, 0xfe, 0x01, 0x02, 0x03];

    fn sample(t: i64, offset: i64) -> PtpSample {
        PtpSample {
            ingress_ns: t,
            master_offset_ns: offset,
            gm_present: true,
            gm_identity: GM,
            timescale_offset_s: 37,
            we_are_gm: false,
            domain: 0,
        }
    }

    /// A deterministic jitter source.
    fn noise(i: i64) -> i64 {
        ((i * 7919) % 41 - 20) * 1000
    }

    #[test]
    fn follows_offset_and_drift() {
        let clock = Clock::new();
        let t0 = 1_800_000_000_000_000_000i64;
        // The local clock runs 50 ppm fast and starts 3 ms ahead of the grandmaster.
        let offset = |t: i64| 3_000_000 + (t - t0) / 20_000;
        for i in 0..40 {
            let t = t0 + i * 250_000_000;
            clock.update(&sample(t, offset(t) + noise(i)));
        }
        let st = clock.status();
        assert!(st.locked);
        assert_eq!(st.grandmaster.as_deref(), Some("00-60-74-FF-FE-01-02-03"));
        assert!((st.rate_ppm + 50.0).abs() < 1.0, "rate {}", st.rate_ppm);
        // Prediction a second past the last sample.
        let t = t0 + 40 * 250_000_000 + 1_000_000_000;
        let want = t + 37_000_000_000 - offset(t);
        assert!(
            (clock.to_ptp(t) - want).abs() < 30_000,
            "error {}",
            clock.to_ptp(t) - want
        );
        let p = clock.to_ptp(t);
        assert!((clock.to_realtime(p) - t).abs() <= 1);
    }

    #[test]
    fn a_new_grandmaster_or_a_step_restarts_the_epoch() {
        let clock = Clock::new();
        let t0 = 1_000_000_000_000_000_000i64;
        for i in 0..20 {
            clock.update(&sample(t0 + i * 250_000_000, 1000));
        }
        let e = clock.epoch();
        assert!(clock.status().locked);
        // A single wild sample is ignored.
        clock.update(&sample(t0 + 20 * 250_000_000, 50_000_000));
        assert_eq!(clock.epoch(), e);
        clock.update(&sample(t0 + 21 * 250_000_000, 1000));
        // A jump that persists is a step.
        for i in 22..25 {
            clock.update(&sample(t0 + i * 250_000_000, 50_000_000));
        }
        assert_eq!(clock.epoch(), e + 1);
        assert!(!clock.status().locked);
        let mut other = sample(t0 + 30 * 250_000_000, 0);
        other.gm_identity = [9; 8];
        clock.update(&other);
        assert_eq!(clock.epoch(), e + 2);
    }

    #[test]
    fn without_ptp_it_is_the_host_clock_on_tai() {
        let clock = Clock::new();
        assert_eq!(clock.to_ptp(1_000), 1_000 + 37_000_000_000);
        assert_eq!(clock.status().source, ClockSource::System);
        let mut s = sample(5, 0);
        s.we_are_gm = true;
        clock.update(&s);
        assert_eq!(clock.status().source, ClockSource::Leader);
        assert!(clock.status().locked);
        let mut gone = sample(6, 0);
        gone.gm_present = false;
        clock.update(&gone);
        assert_eq!(clock.status().source, ClockSource::System);
    }
}
