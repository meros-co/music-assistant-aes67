// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! The Sendspin time filter: the spec's two-state (offset, drift) Kalman
//! filter, followed exactly, so this client converges like every other.

#[derive(Clone, Debug)]
pub struct TimeFilter {
    offset: f64,
    drift: f64,
    p_oo: f64,
    p_od: f64,
    p_dd: f64,
    last_update: i64,
    count: u32,
    use_drift: bool,
}

const PROCESS_STD_DEV: f64 = 0.0;
const DRIFT_PROCESS_STD_DEV: f64 = 1e-11;
const FORGET_FACTOR: f64 = 2.0;
const ADAPTIVE_CUTOFF: f64 = 3.0;
const MIN_SAMPLES: u32 = 100;
const DRIFT_SIGNIFICANCE: f64 = 2.0;
const MAX_ERROR_SCALE: f64 = 0.5;

impl Default for TimeFilter {
    fn default() -> Self {
        TimeFilter {
            offset: 0.0,
            drift: 0.0,
            p_oo: f64::INFINITY,
            p_od: 0.0,
            p_dd: 0.0,
            last_update: 0,
            count: 0,
            use_drift: false,
        }
    }
}

/// One exchange: T1 client sent, T2 server received, T3 server sent, T4 client received (µs).
pub fn measure(t1: i64, t2: i64, t3: i64, t4: i64) -> (i64, i64) {
    let measurement = ((t2 - t1) + (t3 - t4)) / 2;
    let max_error = ((t4 - t1) - (t3 - t2)) / 2;
    (measurement, max_error)
}

fn round_half_away(x: f64) -> i64 {
    x.round() as i64
}

impl TimeFilter {
    pub fn reset(&mut self) {
        *self = TimeFilter::default();
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Standard deviation of the offset estimate, µs.
    pub fn error_us(&self) -> f64 {
        self.p_oo.sqrt()
    }

    pub fn update(&mut self, measurement: i64, max_error: i64, time_added: i64) {
        if time_added <= self.last_update || max_error <= 0 {
            return;
        }
        let dt = (time_added - self.last_update) as f64;
        self.last_update = time_added;
        let m = measurement as f64;
        let r = (MAX_ERROR_SCALE * max_error as f64).powi(2);
        if self.count == 0 {
            self.offset = m;
            self.p_oo = r;
            self.drift = 0.0;
            self.count = 1;
            return;
        }
        if self.count == 1 {
            self.drift = (m - self.offset) / dt;
            self.p_dd = (self.p_oo + r) / (dt * dt);
            self.offset = m;
            self.p_oo = r;
            self.count = 2;
            return;
        }
        let offset_p = self.offset + self.drift * dt;
        let mut p_oo_p = self.p_oo + 2.0 * self.p_od * dt + self.p_dd * dt * dt + PROCESS_STD_DEV.powi(2) * dt;
        let mut p_od_p = self.p_od + self.p_dd * dt;
        let mut p_dd_p = self.p_dd + DRIFT_PROCESS_STD_DEV.powi(2) * dt;
        let y = m - offset_p;
        if self.count < MIN_SAMPLES {
            self.count += 1;
        } else if y.abs() > ADAPTIVE_CUTOFF * max_error as f64 {
            let f = FORGET_FACTOR * FORGET_FACTOR;
            p_oo_p *= f;
            p_od_p *= f;
            p_dd_p *= f;
        }
        let s = p_oo_p + r;
        let k_o = p_oo_p / s;
        let k_d = p_od_p / s;
        self.offset = offset_p + k_o * y;
        self.drift += k_d * y;
        self.p_oo = p_oo_p - k_o * p_oo_p;
        self.p_od = p_od_p - k_d * p_oo_p;
        self.p_dd = p_dd_p - k_d * p_od_p;
        self.use_drift = self.drift * self.drift > DRIFT_SIGNIFICANCE * DRIFT_SIGNIFICANCE * self.p_dd;
    }

    fn d(&self) -> f64 {
        if self.use_drift {
            self.drift
        } else {
            0.0
        }
    }

    pub fn server_time(&self, client: i64) -> i64 {
        client + round_half_away(self.offset + self.d() * (client - self.last_update) as f64)
    }

    pub fn client_time(&self, server: i64) -> i64 {
        let d = self.d();
        round_half_away((server as f64 - self.offset + d * self.last_update as f64) / (1.0 + d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_on_offset_and_drift() {
        let mut f = TimeFilter::default();
        // The server clock is 5 s ahead and runs 20 ppm fast; one-way delay 300 µs ± jitter.
        let server = |c: i64| 5_000_000 + c + c / 50_000;
        let mut c: i64 = 1_000_000;
        for i in 0..400 {
            let jitter = (i * 37 % 11) as i64 * 20;
            let t1 = c;
            let t2 = server(t1 + 300 + jitter);
            let t3 = t2 + 50;
            let t4 = t1 + 650 + jitter * 2;
            let (m, e) = measure(t1, t2, t3, t4);
            f.update(m, e, t4);
            c += 500_000;
        }
        let probe = c + 1_000_000;
        assert!((f.server_time(probe) - server(probe)).abs() < 100, "{}", f.server_time(probe) - server(probe));
        let back = f.client_time(f.server_time(probe));
        assert!((back - probe).abs() <= 1);
        assert!(f.error_us() < 1000.0);
    }

    #[test]
    fn ignores_bad_samples() {
        let mut f = TimeFilter::default();
        f.update(100, 10, 50);
        f.update(999, 0, 60);
        f.update(999, 10, 40);
        assert_eq!(f.count(), 1);
        assert_eq!(f.server_time(1000), 1100);
    }
}
