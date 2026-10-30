// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! Runs ptp4l and feeds its measurements to the clock.
//!
//! ptp4l is configured to measure and never steer (`free_running 1`) with
//! software timestamps, so it needs no PHC and no CAP_SYS_TIME and leaves the
//! host clock to Home Assistant OS.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::clock::Clock;
use crate::log;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PtpRole {
    /// Follow the network's grandmaster; never become one.
    Follower,
    /// Follow, but become grandmaster when the network has none.
    LeaderIfNone,
    /// No PTP: time from the host clock (for networks without PTP, and tests).
    None,
}

#[derive(Clone, Debug)]
pub struct PtpConfig {
    pub role: PtpRole,
    pub interface: String,
    pub domain: u8,
    pub dscp_event: u8,
    pub dscp_general: u8,
    pub run_dir: PathBuf,
    pub ptp4l: PathBuf,
}

impl PtpConfig {
    pub fn socket(&self) -> PathBuf {
        self.run_dir.join("ptp4l")
    }

    /// ptp4l's configuration: the AES67 media profile's default intervals.
    pub fn ptp4l_conf(&self) -> String {
        let (client_only, priority1, clock_class) = match self.role {
            PtpRole::LeaderIfNone => (0, 250, 248),
            _ => (1, 255, 255),
        };
        format!(
            "[global]\n\
             domainNumber {domain}\n\
             clientOnly {client_only}\n\
             priority1 {priority1}\n\
             priority2 255\n\
             clockClass {clock_class}\n\
             free_running 1\n\
             time_stamping software\n\
             network_transport UDPv4\n\
             delay_mechanism E2E\n\
             logSyncInterval -3\n\
             logAnnounceInterval 1\n\
             logMinDelayReqInterval 0\n\
             announceReceiptTimeout 3\n\
             dscp_event {dscp_event}\n\
             dscp_general {dscp_general}\n\
             uds_address {socket}\n\
             uds_ro_address {socket}.ro\n\
             summary_interval 6\n\
             logging_level 5\n\
             use_syslog 0\n\
             verbose 1\n\
             message_tag ptp4l\n\
             \n\
             [{iface}]\n",
            domain = self.domain,
            dscp_event = self.dscp_event,
            dscp_general = self.dscp_general,
            socket = self.socket().display(),
            iface = self.interface,
        )
    }
}

/// Starts the PTP side: nothing for `PtpRole::None`, otherwise a thread that
/// keeps ptp4l running and reads it four times a second.
pub fn start(cfg: PtpConfig, clock: Arc<Clock>, stop: Arc<AtomicBool>) -> std::io::Result<()> {
    if cfg.role == PtpRole::None {
        return Ok(());
    }
    std::fs::create_dir_all(&cfg.run_dir)?;
    let conf_path = cfg.run_dir.join("ptp4l.conf");
    std::fs::write(&conf_path, cfg.ptp4l_conf())?;
    std::thread::Builder::new()
        .name("ptp".into())
        .spawn(move || supervise(cfg, conf_path, clock, stop))?;
    Ok(())
}

fn supervise(cfg: PtpConfig, conf: PathBuf, clock: Arc<Clock>, stop: Arc<AtomicBool>) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        log::info(&format!(
            "starting ptp4l on {} (domain {})",
            cfg.interface, cfg.domain
        ));
        let child = std::process::Command::new(&cfg.ptp4l)
            .arg("-f")
            .arg(&conf)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                log::error(&format!("cannot start {}: {e}", cfg.ptp4l.display()));
                clock.lose();
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        let started = std::time::Instant::now();
        poll(&cfg, &clock, &stop, || matches!(child.try_wait(), Ok(None)));
        let _ = child.kill();
        let _ = child.wait();
        clock.lose();
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        log::error("ptp4l exited; restarting");
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

#[cfg(unix)]
fn poll(cfg: &PtpConfig, clock: &Clock, stop: &AtomicBool, mut alive: impl FnMut() -> bool) {
    use crate::clock::PtpSample;
    use crate::pmc::{self, client::Pmc};

    let socket = cfg.socket();
    let mut pmc_conn: Option<Pmc> = None;
    let mut own_identity: Option<[u8; 8]> = None;
    let mut timescale = crate::clock::DEFAULT_UTC_OFFSET_S;
    let mut tick: u64 = 0;
    while alive() && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        tick += 1;
        if pmc_conn.is_none() {
            if !socket.exists() {
                continue;
            }
            match Pmc::connect(&socket, cfg.domain) {
                Ok(p) => pmc_conn = Some(p),
                Err(_) => continue,
            }
        }
        let p = pmc_conn.as_mut().unwrap();
        if own_identity.is_none() {
            own_identity = p
                .get(pmc::DEFAULT_DATA_SET)
                .ok()
                .and_then(|d| pmc::parse_clock_identity(&d));
        }
        if tick % 4 == 1 {
            if let Some(tp) = p
                .get(pmc::TIME_PROPERTIES_DATA_SET)
                .ok()
                .and_then(|d| pmc::parse_time_properties(&d))
            {
                timescale = tp.timescale_offset_s();
            }
        }
        match p
            .get(pmc::TIME_STATUS_NP)
            .map(|d| pmc::parse_time_status(&d))
        {
            Ok(Some(ts)) => {
                let we_are_gm = own_identity == Some(ts.gm_identity);
                clock.update(&PtpSample {
                    ingress_ns: ts.ingress_time_ns,
                    master_offset_ns: ts.master_offset_ns,
                    gm_present: ts.gm_present || we_are_gm,
                    gm_identity: ts.gm_identity,
                    timescale_offset_s: timescale,
                    we_are_gm,
                    domain: cfg.domain,
                });
            }
            _ => {
                pmc_conn = None;
                clock.lose();
            }
        }
    }
}

#[cfg(not(unix))]
fn poll(_cfg: &PtpConfig, clock: &Clock, stop: &AtomicBool, mut alive: impl FnMut() -> bool) {
    // ptp4l is Linux-only; elsewhere the process is watched and the clock stays on the host.
    while alive() && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
    }
    clock.lose();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follower_config_never_steers_and_never_leads() {
        let cfg = PtpConfig {
            role: PtpRole::Follower,
            interface: "eth0".into(),
            domain: 0,
            dscp_event: 46,
            dscp_general: 0,
            run_dir: "/run/aes67".into(),
            ptp4l: "ptp4l".into(),
        };
        let conf = cfg.ptp4l_conf();
        for line in [
            "free_running 1",
            "clientOnly 1",
            "time_stamping software",
            "dscp_event 46",
            "[eth0]",
        ] {
            assert!(conf.lines().any(|l| l == line), "missing {line}");
        }
        assert!(conf.contains("uds_address /run/aes67"));
        let leader = PtpConfig {
            role: PtpRole::LeaderIfNone,
            ..cfg
        }
        .ptp4l_conf();
        assert!(leader.lines().any(|l| l == "clientOnly 0"));
        assert!(leader.lines().any(|l| l == "priority1 250"));
    }
}
