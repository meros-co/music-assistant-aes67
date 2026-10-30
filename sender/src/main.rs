// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Meros Inc.

//! The add-on's program: reads the options, starts PTP, the outputs, Sendspin,
//! the control API and NMOS, and runs until Home Assistant stops it.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aes67_sender::api::Api;
use aes67_sender::clock::Clock;
use aes67_sender::config::AddonOptions;
use aes67_sender::engine::Engine;
use aes67_sender::log;
use aes67_sender::nmos::{Nmos, NmosConfig};
use aes67_sender::ptp::{self, PtpConfig};
use aes67_sender::sendspin::{Sendspin, SendspinConfig};
use aes67_sender::stream::NetConfig;

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn on_signals() {
    extern "C" fn handler(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn on_signals() {}

/// The interface to use: the named one, or the first non-loopback IPv4 one.
fn pick_interface(name: &str) -> Result<(String, Ipv4Addr), String> {
    let ifs = if_addrs::get_if_addrs().map_err(|e| format!("cannot list interfaces: {e}"))?;
    let v4 = ifs.into_iter().filter_map(|i| match i.addr {
        if_addrs::IfAddr::V4(a) if !a.ip.is_loopback() => Some((i.name, a.ip)),
        _ => None,
    });
    let mut v4: Vec<_> = v4.collect();
    if name.is_empty() {
        v4.sort_by_key(|(n, _)| {
            (
                n.starts_with("docker") || n.starts_with("veth") || n.starts_with("hassio"),
                n.clone(),
            )
        });
        v4.into_iter()
            .next()
            .ok_or_else(|| "no network interface has an IPv4 address".into())
    } else {
        v4.into_iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| format!("interface '{name}' has no IPv4 address"))
    }
}

fn mac_of(iface: &str) -> Option<String> {
    let mac = std::fs::read_to_string(format!("/sys/class/net/{iface}/address")).ok()?;
    let mac = mac.trim().replace(':', "-");
    (mac.len() == 17).then_some(mac)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "homeassistant".into())
}

fn run() -> Result<(), String> {
    let options_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/data/options.json".into());
    let data_dir = PathBuf::from(std::env::var("AES67_DATA").unwrap_or_else(|_| "/data".into()));
    let run_dir =
        PathBuf::from(std::env::var("AES67_RUN").unwrap_or_else(|_| "/run/aes67-sender".into()));
    let text =
        std::fs::read_to_string(&options_path).map_err(|e| format!("{options_path}: {e}"))?;
    let opts: AddonOptions =
        serde_json::from_str(&text).map_err(|e| format!("{options_path}: {e}"))?;
    log::set_debug(opts.debug);

    let (iface, ip) = pick_interface(&opts.interface)?;
    let mac = mac_of(&iface);
    let host = hostname();
    log::info(&format!(
        "AES67 sender {} on {iface} ({ip})",
        env!("CARGO_PKG_VERSION")
    ));

    let clock = Arc::new(Clock::new());
    let stop = Arc::new(AtomicBool::new(false));
    ptp::start(
        PtpConfig {
            role: opts.ptp_role,
            interface: iface.clone(),
            domain: opts.ptp_domain,
            dscp_event: opts.dscp_ptp,
            dscp_general: 0,
            run_dir,
            ptp4l: PathBuf::from(std::env::var("PTP4L").unwrap_or_else(|_| "ptp4l".into())),
        },
        clock.clone(),
        stop.clone(),
    )
    .map_err(|e| format!("PTP: {e}"))?;

    let engine = Engine::new(
        clock,
        NetConfig {
            iface_ip: ip,
            dscp_media: opts.dscp_media,
        },
    );
    engine.start_sap().map_err(|e| format!("SAP: {e}"))?;
    let outputs = opts.outputs()?;
    let mut fixed = HashSet::new();
    for (id, cfg) in &outputs {
        engine
            .upsert_output(id, cfg.clone())
            .map_err(|e| format!("output '{}': {e}", cfg.name))?;
        log::info(&format!(
            "output {id}: '{}' to {}:{}, channels {:?}",
            cfg.name, cfg.address, cfg.port, cfg.channel_map
        ));
        fixed.insert(id.clone());
    }

    let mdns = mdns_sd::ServiceDaemon::new().map_err(|e| format!("mDNS: {e}"))?;

    if opts.sendspin {
        let ss = Sendspin::new(
            SendspinConfig {
                port: opts.sendspin_port,
                data_dir: data_dir.join("sendspin"),
                hostname: host.clone(),
                ip,
                mac: mac.clone(),
                receiver_latency_us: opts.receiver_latency_ms as i64 * 1000,
            },
            engine.clone(),
            stop.clone(),
        );
        for (id, _) in &outputs {
            ss.add_output(id)
                .map_err(|e| format!("sendspin {id}: {e}"))?;
        }
        let listener = ss
            .bind()
            .map_err(|e| format!("sendspin port {}: {e}", opts.sendspin_port))?;
        ss.advertise(&mdns);
        std::thread::spawn(move || ss.serve(listener));
    }

    let api = Arc::new(Api {
        engine: engine.clone(),
        token: Some(opts.api_token.clone()).filter(|t| !t.is_empty()),
        fixed,
    });
    let api_ip: Ipv4Addr = opts
        .api_bind
        .parse()
        .map_err(|_| format!("api_bind '{}' is not an IPv4 address", opts.api_bind))?;
    let server = tiny_http::Server::http((api_ip, opts.api_port))
        .map_err(|e| format!("API port {}: {e}", opts.api_port))?;
    std::thread::spawn(move || api.serve(server));
    log::info(&format!("control API on {api_ip}:{}", opts.api_port));

    if opts.nmos {
        let seed_path = data_dir.join("nmos-seed");
        let seed = match std::fs::read_to_string(&seed_path) {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                let s = data_encoding::HEXLOWER.encode(&aes67_sender::sendspin::secure::random32());
                let _ = std::fs::create_dir_all(&data_dir);
                let _ = std::fs::write(&seed_path, &s);
                s
            }
        };
        let nmos = Nmos::new(
            engine.clone(),
            NmosConfig {
                label: format!("{host}-aes67"),
                hostname: host,
                port: opts.nmos_port,
                interface: iface,
                mac,
                seed,
            },
        );
        let server = tiny_http::Server::http((Ipv4Addr::UNSPECIFIED, opts.nmos_port))
            .map_err(|e| format!("NMOS port {}: {e}", opts.nmos_port))?;
        let n2 = nmos.clone();
        std::thread::spawn(move || n2.serve(server));
        let mdns2 = mdns.clone();
        std::thread::spawn(move || nmos.run_registration(mdns2));
    }

    on_signals();
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    log::info("stopping");
    // Sendspin connections say goodbye, ptp4l stops, SAP withdraws the streams.
    stop.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    engine.shutdown();
    std::thread::sleep(Duration::from_millis(1200));
    let _ = mdns.shutdown();
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        log::error(&e);
        std::process::exit(1);
    }
}
